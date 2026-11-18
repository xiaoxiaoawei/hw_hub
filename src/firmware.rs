//! Validate and snapshot all firmware before opening any device.
use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::{fs::File, io::Read, path::PathBuf};

use crate::chip::ChipModel;

pub const MAX_FLASH_BYTES: usize = 16 * 1024 * 1024;
pub const SECTOR_SIZE: u64 = 4096;

#[derive(Clone, Debug)]
pub struct FirmwareInput {
    pub path: PathBuf,
    pub address: u32,
}

#[derive(Clone, Debug)]
pub struct FirmwareSegment {
    pub address: u32,
    pub data: Vec<u8>,
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct PreparedFirmware {
    pub chip: ChipModel,
    pub segments: Vec<FirmwareSegment>,
    pub sha256: String,
    pub total_bytes: usize,
}

pub fn prepare_firmware(inputs: &[FirmwareInput], chip: ChipModel) -> Result<PreparedFirmware> {
    ensure!(!inputs.is_empty(), "请选择至少一个固件文件");
    ensure!(inputs.len() <= 32, "固件分段不能超过 32 个");
    let mut segments = Vec::with_capacity(inputs.len());
    let mut total_bytes = 0usize;
    for input in inputs {
        ensure!(
            u64::from(input.address) % SECTOR_SIZE == 0,
            "地址 0x{:X} 必须按 4 KiB 对齐",
            input.address
        );
        let file = File::open(&input.path)
            .with_context(|| format!("无法读取固件：{}", input.path.display()))?;
        ensure!(file.metadata()?.is_file(), "固件路径必须是普通文件");
        let mut data = Vec::new();
        file.take(MAX_FLASH_BYTES as u64 + 1)
            .read_to_end(&mut data)?;
        ensure!(!data.is_empty(), "固件文件为空：{}", input.path.display());
        ensure!(data.len() <= MAX_FLASH_BYTES, "单个固件不能超过 16 MiB");
        validate_target_header(&data, input.address, chip)
            .with_context(|| format!("固件校验失败：{}", input.path.display()))?;
        total_bytes = total_bytes
            .checked_add(data.len())
            .context("固件总大小溢出")?;
        ensure!(total_bytes <= MAX_FLASH_BYTES, "固件总大小不能超过 16 MiB");
        let aligned = (data.len() + 3) & !3;
        data.resize(aligned, 0xff);
        let end = u64::from(input.address) + data.len() as u64;
        ensure!(
            end <= MAX_FLASH_BYTES as u64,
            "固件超出本工具支持的 16 MiB 地址范围"
        );
        segments.push(FirmwareSegment {
            address: input.address,
            data,
            name: input
                .path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
        });
    }
    segments.sort_by_key(|s| s.address);
    for pair in segments.windows(2) {
        let first = &pair[0];
        let erase_end = (u64::from(first.address) + first.data.len() as u64).div_ceil(SECTOR_SIZE)
            * SECTOR_SIZE;
        if erase_end > u64::from(pair[1].address) {
            bail!("固件擦除范围重叠：{} 与 {}", first.name, pair[1].name);
        }
    }
    // Include the chip, addresses and lengths so target/layout changes have a
    // different identity even when every firmware byte remains the same.
    let mut hash = Sha256::new();
    hash.update(b"flashtool-layout-v2\0");
    hash.update(chip.image_chip_id().to_le_bytes());
    for s in &segments {
        hash.update(s.address.to_le_bytes());
        hash.update((s.data.len() as u64).to_le_bytes());
        hash.update(&s.data);
    }
    Ok(PreparedFirmware {
        chip,
        segments,
        sha256: format!("{:x}", hash.finalize()),
        total_bytes,
    })
}

fn validate_target_header(data: &[u8], address: u32, chip: ChipModel) -> Result<()> {
    let boot_address = chip.bootloader_address();
    if address == 0 && boot_address != 0 {
        // Standard unencrypted ESP32/S2 merged images start at 0x0, contain
        // erased padding up to 0x1000, and put the bootloader there. Never
        // accept arbitrary raw bytes at 0x0 as a way around chip validation.
        let padding_len = boot_address as usize;
        ensure!(
            data.len() >= padding_len + 24,
            "{} 的 0x0 合并镜像必须包含 0x{boot_address:X} 处的 bootloader",
            chip.label()
        );
        ensure!(
            data[..padding_len].iter().all(|byte| *byte == 0xff),
            "{} 的 0x0 合并镜像前 0x{boot_address:X} 字节必须为 0xFF 填充；单独 bootloader 请放在 0x{boot_address:X}",
            chip.label()
        );
        return validate_image_header(&data[padding_len..], chip);
    }
    if address == boot_address || data.first() == Some(&0xe9) {
        validate_image_header(data, chip)?;
    }
    // Other non-image regions may contain partition tables, NVS and raw data.
    Ok(())
}

fn validate_image_header(data: &[u8], chip: ChipModel) -> Result<()> {
    ensure!(
        data.len() >= 24 && data[0] == 0xe9,
        "{} 镜像头缺失或不完整，bootloader 地址为 0x{:X}",
        chip.label(),
        chip.bootloader_address()
    );
    // ESP extended image header: little-endian chip ID at bytes 12..14.
    let image_id = u16::from_le_bytes([data[12], data[13]]);
    ensure!(
        image_id == chip.image_chip_id(),
        "固件芯片 ID 为 {image_id}，所选目标为 {} (ID {})",
        chip.label(),
        chip.image_chip_id()
    );
    Ok(())
}
