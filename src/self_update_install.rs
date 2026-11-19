//! Private, non-elevated Windows update helper. No network operations occur here.
use anyhow::{Context, Result, bail, ensure};
#[cfg(windows)]
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[cfg(any(windows, test))]
use std::fs::OpenOptions;
#[cfg(windows)]
use std::{
    collections::HashSet,
    process::{Child, Command},
    thread,
    time::{Duration, Instant},
};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};

const MANIFEST_NAME: &str = "install.json";
#[cfg(windows)]
const HELPER_NAME: &str = "helper.exe";
#[cfg(windows)]
const READY_NAME: &str = "helper-ready.json";
const STARTUP_NAME: &str = "startup-ready.json";
#[cfg(windows)]
const ERROR_NAME: &str = "install-error.txt";
#[cfg(windows)]
const SUCCESS_NAME: &str = "install-success.json";
const MAX_MANIFEST: u64 = 1024 * 1024;
#[cfg(windows)]
const MAX_FILES: usize = 8192;
#[cfg(windows)]
const MAX_BYTES: u64 = 512 * 1024 * 1024;
const ENV_MANIFEST: &str = "FLASHTOOL_UPDATE_MANIFEST";
const ENV_SESSION: &str = "FLASHTOOL_UPDATE_SESSION";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateFile {
    pub relative_path: String,
    pub sha256: String,
}

#[derive(Clone, Debug)]
pub struct PreparedUpdate {
    pub version: String,
    pub stage_dir: PathBuf,
    pub extracted_dir: PathBuf,
    pub executable_path: PathBuf,
    pub executable_sha256: String,
    pub files: Vec<UpdateFile>,
    pub helper_path: PathBuf,
    pub manifest_path: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: u32,
    version: String,
    stage_dir: PathBuf,
    extracted_dir: PathBuf,
    target_executable: PathBuf,
    helper_path: PathBuf,
    parent_pid: u32,
    old_sha256: String,
    new_sha256: String,
    session: String,
    files: Vec<UpdateFile>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ticket {
    session: String,
    pid: u32,
    executable_sha256: String,
}

fn hash_file(path: &Path) -> Result<String> {
    let mut input = File::open(path).with_context(|| format!("无法读取 {}", path.display()))?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

#[cfg(windows)]
fn valid_hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn no_links(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "更新路径必须是绝对路径");
    let mut current = PathBuf::new();
    for part in path.components() {
        ensure!(
            !matches!(
                part,
                std::path::Component::ParentDir | std::path::Component::CurDir
            ),
            "更新路径不可包含相对跳转"
        );
        current.push(part.as_os_str());
        // A verbatim Windows prefix (\\?\C: or \\?\UNC\server\share)
        // is not a queryable absolute root until RootDir has been appended.
        if matches!(part, std::path::Component::Prefix(_)) {
            continue;
        }
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).context("无法检查更新路径"),
        };
        ensure!(
            !metadata.file_type().is_symlink(),
            "更新路径不能包含符号链接"
        );
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            ensure!(
                metadata.file_attributes() & 0x400 == 0,
                "更新路径不能包含重解析点"
            );
        }
    }
    Ok(())
}

fn canonical(path: &Path) -> Result<PathBuf> {
    no_links(path)?;
    let result = fs::canonicalize(path).context("无法解析更新路径")?;
    no_links(&result)?;
    Ok(result)
}

#[cfg(any(windows, test))]
fn relative_file(name: &str) -> Result<PathBuf> {
    ensure!(
        !name.is_empty()
            && !name.contains(['\\', ':', '\0', '<', '>', '"', '|', '?', '*'])
            && !name.starts_with('/'),
        "更新文件路径无效"
    );
    let mut path = PathBuf::new();
    for part in name.split('/') {
        ensure!(
            !part.is_empty()
                && part != "."
                && part != ".."
                && !part.ends_with(['.', ' '])
                && !part.chars().any(char::is_control),
            "更新文件路径无效"
        );
        let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
        ensure!(
            !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                && !(stem.len() == 4
                    && (stem.starts_with("COM") || stem.starts_with("LPT"))
                    && matches!(stem.as_bytes()[3], b'1'..=b'9')),
            "更新文件使用了保留名称"
        );
        path.push(part);
    }
    ensure!(
        crate::self_update::allowed_package_file(name),
        "更新包包含非白名单文件：{name}"
    );
    Ok(path)
}

fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("标记文件没有父目录")?;
    no_links(parent)?;
    ensure!(!path.exists(), "更新标记已存在，拒绝重复使用旧会话");
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(file.as_file_mut(), value)?;
    file.write_all(b"\n")?;
    file.as_file().sync_all()?;
    file.persist_noclobber(path).map_err(|error| error.error)?;
    Ok(())
}

fn read_manifest(path: &Path) -> Result<Manifest> {
    no_links(path)?;
    let file = File::open(path).context("无法读取更新清单")?;
    ensure!(file.metadata()?.len() <= MAX_MANIFEST, "更新清单过大");
    serde_json::from_reader(file.take(MAX_MANIFEST + 1)).context("更新清单格式无效")
}

#[cfg(windows)]
fn validate_manifest(manifest: &Manifest, path: &Path) -> Result<()> {
    ensure!(
        manifest.schema == 1 && manifest.parent_pid != 0,
        "更新清单版本或父进程无效"
    );
    ensure!(
        manifest.session.len() >= 20 && manifest.session.bytes().all(|b| b.is_ascii_alphanumeric()),
        "更新会话无效"
    );
    let version = Version::parse(&manifest.version)?;
    ensure!(
        version.pre.is_empty()
            && version.build.is_empty()
            && version.to_string() == manifest.version
            && version > Version::parse(env!("CARGO_PKG_VERSION"))?,
        "更新版本必须是更高的正式版本"
    );
    ensure!(
        valid_hash(&manifest.old_sha256) && valid_hash(&manifest.new_sha256),
        "更新校验值无效"
    );
    let stage = canonical(&manifest.stage_dir)?;
    let extracted = canonical(&manifest.extracted_dir)?;
    ensure!(
        extracted.starts_with(&stage) && extracted != stage,
        "解压目录必须位于本次暂存目录内"
    );
    ensure!(
        canonical(path)? == stage.join(MANIFEST_NAME),
        "更新清单不属于本次暂存目录"
    );
    ensure!(
        canonical(&manifest.helper_path)? == stage.join(HELPER_NAME),
        "更新辅助程序路径无效"
    );
    let target = canonical(&manifest.target_executable)?;
    ensure!(
        !target.starts_with(&stage)
            && target
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("exe")),
        "目标程序不能位于更新暂存目录内"
    );
    ensure!(
        hash_file(&manifest.helper_path)? == manifest.old_sha256,
        "辅助程序与原应用校验值不符"
    );
    ensure!(
        !manifest.files.is_empty() && manifest.files.len() <= MAX_FILES,
        "更新文件清单数量无效"
    );
    let mut names = HashSet::new();
    let mut total = 0u64;
    let mut executable = false;
    for file in &manifest.files {
        let relative = relative_file(&file.relative_path)?;
        ensure!(
            names.insert(file.relative_path.to_lowercase()) && valid_hash(&file.sha256),
            "更新文件名重复或校验值无效"
        );
        let source = extracted.join(relative);
        no_links(&source)?;
        let metadata = fs::metadata(&source)?;
        ensure!(metadata.is_file(), "更新清单包含非文件");
        total = total
            .checked_add(metadata.len())
            .context("更新文件大小溢出")?;
        ensure!(
            total <= MAX_BYTES && hash_file(&source)? == file.sha256,
            "更新文件过大或 SHA256 不匹配：{}",
            file.relative_path
        );
        if file.relative_path == "FlashTool.exe" {
            ensure!(
                file.sha256 == manifest.new_sha256,
                "主程序校验值与清单不一致"
            );
            crate::self_update::validate_windows_executable(&source, &manifest.version)?;
            executable = true;
        }
    }
    ensure!(executable, "更新清单缺少 FlashTool.exe");
    Ok(())
}

