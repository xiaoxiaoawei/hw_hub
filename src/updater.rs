//! Read-only GitHub release discovery. Confirmed downloads and installation are
//! handled separately by the self-update service.
use anyhow::{Context, Result, bail, ensure};
use chrono::{Local, TimeZone};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub const RELEASES_URL: &str = "https://github.com/xiaoxiaoawei/hw_hub/releases";
const RELEASES_PATH: &str = "/xiaoxiaoawei/hw_hub/releases";
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
#[cfg(not(windows))]
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_LOCATION_BYTES: usize = 8192;
const MAX_SETTINGS_BYTES: u64 = 64 * 1024;
const MAX_RELEASE_NOTES_BYTES: usize = 64 * 1024;

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

/// The transport can only request these fixed official GitHub endpoints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateEndpoint {
    ApiLatest,
    WebsiteLatest,
    ExpandedAssets { tag: String },
    ReleaseNotes { tag: String },
}

impl UpdateEndpoint {
    fn destination(&self) -> Result<(&'static str, String)> {
        Ok(match self {
            Self::ApiLatest => (
                "api.github.com",
                "/repos/xiaoxiaoawei/hw_hub/releases/latest".into(),
            ),
            Self::WebsiteLatest => ("github.com", format!("{RELEASES_PATH}/latest")),
            Self::ExpandedAssets { tag } => {
                stable_tag_version(tag)?;
                (
                    "github.com",
                    format!("{RELEASES_PATH}/expanded_assets/{tag}"),
                )
            }
            Self::ReleaseNotes { tag } => {
                stable_tag_version(tag)?;
                (
                    "raw.githubusercontent.com",
                    format!("/xiaoxiaoawei/hw_hub/refs/tags/{tag}/CHANGELOG.md"),
                )
            }
        })
    }

    fn accept_headers(&self) -> &'static str {
        match self {
            Self::ApiLatest => {
                "Accept: application/vnd.github+json\r\nX-GitHub-Api-Version: 2022-11-28\r\n"
            }
            Self::ReleaseNotes { .. } => "Accept: text/plain\r\n",
            _ => "Accept: text/html\r\n",
        }
    }
}

/// A bounded response with redirects disabled. This seam permits offline tests
/// without network access or access to the user's saved update preferences.
#[derive(Clone, Debug)]
pub struct UpdateHttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub location: Option<String>,
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
    check_latest_with_transport(env!("CARGO_PKG_VERSION"), |endpoint| {
        request_http(&endpoint)
    })
}

