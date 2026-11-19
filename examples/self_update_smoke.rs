//! Windows-only, offline end-to-end exercise of the real portable updater.
//! Every executable that can be replaced is a private copy in a temporary tree.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(not(windows))]
fn main() {
    eprintln!("The self-update smoke must run on native Windows.");
    std::process::exit(1);
}

#[cfg(windows)]
fn main() {
    if let Some(code) = esp32c3_flashtool::self_update_install::run_if_requested() {
        std::process::exit(code);
    }
    if let Err(error) = windows::run() {
        eprintln!("Self-update smoke failed: {error:#}");
        std::process::exit(1);
    }
}

#[cfg(windows)]
#[path = "support/self_update_fixture.rs"]
mod fixture;

#[cfg(windows)]
mod windows {
    use super::fixture::{OwnedFixtureProcess, make_future_candidate};
    use anyhow::{Context, Result, bail, ensure};
    use esp32c3_flashtool::{self_update, self_update_install};
    use serde::{Deserialize, Serialize};
    use sha2::{Digest, Sha256};
    use std::{
        env,
        fs::{self, File},
        io::{Read, Write},
        path::{Path, PathBuf},
        process::{Child, Command},
        thread,
        time::{Duration, Instant},
    };
    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    const VERSION: &str = "99.0.0";
    const CONFIG: &str = "self-update-smoke.json";
    const PARENT_ARG: &str = "--self-update-smoke-parent";
    const ORIGINAL_DOC: &[u8] = b"Original disposable installation\n";
    const UPDATED_DOC: &[u8] = b"Updated disposable installation\n";

    #[derive(Clone, Copy, Debug, Deserialize, Serialize)]
    #[serde(rename_all = "snake_case")]
    enum Mode {
        Acknowledge,
        NoAcknowledge,
    }

