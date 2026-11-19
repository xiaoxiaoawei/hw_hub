//! Bounded JSON delivery to an explicitly configured endpoint. No redirects,
//! credential discovery, certificate bypasses, or automatic external requests.
use anyhow::{Result, ensure};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};
use url::Url;

const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostResponse {
    pub status: u16,
}

/// Deliberately contains no endpoint, response body, or configured header values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostError {
    pub message: String,
    pub retryable: bool,
}

impl fmt::Display for PostError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for PostError {}

type PostResult<T> = std::result::Result<T, PostError>;

fn failure(message: impl Into<String>, retryable: bool) -> PostError {
    PostError {
        message: message.into(),
        retryable,
    }
}

fn parse_endpoint(endpoint: &str) -> Result<Url> {
    ensure!(
        !endpoint.is_empty()
            && endpoint.len() <= 4096
            && !endpoint
                .chars()
                .any(|character| character.is_control() || character.is_whitespace()),
        "上报地址为空、过长或包含无效空白字符"
    );
    // Do not include url::ParseError context: some errors contain input text,
    // and an intentional webhook URL may carry a secret query parameter.
    let parsed =
        Url::parse(endpoint).map_err(|_| anyhow::anyhow!("上报地址不是有效的 HTTP/HTTPS URL"))?;
    ensure!(
        matches!(parsed.scheme(), "http" | "https"),
        "上报地址仅支持 HTTP 或 HTTPS"
    );
    ensure!(parsed.host().is_some(), "上报地址缺少服务器名称");
    ensure!(
        parsed.username().is_empty() && parsed.password().is_none(),
        "上报地址不可内嵌用户名或密码，请使用自定义请求头"
    );
    ensure!(parsed.fragment().is_none(), "上报地址不可包含片段标识");
    ensure!(
        parsed.port_or_known_default().is_some_and(|port| port != 0),
        "上报地址端口无效"
    );
    Ok(parsed)
}

pub fn validate_endpoint(endpoint: &str) -> Result<()> {
    parse_endpoint(endpoint).map(|_| ())
}

/// Authorization/X-API-Key are allowed. Transport-owned and proxy headers are
/// forbidden so saved configuration cannot override the target, framing or key.
pub fn validate_custom_headers(headers: &BTreeMap<String, String>) -> Result<()> {
    ensure!(headers.len() <= 32, "自定义请求头不能超过 32 项");
    let mut total = 0usize;
    let mut names = BTreeSet::new();
    for (name, value) in headers {
        ensure!(
            !name.is_empty()
                && name.len() <= 128
                && name.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
                }),
            "自定义请求头名称无效"
        );
        let lowercase = name.to_ascii_lowercase();
        ensure!(
            names.insert(lowercase.clone()),
            "自定义请求头名称重复（不区分大小写）"
        );
        ensure!(
            !lowercase.starts_with("proxy")
                && ![
                    "host",
                    "content-length",
                    "content-type",
                    "connection",
                    "transfer-encoding",
                    "user-agent",
                    "idempotency-key",
                    "expect",
                    "te",
                    "trailer",
                    "upgrade",
                    "keep-alive",
                    "accept-encoding",
                ]
                .contains(&lowercase.as_str()),
            "自定义请求头不可覆盖连接、代理、内容或幂等控制字段"
        );
        // Plain ASCII is portable across curl and WinHTTP's header encoding.
        // Unicode belongs in the UTF-8 JSON payload, not unencoded HTTP fields.
        ensure!(
            value.len() <= 8192
                && value
                    .bytes()
                    .all(|byte| byte == b'\t' || (0x20..=0x7e).contains(&byte)),
            "自定义请求头值过长或包含不允许的字符"
        );
        total = total
            .saturating_add(name.len())
            .saturating_add(value.len())
            .saturating_add(4);
        ensure!(total <= 16 * 1024, "自定义请求头总大小不能超过 16 KiB");
    }
    Ok(())
}

fn request_headers(headers: &BTreeMap<String, String>, record_id: &str) -> String {
    let mut result = format!("Content-Type: application/json\r\nIdempotency-Key: {record_id}\r\n");
    for (name, value) in headers {
        result.push_str(name);
        result.push_str(": ");
        result.push_str(value);
        result.push_str("\r\n");
    }
    result
}