/// Prefer the API, falling back to the official website only for transient
/// transport/server/rate-limit failures or to confirm an API 404. Invalid API
/// release data never bypasses validation through the alternate route.
pub fn check_latest_with_transport(
    current: &str,
    mut request: impl FnMut(UpdateEndpoint) -> Result<UpdateHttpResponse>,
) -> Result<CheckResult> {
    Version::parse(current).context("当前程序版本不是有效的 SemVer")?;
    let (api_error, api_not_found) = match request(UpdateEndpoint::ApiLatest) {
        Ok(response) => {
            ensure!(
                response.body.len() <= MAX_RESPONSE_BYTES,
                "更新响应超过 1 MiB，已拒绝处理"
            );
            match response.status {
                200 => return interpret_response(current, response),
                404 => (
                    anyhow::anyhow!("GitHub API 返回 HTTP 404，尚未确认是否存在正式版本"),
                    true,
                ),
                403 | 429 | 500..=599 => (
                    interpret_response(current, response)
                        .expect_err("These HTTP statuses are errors"),
                    false,
                ),
                _ => return interpret_response(current, response),
            }
        }
        Err(error) => (error.context("GitHub API 网络请求失败"), false),
    };
    check_official_website(current, api_not_found, &mut request)
        .with_context(|| format!("GitHub API 检查失败：{api_error:#}；GitHub 官网备用检查也失败"))
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
    let latest = stable_tag_version(&release.tag_name)?;
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

fn stable_tag_version(tag: &str) -> Result<Version> {
    let text = tag
        .strip_prefix('v')
        .context("发布标签必须采用 v主版本.次版本.修订版本 格式")?;
    let version = Version::parse(text).context("发布标签不是有效的 SemVer")?;
    ensure!(
        version.pre.is_empty() && version.build.is_empty() && text == version.to_string(),
        "发布标签必须采用严格的正式版本 v主版本.次版本.修订版本 格式"
    );
    Ok(version)
}

fn interpret_response(current: &str, response: UpdateHttpResponse) -> Result<CheckResult> {
    ensure!(
        response.body.len() <= MAX_RESPONSE_BYTES,
        "更新响应超过 1 MiB，已拒绝处理"
    );
    match response.status {
        200 => check_latest_from_json(
            current,
            std::str::from_utf8(&response.body).context("GitHub 更新响应不是 UTF-8")?,
        ),
        404 => bail!("GitHub API 返回 HTTP 404，无法确认正式版本"),
        403 => {
            bail!("GitHub 拒绝访问（HTTP 403），可能受到限流、访问权限或网络策略限制，请稍后重试")
        }
        429 => bail!("GitHub 请求过于频繁（HTTP 429），请稍后重试"),
        300..=399 => bail!("GitHub 更新服务返回重定向，已停止请求以保护固定更新来源"),
        status => bail!("GitHub 更新检查失败（HTTP {status}），请稍后重试"),
    }
}

fn check_official_website(
    current: &str,
    api_not_found: bool,
    request: &mut impl FnMut(UpdateEndpoint) -> Result<UpdateHttpResponse>,
) -> Result<CheckResult> {
    let latest =
        request(UpdateEndpoint::WebsiteLatest).context("连接 GitHub 官网最新版本入口失败")?;
    ensure!(
        latest.body.len() <= MAX_RESPONSE_BYTES,
        "官网更新响应超过 1 MiB"
    );
    if latest.status == 404 && api_not_found {
        return Ok(CheckResult::NoPublishedRelease);
    }
    ensure!(
        matches!(latest.status, 301 | 302 | 303 | 307 | 308),
        "GitHub 官网最新版本入口未返回可验证的发布重定向（HTTP {}）",
        latest.status
    );
    let location = latest
        .location
        .context("GitHub 官网最新版本入口缺少 Location")?;
    ensure!(location.len() <= MAX_LOCATION_BYTES, "官网发布重定向过长");
    let tag = location
        .strip_prefix(&format!("{RELEASES_URL}/tag/"))
        .or_else(|| location.strip_prefix(&format!("{RELEASES_PATH}/tag/")))
        .context("官网最新版本重定向不属于本仓库的正式发布页，已拒绝处理")?;
    let version = stable_tag_version(tag)?;
    // GitHub's public /releases/latest redirect is authoritative for a latest
    // non-draft, non-prerelease release. Bare tags/Atom entries are not used.
    let assets = request(UpdateEndpoint::ExpandedAssets {
        tag: tag.to_owned(),
    })
    .context("读取 GitHub 官网正式版本的资源列表失败")?;
    ensure!(
        assets.status == 200,
        "GitHub 官网资源列表检查失败（HTTP {}）",
        assets.status
    );
    ensure!(
        assets.body.len() <= MAX_RESPONSE_BYTES,
        "官网资源列表超过 1 MiB"
    );
    let html = std::str::from_utf8(&assets.body).context("官网资源列表不是 UTF-8")?;
    let name = format!("FlashTool-{tag}-windows-x64.zip");
    let relative = format!("{RELEASES_PATH}/download/{tag}/{name}");
    let canonical = format!("https://github.com{relative}");
    ensure!(
        canonical_asset_count(html, &relative, &canonical)? == 1,
        "官网资源列表未包含唯一且属于本仓库的 Windows 安装包，无法确认更新"
    );
    let installed = Version::parse(current).context("当前程序版本不是有效的 SemVer")?;
    if version <= installed {
        return Ok(CheckResult::UpToDate {
            latest_version: version.to_string(),
        });
    }
    // Notes are optional metadata, never another discovery/update channel.
    // Fetch only this exact immutable official tag after release+asset proof;
    // a blocked raw host or missing section must not hide a verified update.
    let notes = match request(UpdateEndpoint::ReleaseNotes {
        tag: tag.to_owned(),
    }) {
        Ok(response)
            if response.status == 200
                && response.location.is_none()
                && response.body.len() <= MAX_RESPONSE_BYTES =>
        {
            std::str::from_utf8(&response.body)
                .ok()
                .and_then(|text| changelog_release_notes(&version.to_string(), text))
                .unwrap_or_default()
        }
        _ => String::new(),
    };
    Ok(CheckResult::Available(ReleaseUpdate {
        version: version.to_string(),
        release_url: format!("{RELEASES_URL}/tag/{tag}"),
        download_url: Some(canonical),
        notes,
    }))
}

fn changelog_release_notes(version: &str, text: &str) -> Option<String> {
    let wanted = format!("## [{version}]");
    let mut found = false;
    let mut collecting = false;
    let mut fence = None;
    let mut notes = Vec::new();
    let mut bytes = 0usize;
    for line in text.lines() {
        let trimmed = line.trim_start();
        let marker = if trimmed.starts_with("```") {
            Some('`')
        } else if trimmed.starts_with("~~~") {
            Some('~')
        } else {
            None
        };
        if let Some(marker) = marker {
            if fence == Some(marker) {
                fence = None;
            } else if fence.is_none() {
                fence = Some(marker);
            }
        } else if fence.is_none() && line.starts_with("## [") {
            let matching = line
                .strip_prefix(&wanted)
                .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with(char::is_whitespace));
            if matching {
                if found {
                    return None;
                }
                found = true;
                collecting = true;
                continue;
            }
            collecting = false;
        }
        if collecting {
            bytes = bytes.checked_add(line.len() + 1)?;
            if bytes > MAX_RELEASE_NOTES_BYTES {
                return None;
            }
            notes.push(line);
        }
    }
    let notes = notes.join("\n");
    let notes = notes.trim();
    (found && !notes.is_empty()).then(|| notes.to_owned())
}

