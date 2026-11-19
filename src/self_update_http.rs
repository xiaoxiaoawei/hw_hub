//! Strictly bounded official GitHub asset GETs. No arbitrary mirrors, automatic
//! redirects, credential discovery, TLS bypasses, or executable invocation.
use anyhow::{Context, Result, bail, ensure};
use semver::Version;
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::Path,
    time::{Duration, Instant},
};
use url::Url;

const MAX_REDIRECTS: usize = 5;
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_LOCATION_BYTES: usize = 8192;
const MAX_DOWNLOAD_BYTES: u64 = 128 * 1024 * 1024;
const MAX_CHECKSUM_BYTES: u64 = 64 * 1024;
const TOTAL_TIMEOUT: Duration = Duration::from_secs(120);

fn initial_url(tag: &str, filename: &str) -> Result<Url> {
    let version = tag
        .strip_prefix('v')
        .and_then(|value| Version::parse(value).ok())
        .context("更新下载标签必须采用严格的正式版本 vMAJOR.MINOR.PATCH")?;
    ensure!(
        version.pre.is_empty() && version.build.is_empty() && tag == format!("v{version}"),
        "更新下载标签不是严格的正式版本"
    );
    ensure!(
        filename == "SHA256SUMS" || filename == format!("FlashTool-{tag}-windows-x64.zip"),
        "更新下载资源名称不受支持"
    );
    Url::parse(&format!(
        "https://github.com/xiaoxiaoawei/hw_hub/releases/download/{tag}/{filename}"
    ))
    .context("无法构造官方更新资源地址")
}

fn checked_url(initial: &Url, text: &str) -> Result<Url> {
    ensure!(
        text.len() <= MAX_LOCATION_BYTES
            && text.is_ascii()
            && !text
                .bytes()
                .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
            && !text.contains(['\\', '#']),
        "更新服务器返回了无效或过长的跳转地址"
    );
    let url = Url::parse(text).map_err(|_| anyhow::anyhow!("更新服务器跳转地址格式无效"))?;
    ensure!(
        url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
            && url.port_or_known_default() == Some(443),
        "更新服务器跳转不符合 HTTPS 来源限制"
    );
    // Inspect the unnormalized path as well: encoded separators/dot segments
    // are never necessary in GitHub's release-asset object paths.
    let authority_end = text
        .find("://")
        .map(|index| index + 3)
        .context("更新下载地址格式无效")?;
    ensure!(
        !text[authority_end..]
            .split(['/', '?'])
            .next()
            .unwrap_or_default()
            .contains('@'),
        "更新下载地址不可包含用户信息"
    );
    let raw_path = text[authority_end..]
        .find('/')
        .map(|index| &text[authority_end + index..])
        .unwrap_or("/");
    let raw_path = raw_path.split('?').next().unwrap_or(raw_path);
    ensure!(
        !raw_path.contains('%') && !raw_path.split('/').any(|part| matches!(part, "." | "..")),
        "更新服务器资源路径无效"
    );
    let allowed = match url.host_str() {
        Some("github.com") => url.path() == initial.path() && url.query().is_none(),
        Some("release-assets.githubusercontent.com") => {
            trusted_object_path(url.path(), "/github-production-release-asset/615688546/")
        }
        Some("objects.githubusercontent.com") => trusted_object_path(
            url.path(),
            "/github-production-release-asset-2e65be/615688546/",
        ),
        _ => false,
    };
    ensure!(allowed, "更新服务器跳转不属于本仓库的官方资源来源");
    Ok(url)
}

fn trusted_object_path(path: &str, prefix: &str) -> bool {
    path.strip_prefix(prefix).is_some_and(|suffix| {
        !suffix.is_empty()
            && suffix.split('/').all(|part| {
                !part.is_empty()
                    && !matches!(part, "." | "..")
                    && part
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
            })
    })
}

/// Pure redirect policy. Signed CDN query strings are accepted but never put in
/// diagnostics, logs, or process arguments.
pub(super) fn validate_asset_redirect(initial: &Url, current: &Url, location: &str) -> Result<Url> {
    let release_path = initial
        .path()
        .strip_prefix("/xiaoxiaoawei/hw_hub/releases/download/")
        .context("初始更新资源路径不属于本仓库")?;
    let (tag, filename) = release_path
        .split_once('/')
        .context("初始更新资源路径不完整")?;
    ensure!(
        initial_url(tag, filename)? == *initial,
        "初始更新资源地址不是官方标准链接"
    );
    checked_url(initial, current.as_str())?;
    ensure!(
        location.len() <= MAX_LOCATION_BYTES,
        "更新服务器跳转地址过长"
    );
    let target = if location.starts_with('/') && !location.starts_with("//") {
        format!(
            "https://{}{}",
            current.host_str().context("更新服务器缺少主机名")?,
            location
        )
    } else {
        location.to_owned()
    };
    checked_url(initial, &target)
}