/// Download worker preparation only: no helper or application is executed.
pub fn prepare_installer(
    version: &str,
    stage_dir: &Path,
    extracted_dir: &Path,
    executable_sha256: &str,
    files: Vec<UpdateFile>,
) -> Result<PreparedUpdate> {
    #[cfg(not(windows))]
    {
        let _ = (version, stage_dir, extracted_dir, executable_sha256, files);
        bail!("自动安装仅支持 Windows");
    }
    #[cfg(windows)]
    {
        let stage = canonical(stage_dir)?;
        let extracted = canonical(extracted_dir)?;
        let target = canonical(&std::env::current_exe()?)?;
        let helper_path = stage.join(HELPER_NAME);
        let manifest_path = stage.join(MANIFEST_NAME);
        for name in [
            HELPER_NAME,
            MANIFEST_NAME,
            READY_NAME,
            STARTUP_NAME,
            ERROR_NAME,
            SUCCESS_NAME,
            "install.log",
        ] {
            ensure!(
                !stage.join(name).exists(),
                "暂存目录已有旧更新会话，请重新下载"
            );
        }
        let marker = tempfile::Builder::new()
            .prefix("")
            .rand_bytes(32)
            .tempfile_in(&stage)?;
        let session = marker
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        drop(marker);
        let old_sha256 = hash_file(&target)?;
        let mut source = File::open(&target)?;
        let mut helper = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&helper_path)?;
        std::io::copy(&mut source, &mut helper)?;
        helper.sync_all()?;
        drop(helper);
        let manifest = Manifest {
            schema: 1,
            version: version.into(),
            stage_dir: stage.clone(),
            extracted_dir: extracted.clone(),
            target_executable: target,
            helper_path: helper_path.clone(),
            parent_pid: std::process::id(),
            old_sha256,
            new_sha256: executable_sha256.into(),
            session,
            files: files.clone(),
        };
        atomic_json(&manifest_path, &manifest)?;
        validate_manifest(&manifest, &manifest_path)?;
        Ok(PreparedUpdate {
            version: version.into(),
            stage_dir: stage,
            executable_path: extracted.join("FlashTool.exe"),
            extracted_dir: extracted,
            executable_sha256: executable_sha256.into(),
            files,
            helper_path,
            manifest_path,
        })
    }
}

#[cfg(windows)]
fn ticket_matches(path: &Path, manifest: &Manifest, pid: u32, hash: &str) -> bool {
    if no_links(path).is_err() {
        return false;
    }
    let Ok(file) = File::open(path) else {
        return false;
    };
    if !file
        .metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() <= 4096)
    {
        return false;
    }
    let mut bytes = Vec::new();
    if file.take(4097).read_to_end(&mut bytes).is_err() || bytes.len() > 4096 {
        return false;
    }
    let Ok(ticket) = serde_json::from_slice::<Ticket>(&bytes) else {
        return false;
    };
    ticket.session == manifest.session && ticket.pid == pid && ticket.executable_sha256 == hash
}

/// Wait for the helper's verified ready acknowledgement before the old UI exits.
pub fn launch_installer(prepared: &PreparedUpdate) -> Result<()> {
    #[cfg(not(windows))]
    {
        let _ = prepared;
        bail!("自动安装仅支持 Windows");
    }
    #[cfg(windows)]
    {
        let manifest = read_manifest(&prepared.manifest_path)?;
        validate_manifest(&manifest, &prepared.manifest_path)?;
        ensure!(
            canonical(&std::env::current_exe()?)? == manifest.target_executable
                && std::process::id() == manifest.parent_pid,
            "更新会话不属于当前应用进程"
        );
        for name in [READY_NAME, STARTUP_NAME, ERROR_NAME, SUCCESS_NAME] {
            let marker = manifest.stage_dir.join(name);
            match fs::symlink_metadata(&marker) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                _ => bail!("此更新会话已使用，请重新下载后重试"),
            }
        }
        let mut helper = Command::new(&manifest.helper_path)
            .arg("--apply-update")
            .arg(&prepared.manifest_path)
            .spawn()
            .context("无法启动更新辅助程序")?;
        let result = (|| -> Result<()> {
            let deadline = Instant::now() + Duration::from_secs(15);
            while Instant::now() < deadline {
                let ready = ticket_matches(
                    &manifest.stage_dir.join(READY_NAME),
                    &manifest,
                    helper.id(),
                    &manifest.old_sha256,
                );
                let error_path = manifest.stage_dir.join(ERROR_NAME);
                if no_links(&error_path).is_ok()
                    && let Ok(file) = File::open(error_path)
                {
                    let mut message = String::new();
                    file.take(8192).read_to_string(&mut message)?;
                    bail!("更新辅助程序未就绪：{message}");
                }
                if let Some(status) = helper.try_wait()? {
                    bail!("更新辅助程序提前退出：{status}");
                }
                if ready {
                    return Ok(());
                }
                thread::sleep(Duration::from_millis(50));
            }
            bail!("更新辅助程序准备超时，当前应用保持运行")
        })();
        if result.is_err() {
            // Never leave an unacknowledged helper running after launch fails.
            // The old parent is still alive; only this spawned helper is ours.
            let _ = helper.kill();
            let _ = helper.wait();
        }
        result
    }
}

