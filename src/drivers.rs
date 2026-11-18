//! Offline vendor-driver installation. Windows performs signature validation;
//! this module never disables it or changes drivers without an explicit call.

use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{self, Cursor, Read, Seek, Write},
    path::{Path, PathBuf},
};

const MAX_ZIP_BYTES: u64 = 64 * 1024 * 1024;
const MAX_EXPANDED_BYTES: u64 = 128 * 1024 * 1024;
const MAX_ENTRY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ENTRIES: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriverKind {
    Ch343,
    Cp210x,
}

impl DriverKind {
    fn zip_name(self) -> &'static str {
        match self {
            Self::Ch343 => "ch343.zip",
            Self::Cp210x => "cp210x.zip",
        }
    }

    fn inf_name(self) -> &'static str {
        match self {
            Self::Ch343 => "ch343ser.inf",
            Self::Cp210x => "silabser.inf",
        }
    }

    fn expected_hardware_ids(self) -> &'static [&'static str] {
        match self {
            Self::Ch343 => &["USB\\VID_1A86&PID_55D3", "USB\\VID_1A86&PID_55D4"],
            Self::Cp210x => &["USB\\VID_10C4&PID_EA60"],
        }
    }
}

fn embedded_package(kind: DriverKind) -> Option<&'static [u8]> {
    match kind {
        DriverKind::Ch343 => {
            #[cfg(has_ch343_driver)]
            return Some(include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/assets/drivers/ch343.zip"
            )));
            #[cfg(not(has_ch343_driver))]
            None
        }
        DriverKind::Cp210x => {
            #[cfg(has_cp210x_driver)]
            return Some(include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/assets/drivers/cp210x.zip"
            )));
            #[cfg(not(has_cp210x_driver))]
            None
        }
    }
}

fn adjacent_package(kind: DriverKind) -> Option<PathBuf> {
    let path = std::env::current_exe()
        .ok()?
        .parent()?
        .join("drivers")
        .join(kind.zip_name());
    fs::metadata(&path)
        .ok()
        .filter(|m| m.is_file() && m.len() > 0 && m.len() <= MAX_ZIP_BYTES)
        .map(|_| path)
}

/// Availability means that bytes are present, not that a driver is installed.
pub fn package_available(kind: DriverKind) -> bool {
    package_source(kind).is_some()
}

/// Distinguish a compiled-in package from a user-provided, adjacent ZIP.
pub fn package_source(kind: DriverKind) -> Option<&'static str> {
    if embedded_package(kind).is_some() {
        Some("内置驱动包")
    } else if adjacent_package(kind).is_some() {
        Some("程序相邻的离线驱动包")
    } else {
        None
    }
}

pub fn install(kind: DriverKind) -> Result<String> {
    if let Some(bytes) = embedded_package(kind) {
        return install_reader(kind, Cursor::new(bytes));
    }
    if let Some(path) = adjacent_package(kind) {
        return install_from_zip(kind, &path);
    }
    bail!(
        "未找到 {}。请导入原厂官方 ZIP，或将它放入程序旁的 drivers 目录。",
        kind.zip_name()
    );
}

/// Called only after an explicit user action; does not persist the chosen ZIP.
pub fn install_from_zip(kind: DriverKind, path: &Path) -> Result<String> {
    let file = File::open(path).with_context(|| format!("无法读取驱动包 {}", path.display()))?;
    let length = file.metadata()?.len();
    ensure!(
        length > 0 && length <= MAX_ZIP_BYTES,
        "驱动 ZIP 为空或超过 64 MiB 限制"
    );
    install_reader(kind, file)
}

fn install_reader(kind: DriverKind, reader: impl Read + Seek) -> Result<String> {
    let directory = tempfile::Builder::new()
        .prefix("esp32c3-driver-")
        .tempdir()
        .context("无法创建驱动临时目录")?;
    let inf = unpack_and_validate(kind, reader, directory.path())?;
    // Keep the complete package (including catalogs, SYS files and licences)
    // alive until the elevated installation process has actually exited.
    run_installer(&inf)
}

