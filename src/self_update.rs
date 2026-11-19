//! Opt-in portable Windows updates. Discovery alone never downloads or installs.
//! Packages are pinned to this repository's release URL and SHA256SUMS, checked
//! before extraction, and handed to a separate, rollback-capable helper.
use crate::{self_update_install, updater::ReleaseUpdate};
use anyhow::{Context, Result, ensure};
use semver::Version;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{Read, Seek},
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
};

#[path = "self_update_http.rs"]
mod http;

pub use crate::self_update_install::{PreparedUpdate, UpdateFile, launch_installer};

pub const MAX_ARCHIVE_BYTES: u64 = 128 * 1024 * 1024;
pub const MAX_CHECKSUM_BYTES: u64 = 64 * 1024;
const MAX_UNPACKED_BYTES: u64 = 512 * 1024 * 1024;
const MAX_MEMBER_BYTES: u64 = 128 * 1024 * 1024;
const MAX_ENTRIES: usize = 8192;
const RELEASES_URL: &str = "https://github.com/xiaoxiaoawei/hw_hub/releases";

#[derive(Clone, Debug)]
pub enum SelfUpdateEvent {
    Progress {
        downloaded: u64,
        total: Option<u64>,
        phase: String,
    },
    Ready(PreparedUpdate),
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct VerifiedPackage {
    pub version: String,
    pub executable_path: PathBuf,
    pub executable_sha256: String,
    pub files: Vec<UpdateFile>,
}

pub struct SelfUpdateService {
    receiver: Option<Receiver<SelfUpdateEvent>>,
    wake: Arc<dyn Fn() + Send + Sync>,
    busy: bool,
}

impl SelfUpdateService {
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            receiver: None,
            wake: Arc::new(wake),
            busy: false,
        }
    }

    /// Call only after the user's explicit confirmation. No filesystem or
    /// network operation runs on the GUI thread.
    pub fn start_download(&mut self, release: ReleaseUpdate) -> Result<()> {
        ensure!(!self.busy, "更新包正在准备，请等待当前操作完成");
        validate_release(env!("CARGO_PKG_VERSION"), &release)?;
        ensure!(
            cfg!(windows),
            "自动安装仅支持 Windows 便携版；请在 Windows 中更新"
        );
        let (sender, receiver) = mpsc::sync_channel(32);
        let wake = self.wake.clone();
        thread::Builder::new()
            .name("flashtool-self-update".into())
            .spawn(move || {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    download_and_prepare(&release, &sender, &wake)
                }))
                .unwrap_or_else(|_| {
                    Err(anyhow::anyhow!("更新包准备线程出现异常；当前程序未被替换"))
                });
                let event = match result {
                    Ok(update) => SelfUpdateEvent::Ready(update),
                    Err(error) => SelfUpdateEvent::Failed(format!("{error:#}")),
                };
                if sender.send(event).is_ok() {
                    wake();
                }
            })
            .context("无法启动更新下载线程")?;
        self.receiver = Some(receiver);
        self.busy = true;
        Ok(())
    }

    pub fn poll(&mut self) -> Option<SelfUpdateEvent> {
        let event = match self.receiver.as_ref()?.try_recv() {
            Ok(event) => event,
            Err(mpsc::TryRecvError::Empty) => return None,
            Err(mpsc::TryRecvError::Disconnected) => {
                SelfUpdateEvent::Failed("更新下载线程已断开；当前程序未被替换".into())
            }
        };
        if matches!(
            event,
            SelfUpdateEvent::Ready(_) | SelfUpdateEvent::Failed(_)
        ) {
            self.busy = false;
            self.receiver = None;
        }
        Some(event)
    }

    pub fn is_busy(&self) -> bool {
        self.busy
    }
}

/// Validate again at the download entry point, independent of discovery.
pub fn validate_release(current: &str, release: &ReleaseUpdate) -> Result<()> {
    let version = stable_version(&release.version)?;
    let installed = Version::parse(current).context("当前程序版本无效")?;
    ensure!(
        version > installed,
        "更新版本必须高于当前版本，已阻止重复安装或降级"
    );
    let tag = format!("v{version}");
    ensure!(
        release.release_url == format!("{RELEASES_URL}/tag/{tag}"),
        "更新发布页不属于本仓库与指定版本"
    );
    ensure!(
        release.download_url.as_deref()
            == Some(&format!(
                "{RELEASES_URL}/download/{tag}/FlashTool-{tag}-windows-x64.zip"
            )),
        "更新包不属于本仓库与指定版本"
    );
    Ok(())
}