    #[derive(Debug, Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct Case {
        directory: PathBuf,
        archive: PathBuf,
        checksums: String,
        original_sha256: String,
        candidate_sha256: String,
        mode: Mode,
    }

    #[derive(Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct ProcessMarker {
        pid: u32,
    }

    #[derive(Deserialize)]
    struct HelperTicket {
        pid: u32,
    }

    pub fn run() -> Result<()> {
        let executable = env::current_exe()?.canonicalize()?;
        let arguments: Vec<_> = env::args_os().skip(1).collect();
        if arguments.len() == 1 && arguments[0] == "--run" {
            ensure!(
                semver::Version::parse(VERSION)?
                    > semver::Version::parse(env!("CARGO_PKG_VERSION"))?,
                "Advance the smoke's future fixture version before testing this release"
            );
            return run_smoke(&executable);
        }
        let case = read_isolated_case(&executable)?;
        if arguments.len() == 1 && arguments[0] == PARENT_ARG {
            return run_parent(&case, &executable);
        }
        ensure!(arguments.is_empty(), "Unknown smoke invocation");
        run_installed_fixture(&case, &executable)
    }

    fn read_isolated_case(executable: &Path) -> Result<Case> {
        let path = executable
            .parent()
            .context("Fixture has no directory")?
            .join(CONFIG);
        ensure!(
            fs::metadata(&path)?.len() <= 32 * 1024,
            "Fixture configuration is too large"
        );
        let case: Case = serde_json::from_slice(&fs::read(path)?)?;
        let directory = case.directory.canonicalize()?;
        ensure!(
            executable == directory.join("app/FlashTool.exe").canonicalize()?,
            "Only the explicitly owned temporary executable may act as an installed fixture"
        );
        let local_data =
            PathBuf::from(env::var_os("LOCALAPPDATA").context("Missing isolated LOCALAPPDATA")?)
                .canonicalize()?;
        ensure!(
            local_data == directory.join("data").canonicalize()?,
            "Fixture LOCALAPPDATA is not isolated"
        );
        for name in ["TEMP", "TMP"] {
            ensure!(
                PathBuf::from(env::var_os(name).context("Missing isolated temporary directory")?)
                    .canonicalize()?
                    == directory.join("temp").canonicalize()?,
                "Fixture temporary directory is not isolated"
            );
        }
        Ok(case)
    }

    fn stage(case: &Case) -> PathBuf {
        case.directory.join("data/FlashTool/updates/smoke")
    }

    fn run_parent(case: &Case, executable: &Path) -> Result<()> {
        ensure!(
            digest(executable)? == case.original_sha256,
            "Parent executable changed before preparation"
        );
        let stage = stage(case);
        fs::create_dir_all(&stage)?;
        let extracted = stage.join("extracted");
        // Exercise the actual checksum, ZIP policy, PE and VERSIONINFO checks.
        let verified = self_update::verify_and_extract_package(
            VERSION,
            &case.archive,
            case.checksums.as_bytes(),
            &extracted,
        )?;
        ensure!(
            verified.executable_sha256 == case.candidate_sha256,
            "Verified candidate digest changed"
        );
        let prepared = self_update_install::prepare_installer(
            VERSION,
            &stage,
            &extracted,
            &verified.executable_sha256,
            verified.files,
        )?;
        self_update_install::launch_installer(&prepared)?;
        write_json(
            &case.directory.join("helper-ready-observed.json"),
            &ProcessMarker {
                pid: std::process::id(),
            },
        )?;
        // Hold the real parent process open after the authenticated handshake.
        // The controlling harness proves replacement has not started yet.
        wait_until(
            "permission for the disposable parent to exit",
            Duration::from_secs(15),
            || Ok(case.directory.join("allow-parent-exit").is_file()),
        )?;
        ensure!(
            digest(executable)? == case.original_sha256,
            "Helper replaced the still-running parent"
        );
        Ok(())
    }

    fn run_installed_fixture(case: &Case, executable: &Path) -> Result<()> {
        let hash = digest(executable)?;
        let has_manifest = env::var_os("FLASHTOOL_UPDATE_MANIFEST").is_some();
        if hash == case.original_sha256 {
            ensure!(
                !has_manifest && env::var_os("FLASHTOOL_UPDATE_SESSION").is_none(),
                "Rollback restart retained startup credentials"
            );
            write_json(
                &case.directory.join("rollback-restarted.json"),
                &ProcessMarker {
                    pid: std::process::id(),
                },
            )?;
            return Ok(());
        }
        ensure!(
            hash == case.candidate_sha256 && has_manifest,
            "Unrecognized candidate executable or missing session"
        );
        write_json(
            &case.directory.join("candidate-started.json"),
            &ProcessMarker {
                pid: std::process::id(),
            },
        )?;
        match case.mode {
            Mode::Acknowledge => {
                ensure!(
                    self_update_install::notify_startup_ready()?,
                    "Production startup ACK was not accepted"
                );
                fs::write(
                    case.directory.join("candidate-acknowledged"),
                    b"production notify_startup_ready returned true\n",
                )?;
                wait_until(
                    "permission for the acknowledged candidate to exit",
                    Duration::from_secs(30),
                    || Ok(case.directory.join("allow-candidate-exit").is_file()),
                )?;
            }
            Mode::NoAcknowledge => {
                // A healthy helper must terminate only this child, restore the
                // original bytes, and restart the old image after its deadline.
                thread::sleep(Duration::from_secs(45));
                bail!("Unacknowledged candidate was not terminated by the update helper");
            }
        }
        Ok(())
    }

    fn run_smoke(executable: &Path) -> Result<()> {
        let root = tempfile::Builder::new()
            .prefix("flashtool-self-update-smoke-")
            .tempdir()?;
        let result: Result<()> = (|| {
            run_case(
                executable,
                &root.path().join("acknowledged"),
                Mode::Acknowledge,
            )?;
            run_case(
                executable,
                &root.path().join("missing-ack"),
                Mode::NoAcknowledge,
            )?;
            Ok(())
        })();
        if let Err(error) = result {
            let retained = root.keep();
            return Err(error).with_context(|| {
                format!(
                    "Isolated fixtures and helper logs retained at {}",
                    retained.display()
                )
            });
        }
        println!(
            "Self-update smoke passed: authenticated readiness, parent exit, replacement, real startup ACK, exact rollback and old-image restart."
        );
        Ok(())
    }

    fn run_case(executable: &Path, directory: &Path, mode: Mode) -> Result<()> {
        for child in ["app", "data", "temp", "source"] {
            fs::create_dir_all(directory.join(child))?;
        }
        let directory = directory.canonicalize()?;
        let target = directory.join("app/FlashTool.exe");
        let candidate = directory.join("source/FlashTool.exe");
        fs::copy(executable, &target)?;
        fs::copy(executable, &candidate)?;
        make_future_candidate(&candidate, (99, 0, 0))?;
        let original_sha256 = digest(&target)?;
        let candidate_sha256 = digest(&candidate)?;
        ensure!(
            original_sha256 != candidate_sha256,
            "Candidate must differ from the original image"
        );
        fs::write(directory.join("app/README.md"), ORIGINAL_DOC)?;
        let archive = directory.join(format!("source/FlashTool-v{VERSION}-windows-x64.zip"));
        make_package(&archive, &candidate)?;
        let checksums = format!(
            "{}  FlashTool-v{VERSION}-windows-x64.zip\n",
            digest(&archive)?
        );
        let case = Case {
            directory: directory.clone(),
            archive,
            checksums,
            original_sha256,
            candidate_sha256,
            mode,
        };
        write_json(&directory.join("app").join(CONFIG), &case)?;
        let mut parent = OwnedChild(
            Command::new(&target)
                .arg(PARENT_ARG)
                .current_dir(directory.join("app"))
                .env("LOCALAPPDATA", directory.join("data"))
                .env("TEMP", directory.join("temp"))
                .env("TMP", directory.join("temp"))
                .env_remove("FLASHTOOL_UPDATE_MANIFEST")
                .env_remove("FLASHTOOL_UPDATE_SESSION")
                .spawn()
                .context("Start the disposable parent")?,
        );

        wait_until(
            "authenticated helper readiness",
            Duration::from_secs(25),
            || {
                if let Some(status) = parent.0.try_wait()? {
                    bail!("Disposable parent exited before readiness: {status}");
                }
                Ok(directory.join("helper-ready-observed.json").is_file())
            },
        )?;
        ensure!(
            digest(&target)? == case.original_sha256
                && !directory.join("candidate-started.json").exists(),
            "Replacement started before the parent exited"
        );
        let stage = stage(&case);
        let helper_ticket: HelperTicket = read_json(&stage.join("helper-ready.json"))?;
        let helper_process =
            OwnedFixtureProcess::open(helper_ticket.pid, &stage.join("helper.exe"))?;
        fs::write(directory.join("allow-parent-exit"), b"exit\n")?;
        wait_until(
            "disposable parent exit",
            Duration::from_secs(10),
            || match parent.0.try_wait()? {
                Some(status) => {
                    ensure!(status.success(), "Disposable parent failed: {status}");
                    Ok(true)
                }
                None => Ok(false),
            },
        )?;

        wait_until("candidate process startup", Duration::from_secs(15), || {
            Ok(directory.join("candidate-started.json").is_file())
        })?;
        let marker: ProcessMarker = read_json(&directory.join("candidate-started.json"))?;
        let candidate_process = OwnedFixtureProcess::open(marker.pid, &target)?;
        match mode {
            Mode::Acknowledge => {
                wait_until(
                    "successful installation and startup ACK",
                    Duration::from_secs(15),
                    || {
                        Ok(stage.join("install-success.json").is_file()
                            && directory.join("candidate-acknowledged").is_file()
                            && digest(&target)? == case.candidate_sha256)
                    },
                )?;
                ensure!(
                    !directory.join("rollback-restarted.json").exists(),
                    "Successful update unexpectedly rolled back"
                );
                ensure!(
                    fs::read(directory.join("app/README.md"))? == UPDATED_DOC,
                    "Companion file was not updated"
                );
                fs::write(directory.join("allow-candidate-exit"), b"exit\n")?;
            }
            Mode::NoAcknowledge => {
                wait_until(
                    "exact rollback and restart without a startup ACK",
                    Duration::from_secs(45),
                    || {
                        Ok(stage.join("install-error.txt").is_file()
                            && directory.join("rollback-restarted.json").is_file()
                            && digest(&target)? == case.original_sha256)
                    },
                )?;
                ensure!(
                    !stage.join("install-success.json").exists(),
                    "Missing ACK was reported as success"
                );
                ensure!(
                    !directory.join("candidate-acknowledged").exists(),
                    "No-ACK fixture unexpectedly acknowledged startup"
                );
                ensure!(
                    fs::read(directory.join("app/README.md"))? == ORIGINAL_DOC,
                    "Rollback did not restore companion file bytes"
                );
            }
        }
        wait_until(
            "owned candidate process exit",
            Duration::from_secs(5),
            || Ok(candidate_process.has_exited()),
        )?;
        wait_until("owned helper process exit", Duration::from_secs(5), || {
            Ok(helper_process.has_exited())
        })?;
        println!("Self-update case {mode:?} passed.");
        Ok(())
    }

    fn make_package(archive: &Path, candidate: &Path) -> Result<()> {
        let mut zip = ZipWriter::new(File::create(archive)?);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        zip.start_file("FlashTool.exe", options)?;
        std::io::copy(&mut File::open(candidate)?, &mut zip)?;
        zip.start_file("BUILD-METADATA.json", options)?;
        zip.write_all(
            serde_json::to_string(&serde_json::json!({
                "version": VERSION, "target": "x86_64-pc-windows-msvc"
            }))?
            .as_bytes(),
        )?;
        zip.start_file("README.md", options)?;
        zip.write_all(UPDATED_DOC)?;
        zip.finish()?.sync_all()?;
        Ok(())
    }

    fn digest(path: &Path) -> Result<String> {
        let mut file = File::open(path).with_context(|| format!("Hash {}", path.display()))?;
        let mut digest = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let length = file.read(&mut buffer)?;
            if length == 0 {
                break;
            }
            digest.update(&buffer[..length]);
        }
        Ok(format!("{:x}", digest.finalize()))
    }

    fn read_json<T: for<'a> Deserialize<'a>>(path: &Path) -> Result<T> {
        Ok(serde_json::from_slice(&fs::read(path)?)?)
    }

    fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
        let temporary = path.with_extension("tmp");
        let mut file = File::create(&temporary)?;
        file.write_all(&serde_json::to_vec(value)?)?;
        file.sync_all()?;
        drop(file);
        fs::rename(temporary, path)?;
        Ok(())
    }

    fn wait_until(
        label: &str,
        timeout: Duration,
        mut condition: impl FnMut() -> Result<bool>,
    ) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            if condition()? {
                return Ok(());
            }
            ensure!(Instant::now() < deadline, "Timed out waiting for {label}");
            thread::sleep(Duration::from_millis(20));
        }
    }

    struct OwnedChild(Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            if self.0.try_wait().ok().flatten().is_none() {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }
}