/// Executes one attempt only. The durable record service controls whether an
/// upload is enabled and bounds retries while keeping the same record/key.
/// 3xx responses are returned without following Location, preventing forwarded
/// Authorization or query credentials from reaching another endpoint.
pub fn post_json(
    endpoint: &str,
    timeout_seconds: u64,
    custom_headers: &BTreeMap<String, String>,
    record_id: &str,
    body: &[u8],
) -> PostResult<PostResponse> {
    let endpoint = parse_endpoint(endpoint)
        .map_err(|_| failure("上报地址无效，请检查 HTTP/HTTPS 地址及端口", false))?;
    validate_custom_headers(custom_headers)
        .map_err(|_| failure("自定义请求头无效，请检查名称、长度及受保护字段", false))?;
    if !(5..=30).contains(&timeout_seconds) {
        return Err(failure("上报超时必须在 5 至 30 秒之间", false));
    }
    if record_id.is_empty()
        || record_id.len() > 128
        || !record_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
    {
        return Err(failure("上报记录的幂等标识无效", false));
    }
    if body.len() > MAX_REQUEST_BYTES || serde_json::from_slice::<serde_json::Value>(body).is_err()
    {
        return Err(failure("上报 JSON 无效或超过 1 MiB", false));
    }
    let headers = request_headers(custom_headers, record_id);
    platform_post(&endpoint, timeout_seconds, &headers, body)
}

#[cfg(not(windows))]
fn platform_post(
    endpoint: &Url,
    timeout_seconds: u64,
    headers: &str,
    body: &[u8],
) -> PostResult<PostResponse> {
    use std::{
        io::{Read, Write},
        process::{Command, Stdio},
    };

    let io_error = |_| failure("无法创建或读取上报请求的临时文件", false);
    let mut body_file = tempfile::NamedTempFile::new().map_err(io_error)?;
    body_file.write_all(body).map_err(io_error)?;
    let mut header_file = tempfile::NamedTempFile::new().map_err(io_error)?;
    header_file
        .write_all(headers.as_bytes())
        .map_err(io_error)?;
    // Keep even a secret-bearing query URL out of process arguments. Generated
    // config contains a single escaped URL; all policy flags are set below.
    let mut config_file = tempfile::NamedTempFile::new().map_err(io_error)?;
    let escaped_url = endpoint.as_str().replace('\\', "\\\\").replace('"', "\\\"");
    writeln!(config_file, "url = \"{escaped_url}\"").map_err(io_error)?;
    let mut child = Command::new("curl")
        .args(["-q", "--config"])
        .arg(config_file.path())
        .args([
            "--silent",
            "--show-error",
            "--no-insecure",
            "--proto",
            "=http,https",
            "--max-redirs",
            "0",
            "--request",
            "POST",
            "--connect-timeout",
        ])
        .arg(timeout_seconds.to_string())
        .arg("--max-time")
        .arg(timeout_seconds.to_string())
        .args([
            "--max-filesize",
            "65536",
            "--user-agent",
            concat!("FlashTool/", env!("CARGO_PKG_VERSION")),
            "--header",
        ])
        .arg(format!("@{}", header_file.path().display()))
        .arg("--data-binary")
        .arg(format!("@{}", body_file.path().display()))
        .args(["--write-out", "\n%{http_code}"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| failure("无法启动上报 HTTP 客户端", false))?;
    let mut output = Vec::new();
    let read = child
        .stdout
        .take()
        .ok_or_else(|| failure("无法读取上报 HTTP 响应", false))?
        .take((MAX_RESPONSE_BYTES + 5) as u64)
        .read_to_end(&mut output);
    if read.is_err() || output.len() > MAX_RESPONSE_BYTES + 4 {
        let _ = child.kill();
        let _ = child.wait();
        return Err(failure("上报响应读取失败或超过 64 KiB", false));
    }
    let exit = child
        .wait()
        .map_err(|_| failure("无法等待上报 HTTP 请求完成", true))?;
    if !exit.success() {
        return Err(match exit.code() {
            Some(28) => failure("上报请求超时", true),
            Some(60) | Some(51) | Some(58) | Some(77) => {
                failure("上报 HTTPS 证书验证失败，请检查系统时间及可信证书", false)
            }
            Some(63) => failure("上报响应超过 64 KiB", false),
            Some(5 | 6 | 7 | 18 | 52 | 55 | 56) => failure("上报网络连接失败或中断", true),
            _ => failure("上报 HTTP 请求失败", false),
        });
    }
    if output.len() < 4 || output[output.len() - 4] != b'\n' {
        return Err(failure("上报响应缺少 HTTP 状态", false));
    }
    let status = std::str::from_utf8(&output[output.len() - 3..])
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|value| (100..=599).contains(value))
        .ok_or_else(|| failure("上报响应的 HTTP 状态无效", false))?;
    Ok(PostResponse { status })
}