fn stable_version(text: &str) -> Result<Version> {
    let version = Version::parse(text).context("更新版本不是有效的 SemVer")?;
    ensure!(
        version.pre.is_empty() && version.build.is_empty() && version.to_string() == text,
        "更新仅支持严格的稳定版本 MAJOR.MINOR.PATCH"
    );
    ensure!(
        [version.major, version.minor, version.patch]
            .iter()
            .all(|part| *part <= u16::MAX as u64),
        "更新版本超过 Windows 版本资源范围"
    );
    Ok(version)
}

fn progress(
    sender: &SyncSender<SelfUpdateEvent>,
    wake: &Arc<dyn Fn() + Send + Sync>,
    downloaded: u64,
    total: Option<u64>,
    phase: &str,
) {
    if sender
        .try_send(SelfUpdateEvent::Progress {
            downloaded,
            total,
            phase: phase.into(),
        })
        .is_ok()
    {
        wake();
    }
}

fn download_and_prepare(
    release: &ReleaseUpdate,
    sender: &SyncSender<SelfUpdateEvent>,
    wake: &Arc<dyn Fn() + Send + Sync>,
) -> Result<PreparedUpdate> {
    validate_release(env!("CARGO_PKG_VERSION"), release)?;
    let parent = staging_parent()?;
    fs::create_dir_all(&parent).context("无法创建更新缓存目录")?;
    let stage = tempfile::Builder::new()
        .prefix("update-")
        .tempdir_in(&parent)
        .context("无法创建私有更新目录")?
        .keep();
    let result = (|| {
        let tag = format!("v{}", release.version);
        let checksum_path = stage.join("SHA256SUMS");
        let name = format!("FlashTool-{tag}-windows-x64.zip");
        let archive_path = stage.join(&name);
        progress(sender, wake, 0, None, "正在获取同版本 SHA256SUMS");
        http::download_official_asset(
            &tag,
            "SHA256SUMS",
            &checksum_path,
            MAX_CHECKSUM_BYTES,
            &mut |downloaded, total| progress(sender, wake, downloaded, total, "正在获取校验信息"),
        )?;
        let checksums = fs::read(&checksum_path).context("无法读取更新校验文件")?;
        expected_archive_sha256(&release.version, &checksums)?;
        http::download_official_asset(
            &tag,
            &name,
            &archive_path,
            MAX_ARCHIVE_BYTES,
            &mut |downloaded, total| {
                progress(sender, wake, downloaded, total, "正在下载 Windows 更新包")
            },
        )?;
        progress(sender, wake, 0, None, "正在校验并安全解压更新包");
        let extracted = stage.join("payload");
        let package =
            verify_and_extract_package(&release.version, &archive_path, &checksums, &extracted)?;
        progress(sender, wake, 0, None, "正在准备独立安装助手");
        self_update_install::prepare_installer(
            &release.version,
            &stage,
            &extracted,
            &package.executable_sha256,
            package.files,
        )
    })();
    if let Err(error) = &result {
        let _ = fs::write(stage.join("download-error.txt"), format!("{error:#}\n"));
    }
    result.with_context(|| format!("更新缓存保留在 {}；当前程序未被替换", stage.display()))
}

fn staging_parent() -> Result<PathBuf> {
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .context("Windows 未提供 LOCALAPPDATA")?;
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    Ok(base.join("FlashTool").join("updates"))
}