// Read only quoted href attributes of real <a> start tags, never comments,
// scripts, text, attribute substrings or caller-supplied download URLs.
fn canonical_asset_count(html: &str, relative: &str, canonical: &str) -> Result<usize> {
    let mut rest = html;
    let mut matches = 0;
    while let Some(open) = rest.find('<') {
        rest = &rest[open + 1..];
        if let Some(comment) = rest.strip_prefix("!--") {
            let end = comment
                .find("-->")
                .context("官网资源列表包含不完整的 HTML 注释")?;
            rest = &comment[end + 3..];
            continue;
        }
        let mut quote = None;
        let end = rest
            .bytes()
            .position(|byte| {
                if matches!(byte, b'\'' | b'"') {
                    if quote == Some(byte) {
                        quote = None;
                    } else if quote.is_none() {
                        quote = Some(byte);
                    }
                }
                byte == b'>' && quote.is_none()
            })
            .context("官网资源列表包含不完整的 HTML 标签")?;
        let element = &rest[..end];
        rest = &rest[end + 1..];
        let name_end = element
            .bytes()
            .position(|c| !c.is_ascii_alphanumeric() && c != b'-')
            .unwrap_or(element.len());
        let name = &element[..name_end];
        ensure!(
            !["script", "style", "template"]
                .iter()
                .any(|blocked| name.eq_ignore_ascii_case(blocked)),
            "官网资源列表包含无法验证的 HTML 内容"
        );
        if !name.eq_ignore_ascii_case("a") {
            continue;
        }
        let bytes = element.as_bytes();
        let mut index = name_end;
        let mut href = None;
        while index < bytes.len() {
            while index < bytes.len() && bytes[index].is_ascii_whitespace() {
                index += 1;
            }
            if index == bytes.len() || bytes[index] == b'/' {
                break;
            }
            let start = index;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric()
                    || matches!(bytes[index], b'-' | b'_' | b':'))
            {
                index += 1;
            }
            ensure!(index > start, "官网资源列表的 HTML 属性无效");
            let attribute = &element[start..index];
            while index < bytes.len() && bytes[index].is_ascii_whitespace() {
                index += 1;
            }
            if index == bytes.len() || bytes[index] != b'=' {
                continue;
            }
            index += 1;
            while index < bytes.len() && bytes[index].is_ascii_whitespace() {
                index += 1;
            }
            ensure!(index < bytes.len(), "官网资源列表的 HTML 属性缺少值");
            let delimiter = bytes[index];
            let quoted = matches!(delimiter, b'\'' | b'"');
            let value_start = index + usize::from(quoted);
            index = value_start;
            while index < bytes.len()
                && if quoted {
                    bytes[index] != delimiter
                } else {
                    !bytes[index].is_ascii_whitespace()
                }
            {
                index += 1;
            }
            ensure!(
                !quoted || index < bytes.len(),
                "官网资源列表的 HTML 属性未结束"
            );
            if attribute.eq_ignore_ascii_case("href") {
                ensure!(quoted && href.is_none(), "官网资源列表包含不明确的资源链接");
                href = Some(&element[value_start..index]);
            }
            index += usize::from(quoted);
        }
        if href.is_some_and(|href| href == relative || href == canonical) {
            matches += 1;
        }
    }
    Ok(matches)
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
    should_auto_check_in_timezone(settings, now, &Local)
}