fn safe_relative_path(name: &str) -> Result<PathBuf> {
    let normalized = name.replace('\\', "/");
    ensure!(!normalized.starts_with('/'), "驱动 ZIP 包含绝对路径");
    ensure!(
        !normalized.contains(':') && !normalized.contains('"'),
        "驱动 ZIP 包含不安全文件名"
    );
    let mut result = PathBuf::new();
    for part in normalized.trim_end_matches('/').split('/') {
        ensure!(
            !part.is_empty() && part != "." && part != "..",
            "驱动 ZIP 包含目录穿越路径"
        );
        ensure!(
            !part.ends_with(['.', ' ']) && !part.chars().any(char::is_control),
            "驱动 ZIP 包含不安全文件名"
        );
        let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
        let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && matches!(stem.as_bytes()[3], b'1'..=b'9'));
        ensure!(!reserved, "驱动 ZIP 包含 Windows 保留文件名");
        result.push(part);
    }
    Ok(result)
}

fn unpack_and_validate(kind: DriverKind, reader: impl Read + Seek, root: &Path) -> Result<PathBuf> {
    let mut archive = zip::ZipArchive::new(reader).context("驱动文件不是有效 ZIP")?;
    ensure!(
        !archive.is_empty() && archive.len() <= MAX_ENTRIES,
        "驱动 ZIP 的文件数量异常"
    );
    let mut expanded = 0_u64;
    let mut seen = HashSet::new();
    let mut infs = Vec::new();
    let mut system_files = 0;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).context("无法读取 ZIP 条目")?;
        let path = safe_relative_path(entry.name())?;
        ensure!(
            seen.insert(path.to_string_lossy().to_lowercase()),
            "驱动 ZIP 包含重复文件路径"
        );
        let mode = entry.unix_mode().unwrap_or(0) & 0o170000;
        ensure!(
            mode == 0 || mode == 0o100000 || mode == 0o040000,
            "驱动 ZIP 包含符号链接或特殊文件"
        );
        expanded = expanded
            .checked_add(entry.size())
            .context("驱动包大小溢出")?;
        ensure!(
            entry.size() <= MAX_ENTRY_BYTES && expanded <= MAX_EXPANDED_BYTES,
            "驱动包解压大小超过限制"
        );
        let destination = root.join(&path);
        if entry.is_dir() {
            fs::create_dir_all(&destination)?;
            continue;
        }
        fs::create_dir_all(destination.parent().context("驱动文件缺少父目录")?)?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination)?;
        let expected_length = entry.size();
        let copied = io::copy(&mut (&mut entry).take(MAX_ENTRY_BYTES + 1), &mut output)
            .context("驱动 ZIP 解压失败，CRC 或数据可能损坏")?;
        ensure!(
            copied == expected_length && copied <= MAX_ENTRY_BYTES,
            "驱动文件大小不匹配"
        );
        output.flush()?;
        let filename = path
            .file_name()
            .context("驱动条目缺少文件名")?
            .to_string_lossy();
        if filename.eq_ignore_ascii_case(kind.inf_name()) {
            infs.push(destination.clone());
        }
        if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("sys"))
            && copied > 0
        {
            system_files += 1;
        }
    }
    ensure!(
        infs.len() == 1,
        "驱动包必须包含且仅包含一个 {}，当前找到 {} 个",
        kind.inf_name(),
        infs.len()
    );
    ensure!(system_files > 0, "驱动 ZIP 缺少非空 SYS 驱动文件");
    let inf = infs.pop().unwrap();
    let text = read_inf(&fs::read(&inf)?);
    validate_inf(kind, &text, inf.parent().unwrap())?;
    Ok(inf)
}

