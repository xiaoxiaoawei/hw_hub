//! Serial discovery and an independent target-checked flashing session per device.

use std::{borrow::Cow, time::Duration};

use anyhow::{Context, Result, bail};
use espflash::{
    connection::{Connection, ResetAfterOperation, ResetBeforeOperation},
    flasher::{FlashSize, Flasher},
    image_format::Segment,
    target::ProgressCallbacks,
};
use serialport::{FlowControl, SerialPortInfo, SerialPortType, UsbPortInfo};

use crate::firmware::PreparedFirmware;

const SECTOR_SIZE: u64 = 4096;

#[derive(Clone, Debug)]
pub struct Port {
    pub name: String,
    pub description: String,
    pub usb: bool,
    pub suggested: bool,
}

/// A recognized adapter is suggested in the UI; selection remains explicit.
pub fn list_ports() -> Result<Vec<Port>> {
    let mut ports: Vec<_> = serialport::available_ports()
        .context("无法枚举串口，请检查操作系统设备管理器")?
        .into_iter()
        .map(describe_port)
        .collect();
    ports.sort_by(|a, b| {
        port_number(&a.name)
            .cmp(&port_number(&b.name))
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(ports)
}

fn port_number(name: &str) -> Option<u32> {
    name.to_ascii_uppercase()
        .strip_prefix("COM")
        .and_then(|number| number.parse().ok())
}

fn describe_port(info: SerialPortInfo) -> Port {
    let (description, usb, suggested) = match info.port_type {
        SerialPortType::UsbPort(ref usb) => {
            let known = match usb.vid {
                0x1a86 | 0x4348 => Some("WCH CH34x / CH343"),
                0x10c4 => Some("Silicon Labs CP210x"),
                0x303a => Some("Espressif USB"),
                _ => None,
            };
            let label = usb
                .product
                .as_deref()
                .or(known)
                .or(usb.manufacturer.as_deref())
                .unwrap_or("USB 串口");
            (
                format!("{label} · USB {:04X}:{:04X}", usb.vid, usb.pid),
                true,
                known.is_some(),
            )
        }
        SerialPortType::BluetoothPort => ("蓝牙串口".into(), false, false),
        SerialPortType::PciPort => ("系统串口".into(), false, false),
        SerialPortType::Unknown => ("串口（系统未提供 USB 信息）".into(), false, false),
    };
    Port {
        name: info.port_name,
        description,
        usb,
        suggested,
    }
}

#[derive(Clone, Debug)]
pub enum FlashProgress {
    Connecting,
    Writing { fraction: f32, address: u32 },
    Verifying,
    Message(String),
}

/// Runs synchronously on one worker. Separate invocations never share a port.
///
/// `write_bins_to_flash` ends the complete session and performs the configured
/// hard reset, so all segments must be submitted together.
pub fn flash(
    port: &str,
    baud: u32,
    firmware: &PreparedFirmware,
    callback: &mut dyn FnMut(FlashProgress),
) -> Result<()> {
    if port.trim().is_empty() {
        bail!("未选择串口");
    }
    if baud < 115_200 {
        bail!("烧录波特率不能低于 115200");
    }
    validate_layout(firmware, None)?;
    callback(FlashProgress::Connecting);

    // Preserve USB PID metadata: espflash uses it to choose native USB reset
    // sequences for USB Serial/JTAG, versus DTR/RTS for USB-UART adapters.
    let info = serialport::available_ports()
        .context("连接前无法重新枚举串口")?
        .into_iter()
        .find(|info| info.port_name.eq_ignore_ascii_case(port))
        .with_context(|| format!("串口 {port} 已断开或不存在，请刷新设备列表"))?;
    let usb = match info.port_type {
        SerialPortType::UsbPort(usb) => usb,
        _ => UsbPortInfo {
            vid: 0,
            pid: 0,
            serial_number: None,
            manufacturer: None,
            product: None,
        },
    };
    let serial = serialport::new(port, 115_200)
        .flow_control(FlowControl::None)
        .timeout(Duration::from_secs(3))
        .open_native()
        .with_context(|| format!("无法打开 {port}，请检查驱动、连接及串口占用"))?;
    let connection = Connection::new(
        serial,
        usb,
        ResetAfterOperation::HardReset,
        ResetBeforeOperation::DefaultReset,
        baud,
    );

    // The explicit chip argument is an expectation checked against the real
    // detected chip, not a replacement for detection. Never use NoResetNoSync.
    let target_chip = firmware.chip.esp_chip();
    let target_label = firmware.chip.label();
    let mut flasher = Flasher::connect(
        connection,
        true,
        true,
        false,
        Some(target_chip),
        Some(baud),
    )
    .with_context(|| {
        format!(
            "{port} 连接 {target_label} 失败。请检查供电与 BOOT/RESET 电路；也可按住 BOOT 后按 RESET，再重试"
        )
    })?;
    if flasher.chip() != target_chip {
        bail!(
            "{port} 检测到 {}，所选目标为 {target_label}",
            flasher.chip()
        );
    }
    if flasher.secure_download_mode() {
        bail!("{port} 处于安全下载模式，无法执行本工具要求的写后校验，已停止烧录");
    }
    let device = flasher
        .device_info()
        .with_context(|| format!("{port} 无法读取芯片和实际 Flash 容量"))?;
    // espflash 4.7.0 silently substitutes 4 MiB for an unknown JEDEC size.
    // Do not treat that fallback as actual hardware capacity. In this pinned
    // version flash_detect leaves its raw 0x9F read result in SPI W0. Read it
    // immediately, before any other SPI operation, and validate it strictly.
    // Recheck this implementation detail whenever upgrading espflash.
    flasher
        .flash_detect()
        .with_context(|| format!("{port} 无法读取 Flash JEDEC ID"))?
        .with_context(|| format!("{port} Flash 无响应，不能确认实际容量"))?;
    let jedec_id = flasher
        .connection()
        .read_reg(target_chip.spi_registers().w0())
        .with_context(|| format!("{port} 无法读取 Flash 容量识别原始结果"))?;
    let capacity = strict_jedec_capacity(jedec_id, u64::from(device.flash_size.size()))?;
    validate_layout(firmware, Some(capacity))?;
    callback(FlashProgress::Message(format!(
        "已识别 {target_label}，Flash {} MiB，波特率 {baud}；启用压缩传输及写后 MD5 校验",
        capacity / (1024 * 1024)
    )));

    let segments: Vec<_> = firmware
        .segments
        .iter()
        .map(|segment| Segment {
            addr: segment.address,
            data: Cow::Borrowed(segment.data.as_slice()),
        })
        .collect();
    let sizes: Vec<_> = firmware
        .segments
        .iter()
        .map(|segment| (segment.address, segment.data.len() as u64))
        .collect();
    let mut progress = SessionProgress::new(&sizes, callback);
    flasher
        .write_bins_to_flash(&segments, &mut progress)
        .with_context(|| format!("{port} 烧录、校验或写后复位失败"))?;
    callback(FlashProgress::Message(
        "全部分段已通过 MD5 校验，设备已复位".into(),
    ));
    Ok(())
}

fn strict_jedec_capacity(jedec_id: u32, reported_capacity: u64) -> Result<u64> {
    let manufacturer = (jedec_id & 0xff) as u8;
    if matches!(manufacturer, 0x00 | 0xff) {
        bail!("Flash JEDEC ID 0x{jedec_id:06X} 的厂商字段无效，已停止烧录");
    }
    let size_id = ((jedec_id >> 16) & 0xff) as u8;
    let capacity = u64::from(
        FlashSize::from_detected(size_id)
            .with_context(|| {
                format!("无法识别 Flash JEDEC ID 0x{jedec_id:06X} 的容量，已停止烧录")
            })?
            .size(),
    );
    if capacity != reported_capacity {
        bail!(
            "Flash 容量检测结果不一致（JEDEC 0x{jedec_id:06X}：0x{capacity:X}，连接时：0x{reported_capacity:X}），已停止烧录"
        );
    }
    Ok(capacity)
}

fn validate_layout(firmware: &PreparedFirmware, capacity: Option<u64>) -> Result<()> {
    if firmware.segments.is_empty() {
        bail!("没有可烧录的固件分段");
    }
    let mut regions = Vec::with_capacity(firmware.segments.len());
    for segment in &firmware.segments {
        if segment.data.is_empty() {
            bail!("固件分段 {} 为空", segment.name);
        }
        let start = u64::from(segment.address);
        if !start.is_multiple_of(SECTOR_SIZE) {
            bail!("{} 的地址必须按 4 KiB 擦除扇区对齐", segment.name);
        }
        if !segment.data.len().is_multiple_of(4) {
            bail!("{} 的数据长度未完成 4 字节对齐", segment.name);
        }
        let end = start
            .checked_add(segment.data.len() as u64)
            .context("固件地址范围溢出")?;
        let erase_end = end.div_ceil(SECTOR_SIZE) * SECTOR_SIZE;
        if erase_end > (u64::from(u32::MAX) + 1) {
            bail!("{} 超出 32 位 Flash 地址空间", segment.name);
        }
        if let Some(capacity) = capacity
            && erase_end > capacity
        {
            bail!(
                "{} 的擦除范围结束于 0x{erase_end:X}，超出设备实际 Flash 容量 0x{capacity:X}",
                segment.name
            );
        }
        regions.push((start, erase_end, &segment.name));
    }
    regions.sort_by_key(|region| region.0);
    for pair in regions.windows(2) {
        if pair[0].1 > pair[1].0 {
            bail!("{} 与 {} 的 Flash 擦除范围重叠", pair[0].2, pair[1].2);
        }
    }
    Ok(())
}

/// espflash reports compressed transport blocks, independently per segment.
/// Weight each segment by its uncompressed size to obtain whole-job progress.
struct SessionProgress<'a> {
    sizes: &'a [(u32, u64)],
    callback: &'a mut dyn FnMut(FlashProgress),
    total_bytes: u64,
    completed_bytes: u64,
    address: u32,
    current_bytes: u64,
    blocks: usize,
}

impl<'a> SessionProgress<'a> {
    fn new(sizes: &'a [(u32, u64)], callback: &'a mut dyn FnMut(FlashProgress)) -> Self {
        Self {
            sizes,
            callback,
            total_bytes: sizes.iter().map(|(_, bytes)| bytes).sum(),
            completed_bytes: 0,
            address: 0,
            current_bytes: 0,
            blocks: 0,
        }
    }

    fn report(&mut self, segment_fraction: f64) {
        let fraction = if self.total_bytes == 0 {
            0.0
        } else {
            ((self.completed_bytes as f64 + self.current_bytes as f64 * segment_fraction)
                / self.total_bytes as f64)
                .clamp(0.0, 1.0) as f32
        };
        (self.callback)(FlashProgress::Writing {
            fraction,
            address: self.address,
        });
    }
}

impl ProgressCallbacks for SessionProgress<'_> {
    fn init(&mut self, addr: u32, total: usize) {
        self.address = addr;
        self.blocks = total;
        self.current_bytes = self
            .sizes
            .iter()
            .find_map(|(address, bytes)| (*address == addr).then_some(*bytes))
            .unwrap_or(0);
        self.report(0.0);
    }

    fn update(&mut self, current: usize) {
        let part = if self.blocks == 0 {
            0.0
        } else {
            current.min(self.blocks) as f64 / self.blocks as f64
        };
        self.report(part);
    }

    fn verifying(&mut self) {
        (self.callback)(FlashProgress::Verifying);
    }

    fn finish(&mut self, _skipped: bool) {
        self.report(1.0);
        self.completed_bytes += self.current_bytes;
        self.current_bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::firmware::FirmwareSegment;

    fn firmware(segments: Vec<(u32, usize)>) -> PreparedFirmware {
        PreparedFirmware {
            chip: crate::chip::ChipModel::default(),
            total_bytes: segments.iter().map(|(_, length)| length).sum(),
            sha256: String::new(),
            segments: segments
                .into_iter()
                .map(|(address, length)| FirmwareSegment {
                    address,
                    data: vec![0xff; length],
                    name: format!("0x{address:X}.bin"),
                })
                .collect(),
        }
    }

    #[test]
    fn refuses_a_segment_that_crosses_the_actual_device_capacity() {
        let capacity = 4 * 1024 * 1024;
        assert!(
            validate_layout(
                &firmware(vec![(capacity - 4096, 4096)]),
                Some(capacity.into())
            )
            .is_ok()
        );
        assert!(
            validate_layout(
                &firmware(vec![(capacity - 4096, 4100)]),
                Some(capacity.into())
            )
            .is_err()
        );
    }

    #[test]
    fn prevents_the_next_segment_being_erased_by_the_previous_one() {
        assert!(validate_layout(&firmware(vec![(0, 4100), (0x1000, 4)]), None).is_err());
        assert!(validate_layout(&firmware(vec![(0, 4100), (0x2000, 4)]), None).is_ok());
    }

    #[test]
    fn refuses_empty_and_unaligned_prepared_data_before_opening_a_port() {
        assert!(validate_layout(&firmware(vec![]), None).is_err());
        assert!(validate_layout(&firmware(vec![(0, 0)]), None).is_err());
        assert!(validate_layout(&firmware(vec![(4, 4)]), None).is_err());
        assert!(validate_layout(&firmware(vec![(0, 3)]), None).is_err());
    }

    #[test]
    fn progress_weights_different_compression_ratios_by_original_size() {
        let mut values = Vec::new();
        let mut callback = |event| {
            if let FlashProgress::Writing { fraction, .. } = event {
                values.push(fraction);
            }
        };
        let sizes = [(0, 1024), (0x8000, 3072)];
        let mut progress = SessionProgress::new(&sizes, &mut callback);
        progress.init(0, 100);
        progress.update(100);
        progress.verifying();
        progress.finish(false);
        progress.init(0x8000, 2);
        progress.update(1);
        progress.update(2);
        progress.finish(false);
        assert_eq!(values, vec![0.0, 0.25, 0.25, 0.25, 0.625, 1.0, 1.0]);
    }

    #[test]
    fn com_ports_sort_numerically() {
        assert!(port_number("COM2") < port_number("COM10"));
        assert_eq!(port_number("com12"), Some(12));
    }

    #[test]
    fn accepts_recognized_jedec_size_only_when_it_matches_the_connection() {
        assert_eq!(
            strict_jedec_capacity(0x1640ef, 4 * 1024 * 1024).unwrap(),
            4 * 1024 * 1024
        );
        assert!(strict_jedec_capacity(0x1540ef, 4 * 1024 * 1024).is_err());
    }

    #[test]
    fn refuses_unknown_jedec_sizes_instead_of_using_the_library_fallback() {
        assert!(strict_jedec_capacity(0x9940ef, 4 * 1024 * 1024).is_err());
        assert!(strict_jedec_capacity(0x0000ef, 4 * 1024 * 1024).is_err());
    }

    #[test]
    fn refuses_invalid_manufacturer_even_when_size_byte_looks_valid() {
        assert!(strict_jedec_capacity(0x164000, 4 * 1024 * 1024).is_err());
        assert!(strict_jedec_capacity(0x1640ff, 4 * 1024 * 1024).is_err());
        assert!(strict_jedec_capacity(0xffffff, 4 * 1024 * 1024).is_err());
    }
}