// Keep persisted Unix timestamps compatible with older releases, interpreting
// each timestamp in the machine's local calendar rather than a rolling 24h.
fn should_auto_check_in_timezone<Tz: TimeZone>(
    settings: &UpdateSettings,
    now: u64,
    timezone: &Tz,
) -> bool {
    if !settings.auto_check {
        return false;
    }
    let day = |timestamp| {
        i64::try_from(timestamp)
            .ok()
            .and_then(|timestamp| timezone.timestamp_opt(timestamp, 0).single())
            .map(|timestamp| timestamp.date_naive())
    };
    let Some(today) = day(now) else {
        return false;
    };
    let last = settings
        .last_checked_unix
        .into_iter()
        .chain(settings.last_auto_attempt_unix)
        .max();
    last.is_none_or(|last| last <= now && day(last).is_some_and(|last_day| last_day < today))
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
fn request_http(endpoint: &UpdateEndpoint) -> Result<UpdateHttpResponse> {
    use std::process::{Command, Stdio};
    let (host, path) = endpoint.destination()?;
    let url = format!("https://{host}{path}");
    let headers = tempfile::NamedTempFile::new().context("无法创建更新响应头临时文件")?;
    // Development-only fallback. -q preserves strict TLS defaults by ignoring
    // user curl config; proxy and trusted CA environment bindings still apply.
    let mut command = Command::new("curl");
    command.args([
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
    ]);
    for header in endpoint
        .accept_headers()
        .split("\r\n")
        .filter(|value| !value.is_empty())
    {
        command.args(["--header", header]);
    }
    let mut child = command
        .arg("--dump-header")
        .arg(headers.path())
        .args(["--write-out", "\n%{http_code}", &url])
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
    let mut raw_headers = String::new();
    headers
        .reopen()
        .context("无法读取更新响应头")?
        .take((MAX_HEADER_BYTES + 1) as u64)
        .read_to_string(&mut raw_headers)
        .context("更新响应头无效")?;
    ensure!(raw_headers.len() <= MAX_HEADER_BYTES, "更新响应头过长");
    let final_headers = raw_headers
        .rsplit("\r\n\r\n")
        .find(|block| block.starts_with("HTTP/"))
        .context("更新响应缺少 HTTP 响应头")?;
    let mut location = None;
    for line in final_headers.lines().skip(1) {
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("location")
        {
            ensure!(location.is_none(), "更新响应包含重复的 Location");
            ensure!(value.len() <= MAX_LOCATION_BYTES, "官网发布重定向过长");
            location = Some(value.trim().to_owned());
        }
    }
    Ok(UpdateHttpResponse {
        status,
        body: output,
        location,
    })
}

#[cfg(windows)]
fn request_http(endpoint: &UpdateEndpoint) -> Result<UpdateHttpResponse> {
    windows_http::request(endpoint)
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

    fn location_header(request: &InternetHandle) -> Result<Option<String>> {
        unsafe {
            let mut length = 0u32;
            let result = WinHttpQueryHeaders(
                request.0,
                WINHTTP_QUERY_LOCATION,
                null(),
                null_mut(),
                &mut length,
                null_mut(),
            );
            if result == 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(ERROR_WINHTTP_HEADER_NOT_FOUND as i32) {
                    return Ok(None);
                }
                // ERROR_INSUFFICIENT_BUFFER: length is a byte count, including
                // the UTF-16 terminator. Other failures keep their Windows code.
                if error.raw_os_error() != Some(122) {
                    return Err(error).context("更新检查：读取 Location 长度失败");
                }
            }
            ensure!(
                length >= 2 && length as usize <= MAX_LOCATION_BYTES && length.is_multiple_of(2),
                "更新响应的 Location 长度无效"
            );
            let mut buffer = vec![0u16; length as usize / 2];
            succeeded(
                WinHttpQueryHeaders(
                    request.0,
                    WINHTTP_QUERY_LOCATION,
                    null(),
                    buffer.as_mut_ptr().cast(),
                    &mut length,
                    null_mut(),
                ),
                "读取 Location",
            )?;
            ensure!(
                length.is_multiple_of(2) && length as usize / 2 <= buffer.len(),
                "更新响应的 Location 长度无效"
            );
            buffer.truncate(length as usize / 2);
            // WinHttpQueryHeaders returns bytes excluding the trailing NUL on
            // success. Embedded NULs are never valid release redirect targets.
            ensure!(!buffer.contains(&0), "更新响应的 Location 包含无效字符");
            Ok(Some(
                String::from_utf16(&buffer).context("更新响应的 Location 编码无效")?,
            ))
        }
    }

    pub(super) fn request(endpoint: &UpdateEndpoint) -> Result<UpdateHttpResponse> {
        let (hostname, request_path) = endpoint.destination()?;
        let agent = wide(concat!("FlashTool/", env!("CARGO_PKG_VERSION")));
        let host = wide(hostname);
        let method = wide("GET");
        let path = wide(&request_path);
        let headers = wide(endpoint.accept_headers());
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
            let location = location_header(&request)?;
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
            Ok(UpdateHttpResponse {
                status: status.try_into().context("HTTP 状态超出有效范围")?,
                body,
                location,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_errors_never_imply_that_a_release_is_absent() {
        for status in [301, 403, 404, 429, 500] {
            assert!(
                interpret_response(
                    "0.3.0",
                    UpdateHttpResponse {
                        status,
                        body: Vec::new(),
                        location: None,
                    }
                )
                .is_err()
            );
        }
    }

    #[test]
    fn daily_schedule_uses_local_midnight_instead_of_utc_or_elapsed_hours() {
        let timezone = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
        let local = |day, hour, minute, second| {
            u64::try_from(
                timezone
                    .with_ymd_and_hms(2026, 11, day, hour, minute, second)
                    .unwrap()
                    .timestamp(),
            )
            .unwrap()
        };
        let mut settings = UpdateSettings {
            last_auto_attempt_unix: Some(local(19, 23, 59, 55)),
            ..UpdateSettings::default()
        };
        assert!(should_auto_check_in_timezone(
            &settings,
            local(20, 0, 0, 5),
            &timezone
        ));
        settings.last_auto_attempt_unix = Some(local(20, 0, 0, 5));
        // Crossing UTC midnight eight hours later is still the same local day.
        assert!(!should_auto_check_in_timezone(
            &settings,
            local(20, 8, 0, 5),
            &timezone
        ));
        settings.last_checked_unix = Some(local(20, 9, 0, 0));
        assert!(!should_auto_check_in_timezone(
            &settings,
            local(20, 23, 59, 59),
            &timezone
        ));
        assert!(should_auto_check_in_timezone(
            &settings,
            local(21, 0, 0, 0),
            &timezone
        ));
        assert!(!should_auto_check_in_timezone(
            &settings,
            local(19, 12, 0, 0),
            &timezone
        ));
        assert!(!should_auto_check_in_timezone(
            &settings,
            u64::MAX,
            &timezone
        ));
        settings.last_auto_attempt_unix = Some(u64::MAX);
        assert!(!should_auto_check_in_timezone(
            &settings,
            local(21, 0, 0, 0),
            &timezone
        ));
    }

    #[test]
    fn fallback_notes_endpoint_is_fixed_to_the_exact_official_tag() {
        assert_eq!(
            UpdateEndpoint::ReleaseNotes {
                tag: "v0.5.2".into()
            }
            .destination()
            .unwrap(),
            (
                "raw.githubusercontent.com",
                "/xiaoxiaoawei/hw_hub/refs/tags/v0.5.2/CHANGELOG.md".into()
            )
        );
        for tag in ["v0.5.2/../../main", "v0.5.2?x=bad", "v0.5.2-rc.1", "main"] {
            assert!(
                UpdateEndpoint::ReleaseNotes { tag: tag.into() }
                    .destination()
                    .is_err()
            );
        }
    }

    #[test]
    fn changelog_notes_take_one_exact_release_section_and_preserve_code_blocks() {
        let markdown = "# Changelog\n\n## [0.5.3] - date\nnewer-only\n\n## [0.5.2] - date\n- 正确说明\n```text\n## [0.0.1] example inside code\n```\n\n## [0.5.1]\nolder-only\n";
        assert_eq!(
            changelog_release_notes("0.5.2", markdown).as_deref(),
            Some("- 正确说明\n```text\n## [0.0.1] example inside code\n```")
        );
        for markdown in [
            "## [0.5.21]\nwrong prefix",
            "## [0.5.2]unexpected\nwrong boundary",
            "## [0.5.2]\nfirst\n## [0.5.2]\nambiguous",
            "## [0.5.2]\n\n## [0.5.1]\nold",
        ] {
            assert!(changelog_release_notes("0.5.2", markdown).is_none());
        }
    }
}