fn read_inf(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xff, 0xfe]) {
        let words = bytes[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect::<Vec<_>>();
        String::from_utf16_lossy(&words)
    } else if bytes.starts_with(&[0xfe, 0xff]) {
        let words = bytes[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect::<Vec<_>>();
        String::from_utf16_lossy(&words)
    } else {
        // Hardware IDs and CatalogFile keys are ASCII in UTF-8 and Windows ANSI INFs.
        String::from_utf8_lossy(bytes).into_owned()
    }
}

fn validate_inf(kind: DriverKind, text: &str, directory: &Path) -> Result<()> {
    let active_lines = text
        .lines()
        .map(|line| line.split(';').next().unwrap_or("").trim())
        .collect::<Vec<_>>();
    let has_id = active_lines.iter().any(|line| {
        let uppercase = line.to_ascii_uppercase();
        // IDs must occur in a model assignment, not just in an INF comment.
        uppercase.contains('=')
            && kind
                .expected_hardware_ids()
                .iter()
                .any(|id| uppercase.contains(id))
    });
    ensure!(has_id, "驱动 INF 未包含所选芯片的预期 USB VID/PID");
    let catalogs = active_lines
        .iter()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            let key = key.trim().to_ascii_uppercase();
            if key == "CATALOGFILE" || key.starts_with("CATALOGFILE.") {
                Some(value.trim().trim_matches('"'))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    ensure!(!catalogs.is_empty(), "驱动 INF 未引用签名 CAT 目录文件");
    for name in catalogs {
        let relative = safe_relative_path(name)?;
        ensure!(
            relative.components().count() == 1
                && relative
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("cat")),
            "INF 中的 CAT 引用无效"
        );
        let path = fs::read_dir(directory)?
            .filter_map(std::result::Result::ok)
            .find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case(name))
            .map(|e| e.path())
            .with_context(|| format!("缺少 INF 引用的 CAT 文件：{name}"))?;
        let metadata = fs::metadata(&path)?;
        ensure!(
            metadata.is_file() && metadata.len() <= 8 * 1024 * 1024,
            "CAT 文件类型或大小异常"
        );
        let data = fs::read(path)?;
        ensure!(
            valid_catalog_envelope(&data),
            "CAT 文件不是有效的 PKCS#7 SignedData 结构"
        );
    }
    Ok(())
}

// A structural check only. Windows PnPUtil is the authority for signature,
// certificate trust and the catalog's binding to the complete driver package.
fn valid_catalog_envelope(bytes: &[u8]) -> bool {
    const SIGNED_DATA_OID: &[u8] = &[
        0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x07, 0x02,
    ];
    if bytes.len() < 2 || bytes[0] != 0x30 {
        return false;
    }
    let (header, length) = if bytes[1] & 0x80 == 0 {
        (2, usize::from(bytes[1]))
    } else {
        let count = usize::from(bytes[1] & 0x7f);
        if count == 0 || count > 4 || bytes.len() < 2 + count {
            return false;
        }
        let length = bytes[2..2 + count]
            .iter()
            .fold(0_usize, |value, byte| (value << 8) | usize::from(*byte));
        (2 + count, length)
    };
    header.checked_add(length) == Some(bytes.len()) && bytes[header..].starts_with(SIGNED_DATA_OID)
}

#[cfg(not(windows))]
fn run_installer(_inf: &Path) -> Result<String> {
    bail!("驱动安装仅支持 Windows；当前系统不会安装或修改任何驱动。")
}