/// The checksum file is bounded, strict, and must name this exact archive once.
pub fn expected_archive_sha256(version: &str, checksums: &[u8]) -> Result<String> {
    stable_version(version)?;
    ensure!(
        !checksums.is_empty() && checksums.len() as u64 <= MAX_CHECKSUM_BYTES,
        "更新校验文件为空或超过 64 KiB"
    );
    let text = std::str::from_utf8(checksums)
        .context("更新校验文件不是 UTF-8")?
        .trim_start_matches('\u{feff}');
    let wanted = format!("FlashTool-v{version}-windows-x64.zip");
    let mut seen = BTreeSet::new();
    let mut expected = None;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        ensure!(line.len() >= 67, "更新校验文件包含不完整的行");
        let digest = line
            .get(..64)
            .context("更新校验文件摘要不是 64 位 ASCII 十六进制")?;
        let rest = line.get(64..).context("更新校验文件摘要边界无效")?;
        ensure!(
            digest.bytes().all(|byte| byte.is_ascii_hexdigit())
                && (rest.starts_with("  ") || rest.starts_with(" *")),
            "更新校验文件格式无效"
        );
        let name = &rest[2..];
        ensure!(
            !name.is_empty()
                && !name.contains(['/', '\\', '\0', '\r', '\n'])
                && seen.insert(name.to_ascii_lowercase()),
            "更新校验文件包含重复或非法文件名"
        );
        if name == wanted {
            expected = Some(digest.to_ascii_lowercase());
        }
    }
    expected.context("同版本 SHA256SUMS 未包含指定 Windows 更新包")
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).context("无法读取更新文件进行 SHA256 校验")?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).context("读取更新文件失败")?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

