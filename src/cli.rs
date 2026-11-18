//! Read-only checks usable in CI without a desktop or connected device.
use crate::{
    backend,
    chip::ChipModel,
    firmware::{FirmwareInput, prepare_firmware},
};
use anyhow::{Context, Result, bail, ensure};
use std::{ffi::OsString, path::PathBuf};

pub fn run_if_requested() -> Option<i32> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.is_empty() {
        return None;
    }
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::{
            Foundation::INVALID_HANDLE_VALUE,
            System::Console::{
                ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_OUTPUT_HANDLE,
            },
        };
        // A GUI-subsystem executable may inherit a valid redirected pipe.
        // Attaching a console unconditionally can replace it and break CLI
        // output capture in PowerShell and release/CI version checks.
        let stdout = GetStdHandle(STD_OUTPUT_HANDLE);
        if stdout.is_null() || stdout == INVALID_HANDLE_VALUE {
            AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
    Some(match run(&args) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{e:#}");
            1
        }
    })
}

#[derive(Debug)]
enum Command {
    Help,
    Version,
    ListPorts,
    Check {
        chip: ChipModel,
        inputs: Vec<FirmwareInput>,
    },
}

fn run(args: &[OsString]) -> Result<()> {
    match parse_args(args)? {
        Command::Help => {
            println!(
                "FlashTool {}\n\n无参数启动图形界面。\n--version     显示版本\n--list-ports  枚举串口（不连接设备）\n--check [--chip esp32c3] 0x0:<firmware.bin> [0x8000:<partitions.bin> ...]\n              只检查固件/布局并输出 JSON，不连接、不烧录\n\n支持芯片：{}\n默认芯片：esp32c3；ESP32/ESP32-S2 单独 bootloader 地址为 0x1000。\n",
                env!("CARGO_PKG_VERSION"),
                ChipModel::all()
                    .iter()
                    .map(|chip| chip.slug())
                    .collect::<Vec<_>>()
                    .join(", "),
            );
        }
        Command::Version => println!("FlashTool {}", env!("CARGO_PKG_VERSION")),
        Command::ListPorts => {
            let ports: Vec<_> = backend::list_ports()?.into_iter().map(|p| serde_json::json!({"name":p.name,"description":p.description,"usb":p.usb,"suggested":p.suggested})).collect();
            println!("{}", serde_json::to_string_pretty(&ports)?);
        }
        Command::Check { chip, inputs } => {
            let fw = prepare_firmware(&inputs, chip)?;
            let segments: Vec<_> = fw.segments.iter().map(|s| serde_json::json!({"name":s.name,"address":format!("0x{:X}",s.address),"aligned_bytes":s.data.len()})).collect();
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({"chip":fw.chip.slug(),"layout_sha256":fw.sha256,"total_bytes":fw.total_bytes,"segments":segments})
                )?
            );
        }
    }
    Ok(())
}

fn parse_args(args: &[OsString]) -> Result<Command> {
    if args.len() == 1 {
        match args[0].to_str() {
            Some("--help" | "-h") => return Ok(Command::Help),
            Some("--version" | "-V") => return Ok(Command::Version),
            Some("--list-ports") => return Ok(Command::ListPorts),
            _ => {}
        }
    }
    let mut chip = ChipModel::default();
    let mut chip_specified = false;
    let mut check = false;
    let mut inputs = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let text = args[index]
            .to_str()
            .context("参数必须是有效 Unicode 文本或路径")?;
        if text == "--check" {
            ensure!(!check, "不能重复指定 --check");
            check = true;
        } else if text == "--chip" || text.starts_with("--chip=") {
            ensure!(!chip_specified, "不能重复指定 --chip");
            let name = if text == "--chip" {
                index += 1;
                args.get(index)
                    .and_then(|arg| arg.to_str())
                    .context("--chip 后必须指定芯片名称")?
            } else {
                text.strip_prefix("--chip=").unwrap_or_default()
            };
            chip = name.parse()?;
            chip_specified = true;
        } else if text.starts_with('-') {
            bail!("未知参数 {text}，请使用 --help");
        } else {
            inputs.push(parse_input(text)?);
        }
        index += 1;
    }
    ensure!(check, "请使用 --check 检查固件，或使用 --help 查看用法");
    ensure!(
        !inputs.is_empty(),
        "--check 后必须提供至少一个 0x地址:文件路径"
    );
    Ok(Command::Check { chip, inputs })
}

fn parse_input(text: &str) -> Result<FirmwareInput> {
    let (address, path) = text
        .split_once(':')
        .context("固件参数格式为 0x地址:文件路径")?;
    ensure!(!path.is_empty(), "固件路径不能为空");
    let address = u32::from_str_radix(
        address
            .strip_prefix("0x")
            .or_else(|| address.strip_prefix("0X"))
            .unwrap_or(address),
        16,
    )
    .context("固件地址必须为十六进制")?;
    Ok(FirmwareInput {
        path: PathBuf::from(path),
        address,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(OsString::from).collect()
    }

    #[test]
    fn legacy_check_defaults_to_c3_and_keeps_windows_path_colons() {
        let Command::Check { chip, inputs } =
            parse_args(&args(&["--check", "0x0:C:\\firmware dir\\merged.bin"])).unwrap()
        else {
            panic!("expected check")
        };
        assert_eq!(chip, ChipModel::Esp32c3);
        assert_eq!(inputs[0].address, 0);
        assert_eq!(
            inputs[0].path,
            PathBuf::from("C:\\firmware dir\\merged.bin")
        );
    }

    #[test]
    fn chip_option_is_supported_before_or_after_the_check_command() {
        for parts in [
            vec!["--check", "--chip", "esp32-s3", "0x0:merged.bin"],
            vec!["--chip=esp32s3", "--check", "0x0:merged.bin"],
        ] {
            let Command::Check { chip, .. } = parse_args(&args(&parts)).unwrap() else {
                panic!("expected check")
            };
            assert_eq!(chip, ChipModel::Esp32s3);
        }
    }

    #[test]
    fn malformed_or_unsupported_chip_options_are_rejected() {
        for parts in [
            vec!["--check", "--chip"],
            vec!["--check", "--chip", "esp8266", "0x0:a.bin"],
            vec!["--check", "--chip", "esp32", "--chip=esp32c3", "0x0:a.bin"],
            vec!["--chip", "esp32"],
            vec!["--check", "0x0:"],
        ] {
            assert!(parse_args(&args(&parts)).is_err());
        }
    }

    #[test]
    fn version_is_explicit_and_never_runs_a_firmware_check() {
        assert!(matches!(
            parse_args(&args(&["--version"])).unwrap(),
            Command::Version
        ));
        assert!(matches!(
            parse_args(&args(&["-V"])).unwrap(),
            Command::Version
        ));
        assert!(parse_args(&args(&["--version", "--check", "0x0:a.bin"])).is_err());
    }
}