#[cfg(windows)]
fn platform_post(
    endpoint: &Url,
    timeout_seconds: u64,
    headers: &str,
    body: &[u8],
) -> PostResult<PostResponse> {
    windows_http::post(endpoint, timeout_seconds, headers, body)
}

#[cfg(windows)]
mod windows_http {
    use super::*;
    use std::{
        ffi::c_void,
        ptr::{null, null_mut},
        time::{Duration, Instant},
    };
    use windows_sys::Win32::Networking::WinHttp::*;

    struct Handle(*mut c_void);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                WinHttpCloseHandle(self.0);
            }
        }
    }

    fn os_error(operation: &str) -> PostError {
        let code = std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or_default() as u32;
        let retryable = matches!(
            code,
            ERROR_WINHTTP_TIMEOUT
                | ERROR_WINHTTP_NAME_NOT_RESOLVED
                | ERROR_WINHTTP_CANNOT_CONNECT
                | ERROR_WINHTTP_CONNECTION_ERROR
        );
        failure(format!("上报{operation}失败（WinHTTP {code}）"), retryable)
    }

    fn handle(raw: *mut c_void, operation: &str) -> PostResult<Handle> {
        if raw.is_null() {
            Err(os_error(operation))
        } else {
            Ok(Handle(raw))
        }
    }

    fn succeeded(value: i32, operation: &str) -> PostResult<()> {
        if value == 0 {
            Err(os_error(operation))
        } else {
            Ok(())
        }
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }

    fn remaining(deadline: Instant) -> PostResult<u32> {
        let duration = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| failure("上报请求超时", true))?;
        Ok(duration.as_millis().clamp(1, u32::MAX as u128) as u32)
    }

    fn receive_budget(request: &Handle, deadline: Instant) -> PostResult<()> {
        let milliseconds = remaining(deadline)?;
        unsafe {
            succeeded(
                WinHttpSetOption(
                    request.0,
                    WINHTTP_OPTION_RECEIVE_TIMEOUT,
                    (&milliseconds as *const u32).cast(),
                    std::mem::size_of::<u32>() as u32,
                ),
                "设置接收超时",
            )
        }
    }

    pub(super) fn post(
        endpoint: &Url,
        timeout_seconds: u64,
        headers: &str,
        body: &[u8],
    ) -> PostResult<PostResponse> {
        let deadline = Instant::now() + Duration::from_secs(timeout_seconds);
        let agent = wide(concat!("FlashTool/", env!("CARGO_PKG_VERSION")));
        let host = match endpoint
            .host()
            .ok_or_else(|| failure("上报服务器名称无效", false))?
        {
            url::Host::Domain(name) => name.to_owned(),
            url::Host::Ipv4(address) => address.to_string(),
            url::Host::Ipv6(address) => address.to_string(),
        };
        let host = wide(&host);
        let method = wide("POST");
        let mut path = endpoint.path().to_owned();
        if let Some(query) = endpoint.query() {
            path.push('?');
            path.push_str(query);
        }
        let path = wide(&path);
        let headers = wide(headers);
        let port = endpoint
            .port_or_known_default()
            .ok_or_else(|| failure("上报端口无效", false))?;
        unsafe {
            let session = handle(
                WinHttpOpen(
                    agent.as_ptr(),
                    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                    null(),
                    null(),
                    0,
                ),
                "初始化",
            )?;
            let milliseconds = remaining(deadline)? as i32;
            succeeded(
                WinHttpSetTimeouts(
                    session.0,
                    milliseconds,
                    milliseconds,
                    milliseconds,
                    milliseconds,
                ),
                "设置超时",
            )?;
            let connection = handle(
                WinHttpConnect(session.0, host.as_ptr(), port, 0),
                "连接服务器",
            )?;
            let request = handle(
                WinHttpOpenRequest(
                    connection.0,
                    method.as_ptr(),
                    path.as_ptr(),
                    null(),
                    null(),
                    null(),
                    if endpoint.scheme() == "https" {
                        WINHTTP_FLAG_SECURE
                    } else {
                        0
                    },
                ),
                "创建请求",
            )?;
            let policy = WINHTTP_OPTION_REDIRECT_POLICY_NEVER;
            succeeded(
                WinHttpSetOption(
                    request.0,
                    WINHTTP_OPTION_REDIRECT_POLICY,
                    (&policy as *const u32).cast(),
                    std::mem::size_of::<u32>() as u32,
                ),
                "禁止重定向",
            )?;
            succeeded(
                WinHttpSendRequest(
                    request.0,
                    headers.as_ptr(),
                    (headers.len() - 1) as u32,
                    body.as_ptr().cast(),
                    body.len() as u32,
                    body.len() as u32,
                    0,
                ),
                "发送请求",
            )?;
            receive_budget(&request, deadline)?;
            succeeded(WinHttpReceiveResponse(request.0, null_mut()), "接收响应")?;
            let mut status = 0u32;
            let mut length = std::mem::size_of::<u32>() as u32;
            succeeded(
                WinHttpQueryHeaders(
                    request.0,
                    WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                    null(),
                    (&mut status as *mut u32).cast(),
                    &mut length,
                    null_mut(),
                ),
                "读取状态",
            )?;
            let status = u16::try_from(status)
                .ok()
                .filter(|value| (100..=599).contains(value))
                .ok_or_else(|| failure("上报响应的 HTTP 状态无效", false))?;
            let mut received = 0usize;
            loop {
                receive_budget(&request, deadline)?;
                let mut available = 0u32;
                succeeded(
                    WinHttpQueryDataAvailable(request.0, &mut available),
                    "读取响应长度",
                )?;
                if available == 0 {
                    break;
                }
                if received.saturating_add(available as usize) > MAX_RESPONSE_BYTES {
                    return Err(failure("上报响应超过 64 KiB", false));
                }
                let mut buffer = [0u8; 8192];
                let count = available.min(buffer.len() as u32);
                let mut read = 0u32;
                succeeded(
                    WinHttpReadData(request.0, buffer.as_mut_ptr().cast(), count, &mut read),
                    "读取响应",
                )?;
                if read == 0 {
                    break;
                }
                if read > count {
                    return Err(failure("上报响应长度无效", false));
                }
                received += read as usize;
            }
            remaining(deadline)?;
            Ok(PostResponse { status })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_are_explicit_http_https_without_inline_credentials() {
        for endpoint in [
            "http://127.0.0.1:8080/records",
            "http://[::1]:8080/records",
            "http://factory-server/ingest",
            "https://example.invalid/ingest?key=not-logged",
        ] {
            assert!(validate_endpoint(endpoint).is_ok());
        }
        for endpoint in [
            "",
            "file:///tmp/records",
            "ftp://example.invalid",
            "https://user:secret@example.invalid",
            "https://example.invalid/#fragment",
            "http://localhost:0",
            "http://localhost\n/ingest",
            "https://",
        ] {
            assert!(
                validate_endpoint(endpoint).is_err(),
                "invalid endpoint should be rejected"
            );
        }
    }

    #[test]
    fn framing_proxy_and_injected_headers_are_rejected_without_echoing_values() {
        for name in [
            "Host",
            "Content-Length",
            "Content-Type",
            "Connection",
            "Transfer-Encoding",
            "Proxy-Authorization",
            "User-Agent",
            "Idempotency-Key",
            "Expect",
            "Bad Header",
        ] {
            let headers = BTreeMap::from([(name.into(), "never-echo-this-secret".into())]);
            let error = validate_custom_headers(&headers).unwrap_err().to_string();
            assert!(!error.contains("never-echo-this-secret"));
        }
        for value in ["ok\r\nX-Evil: injected", "nul\0value"] {
            assert!(
                validate_custom_headers(&BTreeMap::from([("X-API-Key".into(), value.into())]))
                    .is_err()
            );
        }
        let valid = BTreeMap::from([
            ("Authorization".into(), "Bearer test-only-token".into()),
            ("X-API-Key".into(), "test-only-key".into()),
        ]);
        assert!(validate_custom_headers(&valid).is_ok());
        assert!(
            validate_custom_headers(&BTreeMap::from([
                ("Authorization".into(), "a".into()),
                ("authorization".into(), "b".into())
            ]))
            .is_err()
        );
    }
}