/// Called after the new GUI is initialized, not merely after process creation.
pub fn notify_startup_ready() -> Result<bool> {
    let Some(path) = std::env::var_os(ENV_MANIFEST) else {
        return Ok(false);
    };
    let session = std::env::var(ENV_SESSION).context("缺少更新启动会话")?;
    let path = PathBuf::from(path);
    let manifest = read_manifest(&path)?;
    // The new binary has a new package version, so validate paths and bytes
    // here without applying the old helper's higher-version comparison.
    ensure!(
        manifest.schema == 1 && session == manifest.session,
        "启动确认会话无效"
    );
    let current = canonical(&std::env::current_exe()?)?;
    ensure!(
        current == canonical(&manifest.target_executable)?
            && hash_file(&current)? == manifest.new_sha256,
        "启动程序不属于本次更新"
    );
    ensure!(
        canonical(&path)? == canonical(&manifest.stage_dir)?.join(MANIFEST_NAME),
        "启动确认路径无效"
    );
    atomic_json(
        &manifest.stage_dir.join(STARTUP_NAME),
        &Ticket {
            session,
            pid: std::process::id(),
            executable_sha256: manifest.new_sha256,
        },
    )?;
    Ok(true)
}

/// Reserved helper command is consumed before the application's ordinary CLI.
pub fn run_if_requested() -> Option<i32> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    if arguments
        .first()
        .is_none_or(|value| value != "--apply-update")
    {
        return None;
    }
    if arguments.len() != 2 {
        eprintln!("--apply-update 需要且只接受一个更新清单路径");
        return Some(2);
    }
    #[cfg(not(windows))]
    {
        eprintln!("更新安装辅助程序仅支持 Windows");
        Some(2)
    }
    #[cfg(windows)]
    {
        let path = PathBuf::from(&arguments[1]);
        Some(match apply_update(&path) {
            Ok(()) => 0,
            Err(error) => {
                record_error(&path, &error);
                1
            }
        })
    }
}

#[cfg(any(windows, test))]
fn target_path(manifest: &Manifest, file: &UpdateFile) -> Result<PathBuf> {
    if file.relative_path == "FlashTool.exe" {
        return Ok(manifest.target_executable.clone());
    }
    Ok(manifest
        .target_executable
        .parent()
        .context("目标程序没有父目录")?
        .join(relative_file(&file.relative_path)?))
}

