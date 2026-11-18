//! Read-only GitHub release discovery. Updates are never downloaded or executed.
use anyhow::{Context, Result, bail, ensure};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub const RELEASES_URL: &str = "https://github.com/xiaoxiaoawei/hw_hub/releases";
#[cfg(not(windows))]
const API_URL: &str = "https://api.github.com/repos/xiaoxiaoawei/hw_hub/releases/latest";
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_SETTINGS_BYTES: u64 = 64 * 1024;
const AUTO_CHECK_INTERVAL: u64 = 24 * 60 * 60;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseUpdate {
    pub version: String,
    pub release_url: String,
    pub download_url: Option<String>,
    pub notes: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckResult {
    UpToDate { latest_version: String },
    Available(ReleaseUpdate),
    NoPublishedRelease,
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    html_url: String,
    draft: bool,
    prerelease: bool,
    body: Option<String>,
    assets: Vec<GithubAsset>,
}

#[derive(Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

/// Checks the public repository using system TLS and proxy configuration.
/// Call this blocking operation from a background thread.
pub fn check_latest() -> Result<CheckResult> {
    let response = request_latest()?;
    interpret_response(env!("CARGO_PKG_VERSION"), response)
}

/// Pure release validation, including canonical repository links and SemVer.
/// `current` is a Cargo-style version without a leading `v`.
pub fn check_latest_from_json(current: &str, json: &str) -> Result<CheckResult> {
    ensure!(
        json.len() <= MAX_RESPONSE_BYTES,
        "更新响应超过 1 MiB，已拒绝处理"
    );
    let installed = Version::parse(current).context("当前程序版本不是有效的 SemVer")?;
    let release: GithubRelease = serde_json::from_str(json).context("GitHub 更新响应格式无效")?;
    ensure!(
        !release.draft && !release.prerelease,
        "更新响应不是正式发布版本"
    );
    let version_text = release
        .tag_name
        .strip_prefix('v')
        .context("发布标签必须采用 v主版本.次版本.修订版本 格式")?;
    let latest = Version::parse(version_text).context("发布标签不是有效的 SemVer")?;
    ensure!(
        latest.pre.is_empty() && latest.build.is_empty() && version_text == latest.to_string(),
        "发布标签必须采用严格的正式版本 v主版本.次版本.修订版本 格式"
    );
    let expected_release_url = format!("{RELEASES_URL}/tag/{}", release.tag_name);
    ensure!(
        release.html_url == expected_release_url,
        "更新响应包含非本仓库的发布链接，已拒绝处理"
    );

    let expected_name = format!("FlashTool-v{latest}-windows-x64.zip");
    let matching: Vec<_> = release
        .assets
        .iter()
        .filter(|asset| asset.name == expected_name)
        .collect();
    ensure!(
        matching.len() <= 1,
        "更新响应包含重复的 Windows 安装包，已拒绝处理"
    );
    let download_url = if let Some(asset) = matching.first() {
        let expected = format!(
            "{RELEASES_URL}/download/{}/{expected_name}",
            release.tag_name
        );
        ensure!(
            asset.browser_download_url == expected,
            "更新响应包含非本仓库的下载链接，已拒绝处理"
        );
        Some(expected)
    } else {
        None
    };
    if latest <= installed {
        return Ok(CheckResult::UpToDate {
            latest_version: latest.to_string(),
        });
    }
    Ok(CheckResult::Available(ReleaseUpdate {
        version: latest.to_string(),
        release_url: expected_release_url,
        download_url,
        notes: release.body.unwrap_or_default(),
    }))
}

struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

fn interpret_response(current: &str, response: HttpResponse) -> Result<CheckResult> {
    ensure!(
        response.body.len() <= MAX_RESPONSE_BYTES,
        "更新响应超过 1 MiB，已拒绝处理"
    );
    match response.status {
        200 => check_latest_from_json(
            current,
            std::str::from_utf8(&response.body).context("GitHub 更新响应不是 UTF-8")?,
        ),
        404 => Ok(CheckResult::NoPublishedRelease),
        403 => {
            bail!("GitHub 拒绝访问（HTTP 403），可能受到限流、访问权限或网络策略限制，请稍后重试")
        }
        429 => bail!("GitHub 请求过于频繁（HTTP 429），请稍后重试"),
        300..=399 => bail!("GitHub 更新服务返回重定向，已停止请求以保护固定更新来源"),
        status => bail!("GitHub 更新检查失败（HTTP {status}），请稍后重试"),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateSettings {
    pub auto_check: bool,
    /// Only a completed, successful HTTP/release check updates this timestamp.
    pub last_checked_unix: Option<u64>,
    /// Persist before an automatic attempt, including attempts that later fail.
    pub last_auto_attempt_unix: Option<u64>,
}

impl Default for UpdateSettings {
    fn default() -> Self {
        Self {
            auto_check: true,
            last_checked_unix: None,
            last_auto_attempt_unix: None,
        }
    }
}

pub fn should_auto_check(settings: &UpdateSettings, now: u64) -> bool {
    if !settings.auto_check {
        return false;
    }
    let last = settings
        .last_checked_unix
        .into_iter()
        .chain(settings.last_auto_attempt_unix)
        .max();
    last.is_none_or(|last| now.saturating_sub(last) >= AUTO_CHECK_INTERVAL)
}

pub fn load_settings() -> Result<UpdateSettings> {
    load_settings_from(&settings_path()?)
}

pub fn save_settings(settings: &UpdateSettings) -> Result<()> {
    save_settings_at(&settings_path()?, settings)
}

/// Explicit paths keep tests and callers away from another user's preferences.
pub fn load_settings_from(path: &Path) -> Result<UpdateSettings> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(UpdateSettings::default());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("无法读取更新设置：{}", path.display()));
        }
    };
    let metadata = file.metadata().context("无法检查更新设置文件")?;
    ensure!(
        metadata.is_file() && metadata.len() <= MAX_SETTINGS_BYTES,
        "更新设置文件无效或超过 64 KiB"
    );
    serde_json::from_reader(file.take(MAX_SETTINGS_BYTES + 1))
        .with_context(|| format!("更新设置格式无效：{}", path.display()))
}