fn remaining(deadline: Instant) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .context("官方更新下载已超过 120 秒，请稍后重试")
}

struct Transfer<'a> {
    file: &'a mut File,
    limit: u64,
    received: u64,
    declared: Option<u64>,
    last_notified: Instant,
    last_bytes: u64,
    progress: &'a mut dyn FnMut(u64, Option<u64>),
}

impl Transfer<'_> {
    fn start(&mut self, declared: Option<u64>) -> Result<()> {
        ensure!(
            declared.is_none_or(|length| length <= self.limit),
            "官方更新文件超过允许的大小"
        );
        self.declared = declared;
        (self.progress)(0, declared);
        Ok(())
    }

    fn chunk(&mut self, bytes: &[u8]) -> Result<()> {
        let received = self
            .received
            .checked_add(bytes.len() as u64)
            .context("更新文件大小无效")?;
        ensure!(
            received <= self.limit && self.declared.is_none_or(|length| received <= length),
            "官方更新文件超过大小限制或声明长度"
        );
        self.file
            .write_all(bytes)
            .context("无法写入更新下载暂存文件，请检查磁盘空间")?;
        self.received = received;
        if received.saturating_sub(self.last_bytes) >= 64 * 1024
            || self.last_notified.elapsed() >= Duration::from_millis(100)
        {
            (self.progress)(received, self.declared);
            self.last_notified = Instant::now();
            self.last_bytes = received;
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        ensure!(
            self.declared.is_none_or(|length| self.received == length),
            "官方更新下载不完整，实际大小与 Content-Length 不符"
        );
        ensure!(self.received > 0, "官方更新资源为空");
        self.file.sync_all().context("无法持久保存更新下载文件")?;
        (self.progress)(self.received, self.declared);
        Ok(())
    }
}

pub(super) fn download_official_asset(
    tag: &str,
    filename: &str,
    destination: &Path,
    max_bytes: u64,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<()> {
    let initial = initial_url(tag, filename)?;
    let maximum = if filename == "SHA256SUMS" {
        MAX_CHECKSUM_BYTES
    } else {
        MAX_DOWNLOAD_BYTES
    };
    ensure!(
        max_bytes > 0 && max_bytes <= maximum,
        "更新资源大小限制无效"
    );
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .context("无法创建独立的更新下载暂存文件")?;
    let result = {
        let mut transfer = Transfer {
            file: &mut file,
            limit: max_bytes,
            received: 0,
            declared: None,
            last_notified: Instant::now(),
            last_bytes: 0,
            progress,
        };
        platform_download(&initial, Instant::now() + TOTAL_TIMEOUT, &mut transfer)
            .and_then(|()| transfer.finish())
    };
    drop(file);
    if result.is_err() {
        // Only our create_new file can be removed; an existing destination was
        // rejected before network access. A partial file is never promoted.
        let _ = fs::remove_file(destination);
    }
    result
}

#[derive(Debug)]
struct ResponseHeaders {
    status: u16,
    location: Option<String>,
    length: Option<u64>,
}

fn parse_headers(text: &str) -> Result<ResponseHeaders> {
    ensure!(
        text.len() <= MAX_HEADER_BYTES && !text.contains('\0'),
        "官方更新响应头过长或无效"
    );
    let mut lines = text.lines();
    let status_line = lines.next().context("官方更新响应缺少 HTTP 状态")?;
    let mut words = status_line.split_ascii_whitespace();
    ensure!(
        words.next().is_some_and(|word| word.starts_with("HTTP/")),
        "官方更新响应状态行无效"
    );
    let status = words
        .next()
        .filter(|value| value.len() == 3)
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|value| (100..=599).contains(value))
        .context("官方更新 HTTP 状态无效")?;
    let mut result = ResponseHeaders {
        status,
        location: None,
        length: None,
    };
    for line in lines.filter(|line| !line.is_empty()) {
        ensure!(
            !line.starts_with([' ', '\t']),
            "官方更新响应包含不明确的折叠响应头"
        );
        let (name, value) = line.split_once(':').context("官方更新响应头格式无效")?;
        let value = value.trim();
        if name.eq_ignore_ascii_case("location") {
            ensure!(
                result.location.is_none() && value.len() <= MAX_LOCATION_BYTES,
                "官方更新 Location 重复或过长"
            );
            result.location = Some(value.to_owned());
        } else if name.eq_ignore_ascii_case("content-length") {
            ensure!(
                result.length.is_none()
                    && !value.is_empty()
                    && value.bytes().all(|byte| byte.is_ascii_digit()),
                "官方更新 Content-Length 重复或无效"
            );
            result.length = Some(value.parse().context("官方更新文件长度超出范围")?);
        } else if name.eq_ignore_ascii_case("content-encoding") {
            ensure!(
                value.eq_ignore_ascii_case("identity"),
                "官方更新资源使用了未支持的内容编码"
            );
        }
    }
    Ok(result)
}