#[cfg(windows)]
fn preflight(manifest: &Manifest) -> Result<()> {
    ensure!(
        hash_file(&manifest.target_executable)? == manifest.old_sha256,
        "原应用已变化，请重新检查更新"
    );
    for file in &manifest.files {
        let target = target_path(manifest, file)?;
        no_links(&target)?;
        match fs::metadata(&target) {
            Ok(metadata) => ensure!(
                metadata.is_file() && !metadata.permissions().readonly(),
                "安装目标只读或不是普通文件：{}",
                target.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("无法检查安装目标"),
        }
        let mut directory = target.parent().context("安装目标没有父目录")?;
        while !directory.exists() {
            directory = directory.parent().context("安装目标没有可写父目录")?;
        }
        no_links(directory)?;
        let probe = tempfile::NamedTempFile::new_in(directory)
            .context("安装目录不可写，请移动到当前用户可写的目录后更新")?;
        probe.as_file().sync_all()?;
    }
    let directory = manifest.target_executable.parent().unwrap();
    let probe = tempfile::NamedTempFile::new_in(directory)
        .context("应用目录不可写，请移动到当前用户可写的目录后更新")?;
    probe.as_file().sync_all()?;
    Ok(())
}

#[cfg(any(windows, test))]
#[derive(Serialize)]
struct Change {
    target: PathBuf,
    backup: Option<PathBuf>,
    original_sha256: Option<String>,
}

#[cfg(any(windows, test))]
fn replace_file(target: &Path, replacement: &Path, backup: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;
        let wide = |path: &Path| {
            path.as_os_str()
                .encode_wide()
                .chain(Some(0))
                .collect::<Vec<_>>()
        };
        let (target, replacement, backup) = (wide(target), wide(replacement), wide(backup));
        if unsafe {
            ReplaceFileW(
                target.as_ptr(),
                replacement.as_ptr(),
                backup.as_ptr(),
                0,
                std::ptr::null(),
                std::ptr::null(),
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error()).context("原子替换更新文件失败");
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        fs::rename(target, backup)?;
        if let Err(error) = fs::rename(replacement, target) {
            fs::rename(backup, target)?;
            return Err(error.into());
        }
        Ok(())
    }
}

#[cfg(any(windows, test))]
fn install_files(manifest: &Manifest, backup_dir: &Path, changes: &mut Vec<Change>) -> Result<()> {
    install_files_with(manifest, backup_dir, changes, replace_file)
}

#[cfg(any(windows, test))]
fn install_files_with(
    manifest: &Manifest,
    backup_dir: &Path,
    changes: &mut Vec<Change>,
    mut replace: impl FnMut(&Path, &Path, &Path) -> Result<()>,
) -> Result<()> {
    let mut files = manifest.files.clone();
    // Replace the executable last, after all allowed documentation is staged.
    files.sort_by_key(|file| file.relative_path == "FlashTool.exe");
    for (index, file) in files.iter().enumerate() {
        let source = manifest
            .extracted_dir
            .join(relative_file(&file.relative_path)?);
        let target = target_path(manifest, file)?;
        no_links(&source)?;
        no_links(&target)?;
        fs::create_dir_all(target.parent().unwrap())?;
        no_links(&target)?;
        let mut temporary = tempfile::NamedTempFile::new_in(target.parent().unwrap())?;
        let mut input = File::open(source)?;
        std::io::copy(&mut input, &mut temporary)?;
        temporary.as_file().sync_all()?;
        ensure!(
            hash_file(temporary.path())? == file.sha256,
            "暂存的新文件校验失败"
        );
        let replacement = temporary.into_temp_path();
        let backup = backup_dir.join(format!("{index:04}.bak"));
        if target.exists() {
            // ReplaceFileW can fail after creating a backup or moving a name.
            // Journal a separately verified old copy before the native call;
            // rollback must cover a false return with partial side effects too.
            let recovery = backup_dir.join(format!("{index:04}-original.bak"));
            let original_sha256 = hash_file(&target)?;
            let mut input = File::open(&target)?;
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&recovery)?;
            std::io::copy(&mut input, &mut output)?;
            output.sync_all()?;
            drop(output);
            ensure!(
                hash_file(&recovery)? == original_sha256,
                "旧文件恢复副本校验失败"
            );
            changes.push(Change {
                target: target.clone(),
                backup: Some(recovery),
                original_sha256: Some(original_sha256),
            });
            atomic_json(
                &backup_dir.join(format!("{index:04}-mapping.json")),
                changes.last().unwrap(),
            )?;
            replace(&target, &replacement, &backup)?;
        } else {
            replacement
                .persist_noclobber(&target)
                .map_err(|error| error.error)?;
            changes.push(Change {
                target,
                backup: None,
                original_sha256: None,
            });
        }
    }
    Ok(())
}

#[cfg(any(windows, test))]
fn rollback(changes: &[Change], backup_dir: &Path) -> Result<()> {
    for (index, change) in changes.iter().enumerate().rev() {
        no_links(&change.target)?;
        if let Some(backup) = &change.backup {
            no_links(backup)?;
            ensure!(backup.is_file(), "更新备份丢失：{}", backup.display());
            ensure!(
                Some(hash_file(backup)?) == change.original_sha256,
                "原文件恢复副本校验失败"
            );
            let mut restore = tempfile::NamedTempFile::new_in(change.target.parent().unwrap())?;
            std::io::copy(&mut File::open(backup)?, &mut restore)?;
            restore.as_file().sync_all()?;
            ensure!(
                Some(hash_file(restore.path())?) == change.original_sha256,
                "恢复暂存文件校验失败"
            );
            let restore = restore.into_temp_path();
            if change.target.exists() {
                replace_file(
                    &change.target,
                    &restore,
                    &backup_dir.join(format!("failed-{index:04}.bin")),
                )?;
            } else {
                restore
                    .persist_noclobber(&change.target)
                    .map_err(|error| error.error)?;
            }
        } else if change.target.exists() {
            fs::remove_file(&change.target)?;
        }
    }
    Ok(())
}