pub fn save_settings_at(path: &Path, settings: &UpdateSettings) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("无法创建更新设置目录：{}", parent.display()))?;
    let mut file = tempfile::NamedTempFile::new_in(parent).context("无法创建更新设置临时文件")?;
    serde_json::to_writer_pretty(file.as_file_mut(), settings).context("无法编码更新设置")?;
    file.write_all(b"\n").context("无法写入更新设置")?;
    file.as_file().sync_all().context("无法保存更新设置")?;
    file.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("无法替换更新设置文件：{}", path.display()))?;
    Ok(())
}

fn settings_path() -> Result<PathBuf> {
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .context("Windows 未提供 LOCALAPPDATA，无法定位更新设置")?;
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME"))
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .context("无法定位当前用户的更新设置目录")?;
    Ok(base.join("FlashTool").join("update.json"))
}

#[cfg(not(windows))]
fn request_latest() -> Result<HttpResponse> {
    use std::process::{Command, Stdio};
    // Development-only fallback. -q preserves strict TLS defaults by ignoring
    // user curl config; proxy and trusted CA environment bindings still apply.
    let mut child = Command::new("curl")
        .args([
            "-q",
            "--silent",
            "--show-error",
            "--no-insecure",
            "--proto",
            "=https",
            "--connect-timeout",
            "10",
            "--max-time",
            "10",
            "--max-filesize",
            "1048576",
            "--user-agent",
            concat!("FlashTool/", env!("CARGO_PKG_VERSION")),
            "--header",
            "Accept: application/vnd.github+json",
            "--header",
            "X-GitHub-Api-Version: 2022-11-28",
            "--write-out",
            "\n%{http_code}",
            API_URL,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("无法运行开发环境的 curl；Windows 发行版使用系统 WinHTTP，不依赖 curl")?;
    let mut output = Vec::new();
    let read = child
        .stdout
        .take()
        .context("无法读取更新响应")?
        .take((MAX_RESPONSE_BYTES + 5) as u64)
        .read_to_end(&mut output);
    if read.is_err() || output.len() > MAX_RESPONSE_BYTES + 4 {
        let _ = child.kill();
        let _ = child.wait();
        read.context("读取更新响应失败")?;
        bail!("更新响应超过 1 MiB，已停止请求");
    }
    let status = child.wait().context("无法等待更新请求完成")?;
    if !status.success() {
        match status.code() {
            Some(28) => bail!("更新检查超时，请检查网络或代理设置后重试"),
            Some(60) => bail!("更新检查的 TLS 证书验证失败，请检查系统时间和受信任证书"),
            Some(63) => bail!("更新响应超过 1 MiB，已停止请求"),
            code => {
                bail!("更新检查网络请求失败（curl 退出码 {code:?}），请检查网络、代理或访问策略")
            }
        }
    }
    ensure!(
        output.len() >= 4 && output[output.len() - 4] == b'\n',
        "更新请求缺少 HTTP 状态"
    );
    let split = output.len() - 4;
    let status = std::str::from_utf8(&output[split + 1..])
        .context("HTTP 状态无效")?
        .parse()
        .context("HTTP 状态无效")?;
    output.truncate(split);
    Ok(HttpResponse {
        status,
        body: output,
    })
}

#[cfg(windows)]
fn request_latest() -> Result<HttpResponse> {
    windows_http::request()
}

#[cfg(windows)]
mod windows_http {
    use super::*;
    use std::{
        ffi::c_void,
        ptr::{null, null_mut},
    };
    use windows_sys::Win32::Networking::WinHttp::*;

    struct InternetHandle(*mut c_void);
    impl Drop for InternetHandle {
        fn drop(&mut self) {
            unsafe {
                WinHttpCloseHandle(self.0);
            }
        }
    }

    fn handle(raw: *mut c_void, operation: &str) -> Result<InternetHandle> {
        if raw.is_null() {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("更新检查：{operation}失败"));
        }
        Ok(InternetHandle(raw))
    }

    fn succeeded(value: i32, operation: &str) -> Result<()> {
        if value == 0 {
            return Err(std::io::Error::last_os_error()).with_context(|| {
                format!("更新检查：{operation}失败，请检查网络、代理、系统时间及证书")
            });
        }
        Ok(())
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }

    pub(super) fn request() -> Result<HttpResponse> {
        let agent = wide(concat!("FlashTool/", env!("CARGO_PKG_VERSION")));
        let host = wide("api.github.com");
        let method = wide("GET");
        let path = wide("/repos/xiaoxiaoawei/hw_hub/releases/latest");
        let headers =
            wide("Accept: application/vnd.github+json\r\nX-GitHub-Api-Version: 2022-11-28\r\n");
        // Keep WinHTTP's normal TLS certificate validation. Automatic proxy is
        // supplied by Windows; no credentials, TLS bypasses or custom VPNs.
        unsafe {
            let session = handle(
                WinHttpOpen(
                    agent.as_ptr(),
                    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                    null(),
                    null(),
                    0,
                ),
                "初始化 WinHTTP",
            )?;
            succeeded(
                WinHttpSetTimeouts(session.0, 10_000, 10_000, 10_000, 10_000),
                "设置网络超时",
            )?;
            let connection = handle(
                WinHttpConnect(session.0, host.as_ptr(), 443, 0),
                "连接 GitHub",
            )?;
            let request = handle(
                WinHttpOpenRequest(
                    connection.0,
                    method.as_ptr(),
                    path.as_ptr(),
                    null(),
                    null(),
                    null(),
                    WINHTTP_FLAG_SECURE,
                ),
                "创建 HTTPS 请求",
            )?;
            let policy = WINHTTP_OPTION_REDIRECT_POLICY_NEVER;
            succeeded(
                WinHttpSetOption(
                    request.0,
                    WINHTTP_OPTION_REDIRECT_POLICY,
                    (&policy as *const u32).cast(),
                    std::mem::size_of::<u32>() as u32,
                ),
                "禁用来源重定向",
            )?;
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
                "读取 HTTP 状态",
            )?;
            let mut body = Vec::new();
            loop {
                let mut available = 0u32;
                succeeded(
                    WinHttpQueryDataAvailable(request.0, &mut available),
                    "读取响应长度",
                )?;
                if available == 0 {
                    break;
                }
                ensure!(
                    body.len().saturating_add(available as usize) <= MAX_RESPONSE_BYTES,
                    "更新响应超过 1 MiB，已停止请求"
                );
                let count = (available as usize).min(8192);
                let mut buffer = vec![0u8; count];
                let mut read = 0u32;
                succeeded(
                    WinHttpReadData(
                        request.0,
                        buffer.as_mut_ptr().cast(),
                        count as u32,
                        &mut read,
                    ),
                    "读取响应内容",
                )?;
                if read == 0 {
                    break;
                }
                ensure!((read as usize) <= buffer.len(), "更新响应长度无效");
                body.extend_from_slice(&buffer[..read as usize]);
            }
            Ok(HttpResponse {
                status: status.try_into().context("HTTP 状态超出有效范围")?,
                body,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_found_is_distinct_from_network_and_rate_limit_errors() {
        assert_eq!(
            interpret_response(
                "0.3.0",
                HttpResponse {
                    status: 404,
                    body: br#"{"message":"Not Found"}"#.to_vec()
                }
            )
            .unwrap(),
            CheckResult::NoPublishedRelease
        );
        for status in [301, 403, 429, 500] {
            assert!(
                interpret_response(
                    "0.3.0",
                    HttpResponse {
                        status,
                        body: Vec::new()
                    }
                )
                .is_err()
            );
        }
    }
}