#[cfg(windows)]
fn run_installer(inf: &Path) -> Result<String> {
    use std::{ffi::OsStr, mem, os::windows::ffi::OsStrExt};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_CANCELLED, GetLastError, HANDLE, WAIT_OBJECT_0},
        System::{
            SystemInformation::GetSystemDirectoryW,
            Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject},
        },
        UI::{
            Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW},
            WindowsAndMessaging::SW_SHOWNORMAL,
        },
    };

    fn wide(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(Some(0)).collect()
    }
    struct ProcessHandle(HANDLE);
    impl Drop for ProcessHandle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    let mut system = vec![0_u16; 32768];
    let length = unsafe { GetSystemDirectoryW(system.as_mut_ptr(), system.len() as u32) } as usize;
    ensure!(
        length > 0 && length < system.len(),
        "无法定位 Windows 系统目录"
    );
    let mut executable = system[..length].to_vec();
    executable.extend("\\pnputil.exe".encode_utf16());
    executable.push(0);
    let verb = wide(OsStr::new("runas"));
    // ShellExecute starts a known system executable directly; no command shell,
    // PowerShell or downloaded executable interprets this parameter string.
    let mut parameters = "/add-driver \"".encode_utf16().collect::<Vec<_>>();
    parameters.extend(inf.as_os_str().encode_wide());
    parameters.extend("\" /install".encode_utf16());
    parameters.push(0);
    let working_directory = wide(inf.parent().context("INF 缺少父目录")?.as_os_str());
    let mut info: SHELLEXECUTEINFOW = unsafe { mem::zeroed() };
    info.cbSize = mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS;
    info.lpVerb = verb.as_ptr();
    info.lpFile = executable.as_ptr();
    info.lpParameters = parameters.as_ptr();
    info.lpDirectory = working_directory.as_ptr();
    info.nShow = SW_SHOWNORMAL;
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        let code = unsafe { GetLastError() };
        if code == ERROR_CANCELLED {
            bail!("已取消管理员权限授权，未执行驱动安装。");
        }
        bail!(
            "无法启动驱动安装：{}",
            io::Error::from_raw_os_error(code as i32)
        );
    }
    ensure!(
        !info.hProcess.is_null(),
        "驱动安装程序未返回进程句柄，无法确认结果"
    );
    let process = ProcessHandle(info.hProcess);
    let wait = unsafe { WaitForSingleObject(process.0, INFINITE) };
    if wait != WAIT_OBJECT_0 {
        return Err(io::Error::last_os_error()).context("等待驱动安装结果失败");
    }
    let mut code = 0;
    if unsafe { GetExitCodeProcess(process.0, &mut code) } == 0 {
        return Err(io::Error::last_os_error()).context("无法读取驱动安装退出码");
    }
    match code {
        0 => Ok(
            "驱动包已由 Windows 安装/更新。请刷新串口列表；如设备未出现，请重新插拔 USB。".into(),
        ),
        3010 => Ok("驱动安装成功，需要重新启动 Windows 后生效（退出码 3010）。".into()),
        _ => bail!("Windows 驱动安装失败（退出码 {code}）。请检查安装窗口和系统驱动签名提示。"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_catalog() -> Vec<u8> {
        vec![
            0x30, 0x0b, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x07, 0x02,
        ]
    }

    fn package(inf: &str, catalog: &[u8]) -> Vec<u8> {
        let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        for (name, bytes) in [
            ("package/silabser.inf", inf.as_bytes()),
            ("package/silabser.cat", catalog),
            (
                "package/x64/silabser.sys",
                b"test fixture, never installed".as_slice(),
            ),
        ] {
            archive.start_file(name, options).unwrap();
            archive.write_all(bytes).unwrap();
        }
        archive.finish().unwrap().into_inner()
    }

    #[test]
    fn rejects_windows_and_zip_traversal_paths() {
        for bad in [
            "../driver.inf",
            "/driver.inf",
            "C:/driver.inf",
            "a\\..\\driver.inf",
            "driver.inf:evil",
            "a/CON.inf",
            "a/foo. /b",
            "a//b",
        ] {
            assert!(safe_relative_path(bad).is_err(), "accepted {bad}");
        }
        assert_eq!(
            safe_relative_path("CH343SER/Driver/CH343SER.INF").unwrap(),
            PathBuf::from("CH343SER/Driver/CH343SER.INF")
        );
    }

    #[test]
    fn checks_model_and_catalog_before_installation() {
        let inf = "[Version]\nCatalogFile=silabser.cat\n[Models]\n%device%=Install, USB\\VID_10C4&PID_EA60\n";
        let directory = tempfile::tempdir().unwrap();
        let zip = package(inf, &synthetic_catalog());
        let path =
            unpack_and_validate(DriverKind::Cp210x, Cursor::new(zip), directory.path()).unwrap();
        assert!(path.ends_with("package/silabser.inf"));
        assert!(directory.path().join("package/x64/silabser.sys").is_file());
        // The fixture checks envelope shape only; Windows still verifies trust.
        assert!(validate_inf(DriverKind::Ch343, inf, path.parent().unwrap()).is_err());
    }

    #[test]
    fn refuses_missing_or_invalid_catalog_and_comment_only_ids() {
        let directory = tempfile::tempdir().unwrap();
        let inf = "CatalogFile=silabser.cat\nD=Install,USB\\VID_10C4&PID_EA60\n";
        assert!(validate_inf(DriverKind::Cp210x, inf, directory.path()).is_err());
        fs::write(
            directory.path().join("silabser.cat"),
            b"not a signed catalog",
        )
        .unwrap();
        assert!(validate_inf(DriverKind::Cp210x, inf, directory.path()).is_err());
        fs::write(directory.path().join("silabser.cat"), synthetic_catalog()).unwrap();
        assert!(
            validate_inf(
                DriverKind::Cp210x,
                "; D=Install,USB\\VID_10C4&PID_EA60\nCatalogFile=silabser.cat",
                directory.path()
            )
            .is_err()
        );
    }

    #[test]
    fn reads_utf16_vendor_inf() {
        let text = "USB\\VID_10C4&PID_EA60";
        let mut bytes = vec![0xff, 0xfe];
        for word in text.encode_utf16() {
            bytes.extend(word.to_le_bytes());
        }
        assert_eq!(read_inf(&bytes), text);
    }
}