#[cfg(windows)]
fn spawn_application(manifest: &Manifest, update: bool) -> Result<Child> {
    let mut command = Command::new(&manifest.target_executable);
    command.current_dir(manifest.target_executable.parent().unwrap());
    if update {
        command
            .env(ENV_MANIFEST, manifest.stage_dir.join(MANIFEST_NAME))
            .env(ENV_SESSION, &manifest.session);
    } else {
        command.env_remove(ENV_MANIFEST).env_remove(ENV_SESSION);
    }
    command.spawn().context("无法重新启动应用")
}

#[cfg(windows)]
fn apply_update(path: &Path) -> Result<()> {
    let manifest = read_manifest(path)?;
    validate_manifest(&manifest, path)?;
    ensure!(
        canonical(&std::env::current_exe()?)? == manifest.helper_path,
        "安装入口必须来自本次暂存的应用副本"
    );
    let parent = ParentProcess::open(manifest.parent_pid, &manifest.target_executable)?;
    preflight(&manifest)?;
    log(&manifest, "辅助程序已校验，等待原应用安全退出");
    atomic_json(
        &manifest.stage_dir.join(READY_NAME),
        &Ticket {
            session: manifest.session.clone(),
            pid: std::process::id(),
            executable_sha256: manifest.old_sha256.clone(),
        },
    )?;
    parent.wait()?;
    // All work after the original UI exits belongs to the recovery transaction,
    // including preparation failures before the first file was changed.
    let mut backup = None;
    let mut changes = Vec::new();
    let mut child = None;
    let mut executable_guard = None;
    let installed = (|| -> Result<()> {
        validate_manifest(&manifest, path)?;
        preflight(&manifest)?;
        let directory = tempfile::Builder::new()
            .prefix(".FlashTool-update-backup-")
            .tempdir_in(manifest.target_executable.parent().unwrap())?
            .keep();
        log(&manifest, &format!("旧文件备份：{}", directory.display()));
        backup = Some(directory.clone());
        install_files(&manifest, &directory, &mut changes)?;
        // Prevent another process from writing or deleting the verified image
        // between the final hash check and the new GUI's acknowledgement.
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
        executable_guard = Some(
            OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ)
                .open(&manifest.target_executable)?,
        );
        ensure!(
            hash_file(&manifest.target_executable)? == manifest.new_sha256,
            "安装后的主程序校验失败"
        );
        child = Some(spawn_application(&manifest, true)?);
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let process = child.as_mut().unwrap();
            if ticket_matches(
                &manifest.stage_dir.join(STARTUP_NAME),
                &manifest,
                process.id(),
                &manifest.new_sha256,
            ) {
                log(&manifest, "新版界面初始化确认成功");
                return Ok(());
            }
            if let Some(status) = process.try_wait()? {
                bail!("新版在界面就绪前退出：{status}");
            }
            if Instant::now() >= deadline {
                bail!("新版界面启动确认超时，正在恢复旧版");
            }
            thread::sleep(Duration::from_millis(50));
        }
    })();
    if let Err(error) = installed {
        // Terminate only our own unacknowledged candidate, for every failure
        // after spawn (including failed status polling), then release its lock.
        if let Some(mut process) = child {
            let _ = process.kill();
            let _ = process.wait();
        }
        drop(executable_guard);
        log(&manifest, &format!("安装失败：{error:#}；正在回滚"));
        if let Some(directory) = &backup {
            rollback(&changes, directory).with_context(|| format!("回滚失败，请从 {} 的 *-mapping.json 与 *-original.bak 手动恢复原文件；原始错误：{error:#}", directory.display()))?;
        }
        no_links(&manifest.target_executable)?;
        ensure!(
            canonical(&manifest.target_executable)? == manifest.target_executable,
            "旧版路径已变化，拒绝重新启动未知程序"
        );
        ensure!(
            hash_file(&manifest.target_executable)? == manifest.old_sha256,
            "回滚后的程序校验失败"
        );
        spawn_application(&manifest, false)
            .context("已恢复旧版，但重新启动失败；请手动打开原应用")?;
        return Err(error).context("已恢复旧版应用");
    }
    // The new GUI is already initialized: a marker write failure is diagnostic,
    // and must not roll back a successfully running application.
    if let Err(error) = atomic_json(
        &manifest.stage_dir.join(SUCCESS_NAME),
        &Ticket {
            session: manifest.session.clone(),
            pid: std::process::id(),
            executable_sha256: manifest.new_sha256.clone(),
        },
    ) {
        log(
            &manifest,
            &format!("新版已就绪，但成功标记写入失败：{error:#}"),
        );
    }
    Ok(())
}