#[cfg(not(windows))]
fn platform_download(initial: &Url, deadline: Instant, transfer: &mut Transfer<'_>) -> Result<()> {
    use std::{
        io::{BufRead, BufReader, Read},
        process::{Child, Command, Stdio},
    };
    struct ChildGuard(Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut current = checked_url(initial, initial.as_str())?;
    for redirects in 0..=MAX_REDIRECTS {
        let duration = remaining(deadline)?;
        let mut config = tempfile::NamedTempFile::new().context("无法创建私有更新请求配置")?;
        let escaped = current.as_str().replace('\\', "\\\\").replace('"', "\\\"");
        writeln!(config, "url = \"{escaped}\"").context("无法写入私有更新请求配置")?;
        let mut child = ChildGuard(
            Command::new("curl")
                .args(["-q", "--config"])
                .arg(config.path())
                .args([
                    "--silent",
                    "--show-error",
                    "--no-insecure",
                    "--proto",
                    "=https",
                    "--max-redirs",
                    "0",
                    "--request",
                    "GET",
                    "--include",
                    "--suppress-connect-headers",
                    "--connect-timeout",
                ])
                .arg(format!("{:.3}", duration.as_secs_f64().min(10.0)))
                .arg("--max-time")
                .arg(format!("{:.3}", duration.as_secs_f64()))
                .arg("--max-filesize")
                .arg(transfer.limit.to_string())
                .args([
                    "--header",
                    "Accept: application/octet-stream",
                    "--header",
                    "Accept-Encoding: identity",
                    "--header",
                    "Cache-Control: no-cache",
                    "--user-agent",
                    concat!("FlashTool-Update/", env!("CARGO_PKG_VERSION")),
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .context("无法启动开发环境的安全更新下载客户端")?,
        );
        let output = child.0.stdout.take().context("无法读取官方更新下载响应")?;
        let mut reader = BufReader::new(output);
        let mut header_bytes = 0usize;
        let headers = loop {
            let mut block = Vec::new();
            loop {
                let mut line = Vec::new();
                let count = (&mut reader)
                    .take((MAX_HEADER_BYTES.saturating_sub(header_bytes) + 1) as u64)
                    .read_until(b'\n', &mut line)
                    .context("读取官方更新响应头失败")?;
                ensure!(count != 0, "官方更新响应头不完整");
                header_bytes = header_bytes.saturating_add(count);
                ensure!(
                    header_bytes <= MAX_HEADER_BYTES,
                    "官方更新响应头超过 64 KiB"
                );
                block.extend_from_slice(&line);
                if line == b"\r\n" || line == b"\n" {
                    break;
                }
            }
            let headers =
                parse_headers(std::str::from_utf8(&block).context("官方更新响应头编码无效")?)?;
            if (100..=199).contains(&headers.status) {
                continue;
            }
            break headers;
        };
        if matches!(headers.status, 301 | 302 | 303 | 307 | 308) {
            ensure!(redirects < MAX_REDIRECTS, "官方更新资源跳转超过 5 次");
            current = validate_asset_redirect(
                initial,
                &current,
                headers
                    .location
                    .as_deref()
                    .context("官方更新跳转缺少 Location")?,
            )?;
            continue;
        }
        ensure!(
            headers.status == 200,
            "官方更新资源下载失败（HTTP {}）",
            headers.status
        );
        transfer.start(headers.length)?;
        let mut buffer = [0u8; 32 * 1024];
        loop {
            remaining(deadline)?;
            let count = reader
                .read(&mut buffer)
                .context("读取官方更新下载内容失败")?;
            if count == 0 {
                break;
            }
            transfer.chunk(&buffer[..count])?;
        }
        let status = child.0.wait().context("无法等待官方更新下载结束")?;
        if !status.success() {
            match status.code() {
                Some(28) => bail!("官方更新下载超时，请稍后重试"),
                Some(60 | 51 | 58 | 77) => {
                    bail!("官方更新 HTTPS 证书验证失败，请检查系统时间及可信证书")
                }
                Some(63) => bail!("官方更新资源超过下载大小限制"),
                _ => bail!("官方更新下载连接中断或请求失败"),
            }
        }
        remaining(deadline)?;
        return Ok(());
    }
    bail!("官方更新资源跳转超过允许次数")
}

#[cfg(windows)]
fn platform_download(initial: &Url, deadline: Instant, transfer: &mut Transfer<'_>) -> Result<()> {
    windows_http::download(initial, deadline, transfer)
}

#[cfg(windows)]
mod windows_http {
    use super::*;
    use std::{
        ffi::c_void,
        ptr::{null, null_mut},
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
    fn network_error(operation: &str) -> anyhow::Error {
        let code = std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or_default();
        anyhow::anyhow!("官方更新{operation}失败（WinHTTP {code}），请检查网络、系统时间与可信证书")
    }
    fn handle(raw: *mut c_void, operation: &str) -> Result<Handle> {
        if raw.is_null() {
            Err(network_error(operation))
        } else {
            Ok(Handle(raw))
        }
    }
    fn succeeded(value: i32, operation: &str) -> Result<()> {
        if value == 0 {
            Err(network_error(operation))
        } else {
            Ok(())
        }
    }
    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }
    fn timeouts(handle: &Handle, deadline: Instant) -> Result<()> {
        let milliseconds = remaining(deadline)?.as_millis().clamp(1, 15_000) as i32;
        unsafe {
            succeeded(
                WinHttpSetTimeouts(
                    handle.0,
                    milliseconds,
                    milliseconds,
                    milliseconds,
                    milliseconds,
                ),
                "设置超时",
            )
        }
    }
    fn raw_headers(request: &Handle) -> Result<ResponseHeaders> {
        unsafe {
            let mut length = 0u32;
            let result = WinHttpQueryHeaders(
                request.0,
                WINHTTP_QUERY_RAW_HEADERS_CRLF,
                null(),
                null_mut(),
                &mut length,
                null_mut(),
            );
            if result == 0 && std::io::Error::last_os_error().raw_os_error() != Some(122) {
                return Err(network_error("读取响应头长度"));
            }
            ensure!(
                length >= 2
                    && length.is_multiple_of(2)
                    && length as usize <= (MAX_HEADER_BYTES + 1) * 2,
                "官方更新响应头超过 64 KiB 或编码无效"
            );
            let mut buffer = vec![0u16; length as usize / 2];
            succeeded(
                WinHttpQueryHeaders(
                    request.0,
                    WINHTTP_QUERY_RAW_HEADERS_CRLF,
                    null(),
                    buffer.as_mut_ptr().cast(),
                    &mut length,
                    null_mut(),
                ),
                "读取响应头",
            )?;
            ensure!(
                length.is_multiple_of(2) && length as usize / 2 <= buffer.len(),
                "官方更新响应头长度无效"
            );
            buffer.truncate(length as usize / 2);
            ensure!(!buffer.contains(&0), "官方更新响应头包含无效字符");
            parse_headers(&String::from_utf16(&buffer).context("官方更新响应头编码无效")?)
        }
    }
    pub(super) fn download(
        initial: &Url,
        deadline: Instant,
        transfer: &mut Transfer<'_>,
    ) -> Result<()> {
        let agent = wide(concat!("FlashTool-Update/", env!("CARGO_PKG_VERSION")));
        let session = handle(
            unsafe {
                WinHttpOpen(
                    agent.as_ptr(),
                    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                    null(),
                    null(),
                    0,
                )
            },
            "初始化",
        )?;
        let mut current = checked_url(initial, initial.as_str())?;
        for redirects in 0..=MAX_REDIRECTS {
            timeouts(&session, deadline)?;
            let host = wide(current.host_str().context("官方更新地址缺少主机名")?);
            let mut request_path = current.path().to_owned();
            if let Some(query) = current.query() {
                request_path.push('?');
                request_path.push_str(query);
            }
            let path = wide(&request_path);
            let connection = handle(
                unsafe { WinHttpConnect(session.0, host.as_ptr(), 443, 0) },
                "连接服务器",
            )?;
            let request = handle(
                unsafe {
                    WinHttpOpenRequest(
                        connection.0,
                        wide("GET").as_ptr(),
                        path.as_ptr(),
                        null(),
                        null(),
                        null(),
                        WINHTTP_FLAG_SECURE,
                    )
                },
                "创建请求",
            )?;
            let policy = WINHTTP_OPTION_REDIRECT_POLICY_NEVER;
            let disable = WINHTTP_DISABLE_COOKIES;
            unsafe {
                succeeded(
                    WinHttpSetOption(
                        request.0,
                        WINHTTP_OPTION_REDIRECT_POLICY,
                        (&policy as *const u32).cast(),
                        std::mem::size_of::<u32>() as u32,
                    ),
                    "禁用自动跳转",
                )?;
                succeeded(
                    WinHttpSetOption(
                        request.0,
                        WINHTTP_OPTION_DISABLE_FEATURE,
                        (&disable as *const u32).cast(),
                        std::mem::size_of::<u32>() as u32,
                    ),
                    "禁用 Cookie 缓存",
                )?;
                timeouts(&request, deadline)?;
                let headers = wide(
                    "Accept: application/octet-stream\r\nAccept-Encoding: identity\r\nCache-Control: no-cache\r\n",
                );
                succeeded(
                    WinHttpSendRequest(
                        request.0,
                        headers.as_ptr(),
                        (headers.len() - 1) as u32,
                        null(),
                        0,
                        0,
                        0,
                    ),
                    "发送请求",
                )?;
                timeouts(&request, deadline)?;
                succeeded(WinHttpReceiveResponse(request.0, null_mut()), "接收响应")?;
            }
            let headers = raw_headers(&request)?;
            if matches!(headers.status, 301 | 302 | 303 | 307 | 308) {
                ensure!(redirects < MAX_REDIRECTS, "官方更新资源跳转超过 5 次");
                current = validate_asset_redirect(
                    initial,
                    &current,
                    headers
                        .location
                        .as_deref()
                        .context("官方更新跳转缺少 Location")?,
                )?;
                continue;
            }
            ensure!(
                headers.status == 200,
                "官方更新资源下载失败（HTTP {}）",
                headers.status
            );
            transfer.start(headers.length)?;
            let mut buffer = [0u8; 32 * 1024];
            loop {
                timeouts(&request, deadline)?;
                let mut available = 0u32;
                unsafe {
                    succeeded(
                        WinHttpQueryDataAvailable(request.0, &mut available),
                        "读取响应长度",
                    )?;
                }
                if available == 0 {
                    break;
                }
                timeouts(&request, deadline)?;
                let count = available.min(buffer.len() as u32);
                let mut read = 0u32;
                unsafe {
                    succeeded(
                        WinHttpReadData(request.0, buffer.as_mut_ptr().cast(), count, &mut read),
                        "读取资源内容",
                    )?;
                }
                ensure!(read <= count, "官方更新响应的读取长度无效");
                if read == 0 {
                    break;
                }
                transfer.chunk(&buffer[..read as usize])?;
            }
            remaining(deadline)?;
            return Ok(());
        }
        bail!("官方更新资源跳转超过允许次数")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_exact_assets_of_strict_stable_tags_can_be_requested() {
        assert!(initial_url("v0.5.1", "SHA256SUMS").is_ok());
        assert!(initial_url("v0.5.1", "FlashTool-v0.5.1-windows-x64.zip").is_ok());
        for tag in [
            "0.5.1",
            "v0.5",
            "v00.5.1",
            "v0.5.1-beta",
            "v0.5.1+build",
            "v0.5.1/../x",
        ] {
            assert!(initial_url(tag, "SHA256SUMS").is_err());
        }
        for name in [
            "FlashTool.exe",
            "../SHA256SUMS",
            "FlashTool-v0.5.2-windows-x64.zip",
            "SHA256SUMS?token=x",
        ] {
            assert!(initial_url("v0.5.1", name).is_err());
        }
    }
    #[test]
    fn redirects_are_https_and_exact_repository_scoped_without_secret_diagnostics() {
        let initial = initial_url("v0.5.1", "SHA256SUMS").unwrap();
        for allowed in [
            initial.as_str(),
            "/xiaoxiaoawei/hw_hub/releases/download/v0.5.1/SHA256SUMS",
            "https://release-assets.githubusercontent.com/github-production-release-asset/615688546/object-uuid?sig=test-only-signature&sp=r",
            "https://objects.githubusercontent.com/github-production-release-asset-2e65be/615688546/object-uuid?token=test-only-signature",
        ] {
            assert!(validate_asset_redirect(&initial, &initial, allowed).is_ok());
        }
        for denied in [
            "http://github.com/xiaoxiaoawei/hw_hub/releases/download/v0.5.1/SHA256SUMS",
            "https://github.com/other/repo/releases/download/v0.5.1/SHA256SUMS",
            "https://github.com/xiaoxiaoawei/hw_hub/releases/download/v0.5.2/SHA256SUMS",
            "https://github.com/xiaoxiaoawei/hw_hub/releases/download/v0.5.1/SHA256SUMS?token=test-only-signature",
            "https://github.com@evil.invalid/xiaoxiaoawei/hw_hub/releases/download/v0.5.1/SHA256SUMS",
            "https://@release-assets.githubusercontent.com/github-production-release-asset/615688546/object?sig=test-only-signature",
            "https://github.com.evil.invalid/xiaoxiaoawei/hw_hub/releases/download/v0.5.1/SHA256SUMS",
            "https://release-assets.githubusercontent.com:444/github-production-release-asset/615688546/object?sig=test-only-signature",
            "https://release-assets.githubusercontent.com/github-production-release-asset/123/object?sig=test-only-signature",
            "https://release-assets.githubusercontent.com/github-production-release-asset/615688546evil/object?sig=test-only-signature",
            "https://objects.githubusercontent.com/github-production-release-asset/615688546/object?sig=test-only-signature",
            "https://release-assets.githubusercontent.com/github-production-release-asset/615688546/../123/object?sig=test-only-signature",
            "https://release-assets.githubusercontent.com/github-production-release-asset/615688546/%2e%2e/object?sig=test-only-signature",
            "https://release-assets.githubusercontent.com/github-production-release-asset/615688546/object#test-only-signature",
            "//evil.invalid/object?sig=test-only-signature",
            "https://github-cloud.s3.amazonaws.com/releases/object?sig=test-only-signature",
        ] {
            let message = format!(
                "{:#}",
                validate_asset_redirect(&initial, &initial, denied).unwrap_err()
            );
            assert!(!message.contains("test-only-signature"));
        }
    }
    #[test]
    fn response_headers_are_bounded_and_unambiguous() {
        assert_eq!(
            parse_headers("HTTP/2 200\r\nContent-Length: 123\r\n\r\n")
                .unwrap()
                .length,
            Some(123)
        );
        for headers in [
            "HTTP/2 200\r\nContent-Length: 123\r\nContent-Length: 123\r\n\r\n",
            "HTTP/2 200\r\nContent-Length: -1\r\n\r\n",
            "HTTP/2 302\r\nLocation: one\r\nLocation: two\r\n\r\n",
            "HTTP/2 200\r\nContent-Encoding: gzip\r\n\r\n",
            "HTTP/2 200\r\n Folded: value\r\n\r\n",
        ] {
            assert!(parse_headers(headers).is_err());
        }
        assert!(
            parse_headers(&format!(
                "HTTP/2 200\r\nX-Header: {}\r\n\r\n",
                "x".repeat(MAX_HEADER_BYTES)
            ))
            .is_err()
        );
    }
    #[test]
    fn transfer_does_not_write_past_limit_and_requires_declared_length() {
        let directory = tempfile::tempdir().unwrap();
        let mut file = File::create(directory.path().join("download.part")).unwrap();
        let mut progress = |_, _| {};
        let mut transfer = Transfer {
            file: &mut file,
            limit: 4,
            received: 0,
            declared: None,
            last_notified: Instant::now(),
            last_bytes: 0,
            progress: &mut progress,
        };
        transfer.start(Some(4)).unwrap();
        transfer.chunk(b"ab").unwrap();
        assert!(transfer.finish().is_err());
        assert!(transfer.chunk(b"cde").is_err());
        assert_eq!(transfer.received, 2);
        transfer.chunk(b"cd").unwrap();
        transfer.finish().unwrap();
        assert_eq!(file.metadata().unwrap().len(), 4);
    }
}