/// A local-only validation seam for deterministic archive tests/native smoke.
/// Does not run the new executable or touch a running application's directory.
pub fn verify_and_extract_package(
    version: &str,
    archive_path: &Path,
    checksums: &[u8],
    extracted_dir: &Path,
) -> Result<VerifiedPackage> {
    stable_version(version)?;
    let expected = expected_archive_sha256(version, checksums)?;
    let metadata = fs::symlink_metadata(archive_path).context("无法检查已下载的 ZIP")?;
    ensure!(
        metadata.file_type().is_file() && metadata.len() > 0 && metadata.len() <= MAX_ARCHIVE_BYTES,
        "更新 ZIP 类型或大小无效"
    );
    ensure!(
        sha256_file(archive_path)? == expected,
        "更新 ZIP 的 SHA256 不匹配；未解压或安装"
    );
    let mut archive =
        zip::ZipArchive::new(File::open(archive_path)?).context("更新包不是有效 ZIP")?;
    ensure!(
        !archive.is_empty() && archive.len() <= MAX_ENTRIES,
        "更新 ZIP 文件数量无效或过多"
    );
    // Inspect every entry before creating the destination. Duplicate names are
    // case-insensitive to match Windows, including directory/file collisions.
    let mut entries = Vec::new();
    let mut names = BTreeSet::new();
    let mut unpacked = 0u64;
    let mut has_executable = false;
    let mut has_metadata = false;
    for index in 0..archive.len() {
        let entry = archive.by_index(index).context("无法读取更新 ZIP 目录")?;
        let name = safe_member_name(entry.name())?;
        ensure!(
            names.insert(name.to_ascii_lowercase()),
            "更新 ZIP 含大小写冲突或重复路径"
        );
        if let Some(mode) = entry.unix_mode() {
            ensure!(mode & 0o170000 != 0o120000, "更新 ZIP 不允许符号链接");
        }
        if entry.is_dir() {
            entries.push((index, name, true));
            continue;
        }
        ensure!(
            allowed_package_file(&name),
            "更新 ZIP 含不允许安装的文件：{name}"
        );
        ensure!(
            entry.size() <= MAX_MEMBER_BYTES,
            "更新 ZIP 单个文件超过 128 MiB"
        );
        unpacked = unpacked
            .checked_add(entry.size())
            .context("更新 ZIP 大小溢出")?;
        ensure!(
            unpacked <= MAX_UNPACKED_BYTES,
            "更新 ZIP 解压总量超过 512 MiB"
        );
        has_executable |= name == "FlashTool.exe";
        has_metadata |= name == "BUILD-METADATA.json";
        entries.push((index, name, false));
    }
    ensure!(
        has_executable && has_metadata,
        "更新 ZIP 缺少 FlashTool.exe 或 BUILD-METADATA.json"
    );
    ensure!(!extracted_dir.exists(), "更新解压目录必须是全新的私有目录");
    fs::create_dir_all(extracted_dir).context("无法创建更新解压目录")?;
    let mut files = Vec::new();
    for (index, name, directory) in entries {
        let target = extracted_dir.join(&name);
        if directory {
            fs::create_dir_all(&target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut source = archive.by_index(index)?;
        let expected_size = source.size();
        let mut destination = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .context("无法创建解压文件")?;
        let count = std::io::copy(
            &mut source.by_ref().take(MAX_MEMBER_BYTES + 1),
            &mut destination,
        )
        .context("更新 ZIP 解压或 CRC 校验失败")?;
        ensure!(
            count == expected_size && count <= MAX_MEMBER_BYTES,
            "更新 ZIP 解压长度不匹配"
        );
        destination.sync_all().context("无法持久化解压文件")?;
        files.push(UpdateFile {
            relative_path: name,
            sha256: sha256_file(&target)?,
        });
    }
    let metadata_path = extracted_dir.join("BUILD-METADATA.json");
    ensure!(
        fs::metadata(&metadata_path)?.len() <= 64 * 1024,
        "更新包构建元数据过大"
    );
    let metadata: serde_json::Value =
        serde_json::from_reader(File::open(metadata_path)?).context("更新包构建元数据无效")?;
    ensure!(
        metadata.get("version").and_then(serde_json::Value::as_str) == Some(version),
        "更新包声明的版本与发布版本不一致"
    );
    ensure!(
        matches!(
            metadata.get("target").and_then(serde_json::Value::as_str),
            Some("x86_64-pc-windows-msvc" | "x86_64-pc-windows-gnu")
        ),
        "更新包不是 Windows x64 版本"
    );
    let executable_path = extracted_dir.join("FlashTool.exe");
    validate_windows_executable(&executable_path, version)?;
    let executable_sha256 = sha256_file(&executable_path)?;
    Ok(VerifiedPackage {
        version: version.into(),
        executable_path,
        executable_sha256,
        files,
    })
}

fn safe_member_name(raw: &str) -> Result<String> {
    ensure!(
        !raw.is_empty()
            && raw.len() <= 1024
            && !raw.contains(['\\', ':', '\0'])
            && !raw.starts_with('/'),
        "更新 ZIP 路径无效或存在目录穿越风险"
    );
    let name = raw.strip_suffix('/').unwrap_or(raw);
    ensure!(
        !name.is_empty()
            && name.split('/').all(|part| !part.is_empty()
                && part != "."
                && part != ".."
                && !part.ends_with(['.', ' '])
                && !part.chars().any(char::is_control)
                && !windows_reserved(part)),
        "更新 ZIP 路径包含不安全的 Windows 名称"
    );
    ensure!(
        Path::new(name)
            .components()
            .all(|part| matches!(part, Component::Normal(_))),
        "更新 ZIP 路径不是安全相对路径"
    );
    Ok(name.into())
}

fn windows_reserved(part: &str) -> bool {
    let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
    matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || (stem.len() == 4
        && (stem.starts_with("COM") || stem.starts_with("LPT"))
        && stem.as_bytes()[3].is_ascii_digit())
}

pub fn allowed_package_file(name: &str) -> bool {
    let path = Path::new(name);
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if [
        "exe", "dll", "com", "bat", "cmd", "ps1", "psm1", "vbs", "js", "msi", "scr", "pif", "lnk",
        "url", "reg",
    ]
    .contains(&extension.as_str())
    {
        return name == "FlashTool.exe";
    }
    if !name.contains('/') {
        return matches!(
            name,
            "README.md" | "CHANGELOG.md" | "BUILD-METADATA.json" | "FlashTool.exe"
        ) || ["LICENSE", "COPYING", "NOTICE", "THIRD_PARTY"]
            .iter()
            .any(|prefix| name.starts_with(prefix));
    }
    name.starts_with("docs/")
        || name.starts_with("drivers/")
        || name.starts_with("third-party-licenses/")
}

/// Both extraction and the helper recheck the Windows x64 GUI image and its
/// VERSIONINFO before any replacement. This works without running the image.
pub fn validate_windows_executable(path: &Path, expected_version: &str) -> Result<()> {
    let version = stable_version(expected_version)?;
    let metadata = fs::symlink_metadata(path).context("无法检查更新 EXE")?;
    ensure!(
        metadata.file_type().is_file() && (256..=MAX_MEMBER_BYTES).contains(&metadata.len()),
        "更新 EXE 大小或类型无效"
    );
    let mut file = File::open(path).context("无法读取更新 EXE")?;
    let mut dos = [0u8; 64];
    file.read_exact(&mut dos).context("更新 EXE 头不完整")?;
    ensure!(&dos[..2] == b"MZ", "更新文件不是 Windows PE");
    let pe_offset = u32::from_le_bytes(dos[60..64].try_into().unwrap()) as u64;
    ensure!(
        pe_offset >= 64
            && pe_offset
                .checked_add(96)
                .is_some_and(|end| end <= metadata.len()),
        "更新 PE 头位置无效"
    );
    file.seek(std::io::SeekFrom::Start(pe_offset))?;
    let mut pe = [0u8; 96];
    file.read_exact(&mut pe)?;
    let number = |offset: usize| u16::from_le_bytes([pe[offset], pe[offset + 1]]);
    let optional_size = number(20) as u64;
    ensure!(
        &pe[..4] == b"PE\0\0"
            && number(4) == 0x8664
            && optional_size >= 112
            && pe_offset + 24 + optional_size <= metadata.len()
            && number(22) & 0x0002 != 0
            && number(22) & 0x2000 == 0
            && number(24) == 0x20b
            && number(92) == 2,
        "更新 EXE 不是有效的 Windows x64 GUI 程序"
    );
    #[cfg(windows)]
    verify_resource_version(path, &version)?;
    #[cfg(not(windows))]
    let _ = version;
    Ok(())
}

#[cfg(windows)]
fn verify_resource_version(path: &Path, expected: &Version) -> Result<()> {
    use std::{ffi::c_void, ptr::null_mut};
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VS_FIXEDFILEINFO, VerQueryValueW,
    };
    let path: Vec<u16> = path
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain(Some(0))
        .collect();
    unsafe {
        let mut ignored = 0;
        let size = GetFileVersionInfoSizeW(path.as_ptr(), &mut ignored);
        ensure!(
            size > 0 && size <= 1024 * 1024,
            "更新 EXE 缺少有界的 Windows 版本资源"
        );
        let mut bytes = vec![0u8; size as usize];
        ensure!(
            GetFileVersionInfoW(path.as_ptr(), 0, size, bytes.as_mut_ptr().cast()) != 0,
            "无法读取更新 EXE 版本资源"
        );
        let query = [b'\\' as u16, 0];
        let mut value: *mut c_void = null_mut();
        let mut length = 0;
        ensure!(
            VerQueryValueW(
                bytes.as_ptr().cast(),
                query.as_ptr(),
                &mut value,
                &mut length
            ) != 0
                && !value.is_null()
                && length as usize >= std::mem::size_of::<VS_FIXEDFILEINFO>(),
            "更新 EXE 版本资源无效"
        );
        let info = std::ptr::read_unaligned(value.cast::<VS_FIXEDFILEINFO>());
        let ms = ((expected.major as u32) << 16) | expected.minor as u32;
        let ls = (expected.patch as u32) << 16;
        ensure!(
            info.dwSignature == 0xfeef04bd
                && info.dwFileType == 1
                && info.dwFileVersionMS == ms
                && info.dwFileVersionLS == ls
                && info.dwProductVersionMS == ms
                && info.dwProductVersionLS == ls,
            "更新 EXE 内嵌版本与指定发布版本不一致"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn pe_fixture(version: (u16, u16, u16)) -> Vec<u8> {
        // A tiny, non-runnable fixture with a real VERSIONINFO resource. No
        // downloaded executable or subprocess is used to exercise the parser.
        let mut bytes = vec![0u8; 1024];
        fn word(bytes: &mut [u8], at: usize, value: u16) {
            bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
        }
        fn dword(bytes: &mut [u8], at: usize, value: u32) {
            bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
        }
        bytes[..2].copy_from_slice(b"MZ");
        dword(&mut bytes, 60, 0x80);
        bytes[0x80..0x84].copy_from_slice(b"PE\0\0");
        word(&mut bytes, 0x84, 0x8664);
        word(&mut bytes, 0x86, 1);
        word(&mut bytes, 0x94, 240);
        word(&mut bytes, 0x96, 0x22);
        word(&mut bytes, 0x98, 0x20b);
        dword(&mut bytes, 0xa8, 0x1000);
        dword(&mut bytes, 0xb8, 0x1000);
        dword(&mut bytes, 0xbc, 0x200);
        dword(&mut bytes, 0xd0, 0x2000);
        dword(&mut bytes, 0xd4, 0x200);
        word(&mut bytes, 0xdc, 2);
        dword(&mut bytes, 0x104, 16);
        dword(&mut bytes, 0x118, 0x1000);
        dword(&mut bytes, 0x11c, 512);
        bytes[0x188..0x18d].copy_from_slice(b".rsrc");
        for (at, value) in [
            (0x190, 512),
            (0x194, 0x1000),
            (0x198, 512),
            (0x19c, 0x200),
            (0x1ac, 0x40000040),
        ] {
            dword(&mut bytes, at, value);
        }
        for base in [0x200, 0x218, 0x230] {
            word(&mut bytes, base + 14, 1);
        }
        for (at, value) in [
            (0x210, 16),
            (0x214, 0x80000018),
            (0x228, 1),
            (0x22c, 0x80000030),
            (0x240, 1033),
            (0x244, 72),
            (0x248, 0x1058),
            (0x24c, 92),
        ] {
            dword(&mut bytes, at, value);
        }
        let blob = 0x258;
        word(&mut bytes, blob, 92);
        word(&mut bytes, blob + 2, 52);
        for (index, character) in "VS_VERSION_INFO\0".encode_utf16().enumerate() {
            word(&mut bytes, blob + 6 + index * 2, character);
        }
        let fixed = blob + 40;
        let ms = ((version.0 as u32) << 16) | version.1 as u32;
        let ls = (version.2 as u32) << 16;
        for (at, value) in [
            (0, 0xfeef04bd),
            (4, 0x10000),
            (8, ms),
            (12, ls),
            (16, ms),
            (20, ls),
            (24, 0x3f),
            (32, 0x40004),
            (36, 1),
        ] {
            dword(&mut bytes, fixed + at, value);
        }
        bytes
    }

    fn zip_fixture(root: &Path, entries: &[(&str, Vec<u8>)]) -> (PathBuf, Vec<u8>) {
        let path = root.join("package.zip");
        let mut writer = zip::ZipWriter::new(File::create(&path).unwrap());
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, bytes) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap().sync_all().unwrap();
        let sums = format!(
            "{}  FlashTool-v1.2.3-windows-x64.zip\n",
            sha256_file(&path).unwrap()
        )
        .into_bytes();
        (path, sums)
    }

    fn valid_members() -> Vec<(&'static str, Vec<u8>)> {
        vec![
            ("FlashTool.exe", pe_fixture((1, 2, 3))),
            (
                "BUILD-METADATA.json",
                br#"{"version":"1.2.3","target":"x86_64-pc-windows-msvc"}"#.to_vec(),
            ),
            ("README.md", "FlashTool 使用说明".as_bytes().to_vec()),
            ("docs/log-upload-api.md", b"API schema".to_vec()),
        ]
    }

    #[test]
    fn malformed_unicode_checksum_boundary_returns_error_without_panicking() {
        let text = format!("{}é  FlashTool-v0.5.1-windows-x64.zip\n", "a".repeat(63));
        assert!(expected_archive_sha256("0.5.1", text.as_bytes()).is_err());
    }

    #[test]
    fn archive_integrity_and_valid_package_preserve_docs_without_touching_installed_files() {
        let directory = tempfile::tempdir().unwrap();
        let untouched = directory.path().join("installed-settings.json");
        fs::write(&untouched, b"keep user configuration").unwrap();
        let (archive, sums) = zip_fixture(directory.path(), &valid_members());
        let output = directory.path().join("payload");
        let package = verify_and_extract_package("1.2.3", &archive, &sums, &output).unwrap();
        assert_eq!(package.version, "1.2.3");
        assert_eq!(
            package.executable_sha256,
            sha256_file(&package.executable_path).unwrap()
        );
        assert!(
            package
                .files
                .iter()
                .any(|file| file.relative_path == "docs/log-upload-api.md")
        );
        assert_eq!(
            fs::read(output.join("docs/log-upload-api.md")).unwrap(),
            b"API schema"
        );
        assert_eq!(fs::read(untouched).unwrap(), b"keep user configuration");
        assert!(verify_and_extract_package("1.2.3", &archive, &sums, &output).is_err());
    }

    #[test]
    fn wrong_checksum_is_rejected_before_any_extraction() {
        let directory = tempfile::tempdir().unwrap();
        let (archive, _) = zip_fixture(directory.path(), &valid_members());
        let output = directory.path().join("payload");
        let sums = format!("{}  FlashTool-v1.2.3-windows-x64.zip\n", "0".repeat(64));
        assert!(verify_and_extract_package("1.2.3", &archive, sums.as_bytes(), &output).is_err());
        assert!(!output.exists());
    }

    #[test]
    fn unsafe_archive_paths_extra_programs_and_case_collisions_are_rejected() {
        for name in [
            "../outside.txt",
            "/absolute.txt",
            "docs/../../outside.txt",
            "docs\\outside.txt",
            "docs/file:stream",
            "docs/NUL.txt",
            "docs/trailing. ",
            "docs/extra.exe",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let mut members = valid_members();
            members.push((name, b"untrusted".to_vec()));
            let (archive, sums) = zip_fixture(directory.path(), &members);
            assert!(
                verify_and_extract_package(
                    "1.2.3",
                    &archive,
                    &sums,
                    &directory.path().join("payload")
                )
                .is_err(),
                "must reject {name}"
            );
            assert!(!directory.path().join("outside.txt").exists());
        }
        let directory = tempfile::tempdir().unwrap();
        let mut members = valid_members();
        members.push(("docs/LOG-UPLOAD-API.md", b"duplicate".to_vec()));
        let (archive, sums) = zip_fixture(directory.path(), &members);
        assert!(
            verify_and_extract_package("1.2.3", &archive, &sums, &directory.path().join("payload"))
                .is_err()
        );
    }

    #[test]
    fn wrong_package_version_target_and_pe_type_are_rejected() {
        for metadata in [
            br#"{"version":"1.2.4","target":"x86_64-pc-windows-msvc"}"#.as_slice(),
            br#"{"version":"1.2.3","target":"x86_64-unknown-linux-gnu"}"#.as_slice(),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let mut members = valid_members();
            members[1].1 = metadata.to_vec();
            let (archive, sums) = zip_fixture(directory.path(), &members);
            assert!(
                verify_and_extract_package(
                    "1.2.3",
                    &archive,
                    &sums,
                    &directory.path().join("payload")
                )
                .is_err()
            );
        }
        let directory = tempfile::tempdir().unwrap();
        for (offset, value) in [(0x84, 0x014c_u16), (0x96, 0x2022), (0xdc, 3)] {
            let mut bytes = pe_fixture((1, 2, 3));
            bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
            let path = directory.path().join(format!("invalid-{offset}.exe"));
            fs::write(&path, bytes).unwrap();
            assert!(validate_windows_executable(&path, "1.2.3").is_err());
        }
        #[cfg(windows)]
        {
            let path = directory.path().join("wrong-version.exe");
            fs::write(&path, pe_fixture((1, 2, 4))).unwrap();
            assert!(validate_windows_executable(&path, "1.2.3").is_err());
        }
    }

    #[test]
    fn download_entry_refuses_downgrades_prereleases_and_origin_substitution() {
        let release = ReleaseUpdate {
            version: "1.2.3".into(),
            release_url: format!("{RELEASES_URL}/tag/v1.2.3"),
            download_url: Some(format!(
                "{RELEASES_URL}/download/v1.2.3/FlashTool-v1.2.3-windows-x64.zip"
            )),
            notes: String::new(),
        };
        validate_release("1.2.2", &release).unwrap();
        for current in ["1.2.3", "1.2.4", "2.0.0"] {
            assert!(validate_release(current, &release).is_err());
        }
        let mut unsafe_release = release.clone();
        unsafe_release.download_url = Some("https://evil.example/FlashTool.exe".into());
        assert!(validate_release("1.2.2", &unsafe_release).is_err());
        unsafe_release = release;
        unsafe_release.version = "1.2.3-rc.1".into();
        assert!(validate_release("1.2.2", &unsafe_release).is_err());
        let duplicate = format!("{}  FlashTool-v1.2.3-windows-x64.zip\n", "a".repeat(64)).repeat(2);
        assert!(expected_archive_sha256("1.2.3", duplicate.as_bytes()).is_err());
    }
}