#[cfg(windows)]
struct ParentProcess(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl ParentProcess {
    fn open(pid: u32, target: &Path) -> Result<Self> {
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
            QueryFullProcessImageNameW,
        };
        let raw = unsafe {
            OpenProcess(
                PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                pid,
            )
        };
        ensure!(!raw.is_null(), "无法确认原应用进程，保持当前应用运行");
        let handle = Self(raw);
        let mut buffer = vec![0u16; 32768];
        let mut length = buffer.len() as u32;
        if unsafe { QueryFullProcessImageNameW(raw, 0, buffer.as_mut_ptr(), &mut length) } == 0 {
            return Err(std::io::Error::last_os_error()).context("无法读取原应用进程路径");
        }
        let image = PathBuf::from(String::from_utf16(&buffer[..length as usize])?);
        ensure!(
            canonical(&image)? == canonical(target)?,
            "更新父进程不是目标应用"
        );
        Ok(handle)
    }
    fn wait(&self) -> Result<()> {
        use windows_sys::Win32::{
            Foundation::WAIT_OBJECT_0, System::Threading::WaitForSingleObject,
        };
        let result = unsafe { WaitForSingleObject(self.0, 60_000) };
        ensure!(
            result == WAIT_OBJECT_0,
            "原应用在 60 秒内未退出，取消本次安装；不会强制关闭应用"
        );
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for ParentProcess {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
fn log(manifest: &Manifest, message: &str) {
    let path = manifest.stage_dir.join("install.log");
    if no_links(&path).is_err() {
        return;
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{message}");
        let _ = file.sync_all();
    }
}

#[cfg(windows)]
fn record_error(path: &Path, error: &anyhow::Error) {
    if let Ok(manifest) = read_manifest(path)
        && canonical(path).ok().as_ref() == Some(&manifest.stage_dir.join(MANIFEST_NAME))
        && no_links(&manifest.stage_dir).is_ok()
    {
        let message = format!("{error:#}");
        log(&manifest, &message);
        let error_path = manifest.stage_dir.join(ERROR_NAME);
        if no_links(&error_path).is_ok()
            && let Ok(mut file) = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(error_path)
        {
            let _ = file.write_all(message.as_bytes());
            let _ = file.sync_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_policy_preserves_real_license_names_and_blocks_unsafe_paths() {
        for name in [
            "FlashTool.exe",
            "LICENSE",
            "docs/software-updates.md",
            "drivers/README.md",
            "third-party-licenses/a/UNLICENSE",
            "third-party-licenses/a/LICENSE.APACHE",
            "third-party-licenses/a/LICENSE.MIT",
            "third-party-licenses/a/license-apache-2.0",
            "third-party-licenses/a/tests/fonts/colr_1_LICENSE",
        ] {
            assert!(relative_file(name).is_ok(), "{name}");
        }
        for name in [
            "../FlashTool.exe",
            "docs/../README.md",
            "docs//a.txt",
            "C:/FlashTool.exe",
            "docs/a:stream",
            "docs\\a.txt",
            "/README.md",
            "docs/CON.txt",
            "docs/a. ",
            "docs/helper.exe",
            "drivers/install.ps1",
            "docs/a?.md",
            "private.csv",
        ] {
            assert!(relative_file(name).is_err(), "{name}");
        }
    }

    fn fixture(root: &Path) -> Manifest {
        let stage = root.join("update staging 中文");
        let extracted = stage.join("extracted");
        let installed = root.join("portable app 中文");
        fs::create_dir_all(extracted.join("docs")).unwrap();
        fs::create_dir_all(&installed).unwrap();
        fs::write(installed.join("重命名 FlashTool.exe"), b"old executable").unwrap();
        fs::write(installed.join("README.md"), b"old readme").unwrap();
        fs::write(installed.join("results.csv"), b"private,untouched").unwrap();
        let mut files = Vec::new();
        for (name, bytes) in [
            ("FlashTool.exe", b"new executable".as_slice()),
            ("README.md", b"new readme".as_slice()),
            ("docs/new.md", b"new documentation".as_slice()),
        ] {
            let path = extracted.join(name);
            fs::write(&path, bytes).unwrap();
            files.push(UpdateFile {
                relative_path: name.into(),
                sha256: hash_file(&path).unwrap(),
            });
        }
        let target = installed.join("重命名 FlashTool.exe");
        Manifest {
            schema: 1,
            version: "99.0.0".into(),
            stage_dir: stage.clone(),
            extracted_dir: extracted,
            target_executable: target.clone(),
            helper_path: stage.join("helper.exe"),
            parent_pid: 1,
            old_sha256: hash_file(&target).unwrap(),
            new_sha256: files[0].sha256.clone(),
            session: "aSessionWithEnoughRandomCharacters123".into(),
            files,
        }
    }

    fn assert_old_files(manifest: &Manifest) {
        let directory = manifest.target_executable.parent().unwrap();
        assert_eq!(
            fs::read(&manifest.target_executable).unwrap(),
            b"old executable"
        );
        assert_eq!(
            fs::read(directory.join("README.md")).unwrap(),
            b"old readme"
        );
        assert_eq!(
            fs::read(directory.join("results.csv")).unwrap(),
            b"private,untouched"
        );
        assert!(!directory.join("docs/new.md").exists());
    }

    #[test]
    fn file_transaction_restores_renamed_exe_docs_and_preserves_private_csv() {
        let root = tempfile::tempdir().unwrap();
        let manifest = fixture(root.path());
        let backup = root.path().join("backups");
        fs::create_dir(&backup).unwrap();
        let mut changes = Vec::new();
        install_files(&manifest, &backup, &mut changes).unwrap();
        assert_eq!(
            fs::read(&manifest.target_executable).unwrap(),
            b"new executable"
        );
        assert_eq!(changes.len(), 3);
        rollback(&changes, &backup).unwrap();
        assert_old_files(&manifest);
        for change in changes.iter().filter(|change| change.backup.is_some()) {
            assert!(
                change.backup.as_ref().unwrap().is_file(),
                "recovery copies remain available"
            );
        }
    }

    #[test]
    fn partial_native_replace_failure_is_journaled_and_recoverable() {
        let root = tempfile::tempdir().unwrap();
        let manifest = fixture(root.path());
        let backup = root.path().join("backups");
        fs::create_dir(&backup).unwrap();
        let mut changes = Vec::new();
        let error = install_files_with(
            &manifest,
            &backup,
            &mut changes,
            |target, replacement, native_backup| {
                if target == manifest.target_executable {
                    // Model ReplaceFileW returning false after moving both names.
                    fs::rename(target, native_backup)?;
                    fs::rename(replacement, target)?;
                    bail!("injected partial native replacement failure");
                }
                replace_file(target, replacement, native_backup)
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("injected partial"));
        assert!(
            changes
                .iter()
                .any(|change| change.target == manifest.target_executable)
        );
        rollback(&changes, &backup).unwrap();
        assert_old_files(&manifest);
    }

    #[test]
    fn corrupted_recovery_copy_is_never_installed() {
        let root = tempfile::tempdir().unwrap();
        let manifest = fixture(root.path());
        let backup = root.path().join("backups");
        fs::create_dir(&backup).unwrap();
        let mut changes = Vec::new();
        install_files(&manifest, &backup, &mut changes).unwrap();
        let executable_change = changes
            .iter()
            .find(|change| change.target == manifest.target_executable)
            .unwrap();
        fs::write(
            executable_change.backup.as_ref().unwrap(),
            b"tampered recovery",
        )
        .unwrap();
        assert!(rollback(&changes, &backup).is_err());
        assert_eq!(
            fs::read(&manifest.target_executable).unwrap(),
            b"new executable"
        );
    }

    #[cfg(unix)]
    #[test]
    fn installation_refuses_symbolic_link_parent() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let manifest = fixture(root.path());
        let outside = root.path().join("outside");
        fs::create_dir(&outside).unwrap();
        symlink(
            &outside,
            manifest.target_executable.parent().unwrap().join("docs"),
        )
        .unwrap();
        let backup = root.path().join("backups");
        fs::create_dir(&backup).unwrap();
        let mut changes = Vec::new();
        assert!(install_files(&manifest, &backup, &mut changes).is_err());
        rollback(&changes, &backup).unwrap();
        assert!(!outside.join("new.md").exists());
        assert_eq!(
            fs::read(&manifest.target_executable).unwrap(),
            b"old executable"
        );
    }
}
