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
    /// Stable metadata identity, including a USB serial number when available.
    /// A port without a USB serial still needs observed-disconnect invalidation
    /// before queued work may continue on the same OS port name.
    pub identity: String,
    pub description: String,
    pub usb: bool,
    pub suggested: bool,
}

/// A recognized adapter is suggested in the UI; selection remains explicit.
pub fn list_ports() -> Result<Vec<Port>> {
    let mut ports: Vec<_> = available_port_infos()?
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

fn available_port_infos() -> Result<Vec<SerialPortInfo>> {
    let ports = serialport::available_ports().context("无法枚举串口，请检查操作系统设备管理器")?;
    #[cfg(windows)]
    let ports = retain_present_ports(ports, present_ports::names())?;
    Ok(ports)
}

#[cfg(any(windows, test))]
fn retain_present_ports(
    ports: Vec<SerialPortInfo>,
    present_names: Result<std::collections::HashSet<String>>,
) -> Result<Vec<SerialPortInfo>> {
    let present: std::collections::HashSet<_> = present_names
        .context("无法确认 Windows 当前在线串口；保留上次设备状态")?
        .into_iter()
        .map(|name| port_name_key(&name))
        .collect();
    // serialport 4.10.1 supplements its DIGCF_PRESENT enumeration with raw
    // SERIALCOMM registry entries. Those can outlive a removal notification.
    // Intersect every entry with actual present device nodes, without probing
    // or opening COM ports. A genuinely present Unknown port stays available.
    Ok(ports
        .into_iter()
        .filter(|port| present.contains(&port_name_key(&port.port_name)))
        .collect())
}

#[cfg(windows)]
mod present_ports {
    use anyhow::{Context, Result, bail, ensure};
    use std::{
        collections::HashSet,
        mem,
        ptr::{null, null_mut},
    };
    use windows_sys::Win32::{
        Devices::DeviceAndDriverInstallation::{
            CM_Get_DevNode_Status, CR_SUCCESS, DICS_FLAG_GLOBAL, DIGCF_PRESENT, DIREG_DEV,
            GUID_DEVCLASS_MODEM, GUID_DEVCLASS_PORTS, HDEVINFO, SP_DEVINFO_DATA,
            SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW,
            SetupDiOpenDevRegKey,
        },
        Foundation::{
            ERROR_FILE_NOT_FOUND, ERROR_KEY_DOES_NOT_EXIST, ERROR_NO_MORE_ITEMS,
            ERROR_PATH_NOT_FOUND, INVALID_HANDLE_VALUE,
        },
        System::Registry::{HKEY, KEY_QUERY_VALUE, REG_SZ, RegCloseKey, RegQueryValueExW},
    };

    struct DeviceSet(HDEVINFO);
    impl Drop for DeviceSet {
        fn drop(&mut self) {
            unsafe {
                SetupDiDestroyDeviceInfoList(self.0);
            }
        }
    }

    struct DeviceKey(HKEY);
    impl Drop for DeviceKey {
        fn drop(&mut self) {
            unsafe {
                RegCloseKey(self.0);
            }
        }
    }

    pub(super) fn names() -> Result<HashSet<String>> {
        let mut names = HashSet::new();
        for class in [GUID_DEVCLASS_PORTS, GUID_DEVCLASS_MODEM] {
            let raw = unsafe { SetupDiGetClassDevsW(&class, null(), null_mut(), DIGCF_PRESENT) };
            if raw == INVALID_HANDLE_VALUE as HDEVINFO {
                return Err(std::io::Error::last_os_error())
                    .context("无法读取当前在线的 Ports/Modem 设备集合");
            }
            let devices = DeviceSet(raw);
            let mut index = 0u32;
            loop {
                let mut device: SP_DEVINFO_DATA = unsafe { mem::zeroed() };
                device.cbSize = mem::size_of::<SP_DEVINFO_DATA>() as u32;
                if unsafe { SetupDiEnumDeviceInfo(devices.0, index, &mut device) } == 0 {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() == Some(ERROR_NO_MORE_ITEMS as i32) {
                        break;
                    }
                    return Err(error).context("枚举当前在线串口设备失败");
                }
                let (mut status, mut problem) = (0u32, 0u32);
                let result =
                    unsafe { CM_Get_DevNode_Status(&mut status, &mut problem, device.DevInst, 0) };
                ensure!(
                    result == CR_SUCCESS,
                    "读取当前串口设备状态失败（CM 错误 0x{result:X}）"
                );
                // Match serialport's healthy-device policy, including Unknown
                // entries obtained from SERIALCOMM after the primary scan.
                if problem == 0
                    && let Some(name) = port_name(&devices, &device)?
                {
                    names.insert(name);
                }
                index = index.checked_add(1).context("在线设备数量超出支持范围")?;
            }
        }
        Ok(names)
    }

    fn port_name(devices: &DeviceSet, device: &SP_DEVINFO_DATA) -> Result<Option<String>> {
        let raw = unsafe {
            SetupDiOpenDevRegKey(
                devices.0,
                device,
                DICS_FLAG_GLOBAL,
                0,
                DIREG_DEV,
                KEY_QUERY_VALUE,
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            let error = std::io::Error::last_os_error();
            // Some present Ports/Modem nodes are controllers or have not been
            // assigned a port. A missing key/value is not a failed full scan.
            if matches!(error.raw_os_error(), Some(code) if code == ERROR_FILE_NOT_FOUND as i32 || code == ERROR_PATH_NOT_FOUND as i32 || code == ERROR_KEY_DOES_NOT_EXIST as i32)
            {
                return Ok(None);
            }
            return Err(error).context("无法读取在线设备的 PortName 注册表项");
        }
        let key = DeviceKey(raw);
        let value_name: Vec<_> = "PortName".encode_utf16().chain(Some(0)).collect();
        let mut buffer = [0u16; 256];
        let mut byte_len = mem::size_of_val(&buffer) as u32;
        let mut value_type = 0;
        let result = unsafe {
            RegQueryValueExW(
                key.0,
                value_name.as_ptr(),
                null(),
                &mut value_type,
                buffer.as_mut_ptr().cast(),
                &mut byte_len,
            )
        };
        if matches!(result, ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) {
            return Ok(None);
        }
        if result != 0 {
            return Err(std::io::Error::from_raw_os_error(result as i32))
                .context("读取在线设备的 PortName 失败");
        }
        ensure!(
            value_type == REG_SZ
                && byte_len.is_multiple_of(2)
                && byte_len as usize <= mem::size_of_val(&buffer),
            "在线设备的 PortName 类型或长度无效"
        );
        let name = String::from_utf16(&buffer[..byte_len as usize / 2])
            .context("在线设备的 PortName 不是有效 UTF-16")?;
        let name = name.trim_end_matches('\0');
        if name.is_empty() {
            return Ok(None);
        }
        if name.contains('\0') {
            bail!("在线设备的 PortName 含有无效的空字符");
        }
        Ok(Some(name.to_owned()))
    }
}

fn port_number(name: &str) -> Option<u32> {
    name.to_ascii_uppercase()
        .strip_prefix("COM")
        .and_then(|number| number.parse().ok())
}

fn describe_port(info: SerialPortInfo) -> Port {
    let identity = identity_for_info(&info);
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
        identity,
        description,
        usb,
        suggested,
    }
}

pub(crate) fn port_name_key(name: &str) -> String {
    if cfg!(windows) {
        name.strip_prefix("\\\\.\\")
            .unwrap_or(name)
            .to_ascii_uppercase()
    } else {
        name.to_owned()
    }
}

fn identity_for_info(info: &SerialPortInfo) -> String {
    let name = port_name_key(&info.port_name);
    match &info.port_type {
        SerialPortType::UsbPort(usb) => serde_json::json!({
            "kind": "usb", "port": name, "vid": usb.vid, "pid": usb.pid,
            "serial": usb.serial_number.as_deref(),
        }),
        SerialPortType::BluetoothPort => serde_json::json!({"kind": "bluetooth", "port": name}),
        SerialPortType::PciPort => serde_json::json!({"kind": "pci", "port": name}),
        SerialPortType::Unknown => serde_json::json!({"kind": "unknown", "port": name}),
    }
    .to_string()
}

fn open_checked<T>(
    expected: Option<&Port>,
    actual: &SerialPortInfo,
    open: impl FnOnce(&SerialPortInfo) -> Result<T>,
) -> Result<T> {
    if let Some(expected) = expected
        && (expected.identity.is_empty()
            || port_name_key(&expected.name) != port_name_key(&actual.port_name)
            || expected.identity != identity_for_info(actual))
    {
        bail!(
            "{} 的设备身份已变化；为避免烧录新接入设备，本任务已停止，请重新选择设备并启动新批次",
            expected.name
        );
    }
    open(actual)
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
    flash_impl(port, None, baud, firmware, callback)
}

/// Production batch entry point: bind a queued task to its selected device
/// snapshot before opening any native serial handle.
pub fn flash_expected(
    expected: &Port,
    baud: u32,
    firmware: &PreparedFirmware,
    callback: &mut dyn FnMut(FlashProgress),
) -> Result<()> {
    flash_impl(&expected.name, Some(expected), baud, firmware, callback)
}

fn flash_impl(
    port: &str,
    expected: Option<&Port>,
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
    let info = available_port_infos()
        .context("连接前无法重新枚举在线串口")?
        .into_iter()
        .find(|info| port_name_key(&info.port_name) == port_name_key(port))
        .with_context(|| format!("串口 {port} 已断开或不存在，请刷新设备列表"))?;
    let serial = open_checked(expected, &info, |actual| {
        serialport::new(&actual.port_name, 115_200)
            .flow_control(FlowControl::None)
            .timeout(Duration::from_secs(3))
            .open_native()
            .with_context(|| format!("无法打开 {port}，请检查驱动、连接及串口占用"))
    })?;
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

    fn usb_info(name: &str, serial: Option<&str>) -> SerialPortInfo {
        SerialPortInfo {
            port_name: name.into(),
            port_type: SerialPortType::UsbPort(UsbPortInfo {
                vid: 0x10c4,
                pid: 0xea60,
                serial_number: serial.map(str::to_owned),
                manufacturer: Some("Silicon Labs".into()),
                product: Some("CP210x".into()),
            }),
        }
    }

    fn present(names: &[&str]) -> Result<std::collections::HashSet<String>> {
        Ok(names.iter().map(|name| (*name).to_owned()).collect())
    }

    #[test]
    fn excludes_registry_only_ghost_com_without_dropping_present_usb() {
        let ghost = SerialPortInfo {
            port_name: "COM99".into(),
            port_type: SerialPortType::Unknown,
        };
        let ports =
            retain_present_ports(vec![usb_info("COM3", Some("A")), ghost], present(&["COM3"]))
                .unwrap();
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].port_name, "COM3");
        assert!(matches!(ports[0].port_type, SerialPortType::UsbPort(_)));
    }

    #[test]
    fn preserves_an_unknown_port_confirmed_by_present_device_metadata() {
        let unknown = SerialPortInfo {
            port_name: "COM7".into(),
            port_type: SerialPortType::Unknown,
        };
        let ports = retain_present_ports(
            vec![unknown, usb_info("COM3", Some("A"))],
            present(&["COM7"]),
        )
        .unwrap();
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].port_name, "COM7");
        assert!(matches!(ports[0].port_type, SerialPortType::Unknown));
    }

    #[test]
    fn unplugged_usb_does_not_reappear_as_registry_unknown_before_reinsertion() {
        let connected =
            retain_present_ports(vec![usb_info("COM3", Some("original"))], present(&["COM3"]))
                .unwrap();
        let original = describe_port(connected[0].clone());
        let stale_registry = SerialPortInfo {
            port_name: "COM3".into(),
            port_type: SerialPortType::Unknown,
        };
        let removed = retain_present_ports(vec![stale_registry], present(&[])).unwrap();
        assert!(removed.is_empty());
        let reinserted = retain_present_ports(
            vec![usb_info("COM3", Some("replacement"))],
            present(&["COM3"]),
        )
        .unwrap();
        let replacement = describe_port(reinserted[0].clone());
        assert_eq!(original.name, replacement.name);
        assert_ne!(original.identity, replacement.identity);
    }

    #[test]
    fn failed_present_device_query_is_not_reported_as_a_successful_empty_scan() {
        let result = retain_present_ports(
            vec![usb_info("COM3", Some("A"))],
            Err(anyhow::anyhow!("mock registry access denied")),
        );
        let error = result.unwrap_err();
        assert!(format!("{error:#}").contains("mock registry access denied"));
    }

    #[test]
    fn changed_usb_serial_is_rejected_before_any_port_open_operation() {
        let expected = describe_port(usb_info("COM3", Some("device-A")));
        let replacement = usb_info("COM3", Some("device-B"));
        // Same COM name, product description and VID/PID must not hide that
        // another physical USB-UART adapter has replaced the selected one.
        assert_eq!(
            expected.description,
            describe_port(replacement.clone()).description
        );
        let mut opened = false;
        let result = open_checked(Some(&expected), &replacement, |_| {
            opened = true;
            Ok(())
        });
        assert!(result.is_err());
        assert!(!opened);
        let result = open_checked(Some(&expected), &usb_info("COM3", Some("device-A")), |_| {
            opened = true;
            Ok(())
        });
        assert!(result.is_ok());
        assert!(opened);
    }

    #[test]
    fn usb_identity_includes_port_vid_pid_and_optional_serial_but_not_labels() {
        let original = usb_info("COM3", Some("A"));
        let mut renamed = original.clone();
        if let SerialPortType::UsbPort(usb) = &mut renamed.port_type {
            usb.product = Some("a localized product label".into());
        }
        assert_eq!(identity_for_info(&original), identity_for_info(&renamed));
        assert_ne!(
            identity_for_info(&original),
            identity_for_info(&usb_info("COM4", Some("A")))
        );
        assert_ne!(
            identity_for_info(&original),
            identity_for_info(&usb_info("COM3", None))
        );
        if let SerialPortType::UsbPort(usb) = &mut renamed.port_type {
            usb.pid = 0xea70;
        }
        assert_ne!(identity_for_info(&original), identity_for_info(&renamed));
    }

    #[test]
    fn non_usb_port_types_have_distinct_identities() {
        let pci = SerialPortInfo {
            port_name: "COM1".into(),
            port_type: SerialPortType::PciPort,
        };
        let unknown = SerialPortInfo {
            port_name: "COM1".into(),
            port_type: SerialPortType::Unknown,
        };
        assert_ne!(identity_for_info(&pci), identity_for_info(&unknown));
    }

    #[cfg(windows)]
    #[test]
    fn win32_namespace_and_case_aliases_share_the_same_port_key() {
        assert_eq!(port_name_key("\\\\.\\COM3"), port_name_key("COM3"));
        assert_eq!(port_name_key("com3"), port_name_key("COM3"));
    }

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
