//! Serial discovery and an independent target-checked flashing session per device.

use std::{borrow::Cow, time::Duration};

use anyhow::{Context, Result, bail};
use espflash::{
    connection::{Connection, ResetAfterOperation, ResetBeforeOperation},
    flasher::{FlashSize, Flasher},
    image_format::Segment,
    target::{Chip, ProgressCallbacks},
};
use serialport::{FlowControl, SerialPortInfo, SerialPortType, UsbPortInfo};

use crate::firmware::PreparedFirmware;

const SECTOR_SIZE: u64 = 4096;

#[derive(Clone, Debug)]
struct DetectedPort {
    info: SerialPortInfo,
    instance_id: Option<String>,
}

#[cfg(any(windows, test))]
#[derive(Clone, Debug)]
struct PresentPort {
    info: SerialPortInfo,
    instance_id: String,
}

#[derive(Clone, Debug)]
pub struct Port {
    pub name: String,
    /// Windows uses the full PnP instance ID plus COM name; other platforms use
    /// native USB metadata. No VID/PID/product/serial-number deduplication.
    /// A device without a unique serial still needs observed-disconnect invalidation
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
        .map(describe_detected_port)
        .collect();
    ports.sort_by(|a, b| {
        port_number(&a.name)
            .cmp(&port_number(&b.name))
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(ports)
}

/// Operator-requested diagnostics only: metadata enumeration, no serial open,
/// device probe, network request, or automatic telemetry upload.
pub fn port_diagnostics() -> Result<String> {
    #[cfg(windows)]
    {
        let metadata = serialport::available_ports();
        let present = present_ports::entries()?;
        Ok(format_windows_diagnostics(&metadata, &present))
    }
    #[cfg(not(windows))]
    {
        let ports = list_ports()?;
        let mut report = format!(
            "FlashTool {}\nOS={}\nSYSTEM_COM_COUNT={}\n",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            ports.len()
        );
        for port in ports {
            report.push_str(&format!(
                "{}\t{}\tIDENTITY={}\n",
                port.name, port.description, port.identity
            ));
        }
        Ok(report)
    }
}

#[cfg(any(windows, test))]
fn format_windows_diagnostics(
    metadata: &serialport::Result<Vec<SerialPortInfo>>,
    present: &[PresentPort],
) -> String {
    use std::collections::{BTreeMap, BTreeSet};
    let mut nodes = BTreeMap::new();
    for node in present {
        let name = port_name_key(&node.info.port_name);
        if is_com_name(&name) {
            nodes
                .entry((name, node.instance_id.to_ascii_uppercase()))
                .or_insert(node);
        }
    }
    let names: BTreeSet<_> = nodes.keys().map(|(name, _)| name.clone()).collect();
    let library_names: BTreeSet<_> = metadata
        .as_ref()
        .map(|ports| {
            ports
                .iter()
                .map(|port| port_name_key(&port.port_name))
                .collect()
        })
        .unwrap_or_default();
    let missing: Vec<_> = names.difference(&library_names).cloned().collect();
    let stale: Vec<_> = library_names.difference(&names).cloned().collect();
    let mut report = format!(
        "FlashTool {}\nOS=windows\nSOURCE=ConfigManager PRESENT healthy Ports/Modem/COMPORT\nWINDOWS_HEALTHY_COM_COUNT={}\nWINDOWS_PNP_NODE_COUNT={}\n",
        env!("CARGO_PKG_VERSION"),
        names.len(),
        nodes.len(),
    );
    match metadata {
        Ok(ports) => report.push_str(&format!("LIBRARY_METADATA_COUNT={}\n", ports.len())),
        Err(error) => report.push_str(&format!("LIBRARY_METADATA_ERROR={error}\n")),
    }
    report.push_str(&format!(
        "PNP_WITHOUT_LIBRARY_METADATA={}\nLIBRARY_ONLY_NOT_PRESENT={}\n",
        missing.join(","),
        stale.join(",")
    ));
    for ((name, id), node) in nodes {
        let adapter = match &node.info.port_type {
            SerialPortType::UsbPort(usb) => format!("USB VID={:04X} PID={:04X}", usb.vid, usb.pid),
            _ => "USB_METADATA=unavailable".into(),
        };
        report.push_str(&format!("{name}\t{adapter}\tPNP_INSTANCE_ID={id}\n"));
    }
    report.push_str(
        "仅列出 Windows 实际枚举且健康的串口；若系统只报告一个 COM，软件不能创建第二个物理串口。\n",
    );
    report
}

fn available_port_infos() -> Result<Vec<DetectedPort>> {
    #[cfg(windows)]
    {
        // Hotplug discovery must not wait for a second library-wide metadata /
        // SERIALCOMM traversal before querying the authoritative PnP records.
        // PnP already supplies COM, VID/PID, full instance identity and the PID
        // needed by native USB reset. Friendly adapter names derive from VID.
        // Only explicit port_diagnostics() requests run the library comparison.
        present_only_ports(present_ports::entries())
    }
    #[cfg(not(windows))]
    {
        Ok(serialport::available_ports()
            .context("无法枚举串口，请检查操作系统设备管理器")?
            .into_iter()
            .map(|info| DetectedPort {
                info,
                instance_id: None,
            })
            .collect())
    }
}

#[cfg(any(windows, test))]
fn present_only_ports(present: Result<Vec<PresentPort>>) -> Result<Vec<DetectedPort>> {
    merge_present_ports(Ok(Vec::new()), present)
}

#[cfg(any(windows, test))]
fn merge_present_ports(
    metadata: serialport::Result<Vec<SerialPortInfo>>,
    present_ports: Result<Vec<PresentPort>>,
) -> Result<Vec<DetectedPort>> {
    use std::collections::{HashMap, hash_map::Entry};
    let present = present_ports
        .context("无法确认 Windows 当前在线串口；保留上次设备状态")?
        .into_iter();
    let mut known = HashMap::new();
    // Failure of optional friendly/USB metadata must not hide healthy PnP
    // devices. All actual presence/query failures above still propagate.
    for info in metadata.unwrap_or_default() {
        let key = port_name_key(&info.port_name);
        let prefer = matches!(info.port_type, SerialPortType::UsbPort(_));
        match known.entry(key) {
            Entry::Vacant(entry) => {
                entry.insert(info);
            }
            Entry::Occupied(mut entry) if prefer => {
                entry.insert(info);
            }
            Entry::Occupied(_) => {}
        }
    }
    let mut used: HashMap<String, String> = HashMap::new();
    let mut result = Vec::new();
    for current in present {
        let key = port_name_key(&current.info.port_name);
        if !is_com_name(&key) {
            continue;
        }
        if let Some(previous) = used.get(&key) {
            if !previous.eq_ignore_ascii_case(&current.instance_id) {
                bail!(
                    "多台在线设备同时占用 {}，请在 Windows 设备管理器中分配不同 COM 号",
                    current.info.port_name
                );
            }
            continue;
        }
        used.insert(key.clone(), current.instance_id.clone());
        let mut info = known
            .remove(&key)
            .filter(|info| metadata_matches_device(info, &current.info))
            .unwrap_or_else(|| current.info.clone());
        info.port_name = current.info.port_name;
        result.push(DetectedPort {
            info,
            instance_id: Some(current.instance_id),
        });
    }
    Ok(result)
}

#[cfg(any(windows, test))]
fn metadata_matches_device(metadata: &SerialPortInfo, present: &SerialPortInfo) -> bool {
    match (&metadata.port_type, &present.port_type) {
        (SerialPortType::UsbPort(old), SerialPortType::UsbPort(current)) => {
            old.vid == current.vid && old.pid == current.pid
        }
        (SerialPortType::UsbPort(_), _) => false,
        (_, SerialPortType::UsbPort(_)) => false,
        _ => true,
    }
}

#[cfg(any(windows, test))]
fn is_com_name(name: &str) -> bool {
    name.strip_prefix("COM")
        .is_some_and(|number| !number.is_empty() && number.bytes().all(|c| c.is_ascii_digit()))
}

#[cfg(any(windows, test))]
fn serial_info_from_instance(name: String, instance_id: &str) -> SerialPortInfo {
    let upper = instance_id.to_ascii_uppercase();
    let hex_id = |prefix: &str| {
        let start = upper.find(prefix)? + prefix.len();
        u16::from_str_radix(upper.get(start..start + 4)?, 16).ok()
    };
    let usb_ids = if upper.starts_with("USB\\") || upper.starts_with("FTDIBUS\\") {
        hex_id("VID_").zip(hex_id("PID_"))
    } else {
        None
    };
    let port_type = if let Some((vid, pid)) = usb_ids {
        SerialPortType::UsbPort(UsbPortInfo {
            vid,
            pid,
            // An instance ID suffix can be a generated location identifier,
            // not a factory USB serial. Do not fabricate a serial number.
            serial_number: None,
            manufacturer: None,
            product: None,
        })
    } else {
        SerialPortType::Unknown
    };
    SerialPortInfo {
        port_name: name,
        port_type,
    }
}

// Config Manager list sizes can change during hotplug. Bound both allocation
// and retries; a churn/error is not an authoritative empty snapshot.
#[cfg(any(windows, test))]
fn read_pnp_list(
    mut size: impl FnMut() -> Result<u32>,
    mut read: impl FnMut(&mut [u16]) -> Result<bool>,
) -> Result<Vec<String>> {
    for _ in 0..4 {
        let length = size()? as usize;
        anyhow::ensure!(
            (1..=1024 * 1024).contains(&length),
            "Windows PnP 列表长度无效或过大"
        );
        let mut buffer = vec![0_u16; length];
        if !read(&mut buffer)? {
            continue;
        }
        let mut entries = Vec::new();
        let mut position = 0;
        while position < buffer.len() {
            let end = buffer[position..]
                .iter()
                .position(|word| *word == 0)
                .map(|offset| position + offset)
                .context("Windows PnP 列表未终止")?;
            if end == position {
                anyhow::ensure!(
                    buffer[position..].iter().all(|word| *word == 0),
                    "Windows PnP 列表终止后含无效数据"
                );
                return Ok(entries);
            }
            entries.push(
                String::from_utf16(&buffer[position..end])
                    .context("Windows PnP 列表不是有效 UTF-16")?,
            );
            position = end + 1;
        }
        anyhow::bail!("Windows PnP 列表缺少最终终止符");
    }
    anyhow::bail!("Windows 正在变更设备列表，本次扫描未完成；请稍后重试")
}

#[cfg(windows)]
mod present_ports {
    use super::{PresentPort, read_pnp_list, serial_info_from_instance};
    use anyhow::{Context, Result, ensure};
    use std::{
        collections::BTreeSet,
        mem,
        ptr::{null, null_mut},
    };
    use windows_sys::{
        Win32::{
            Devices::{
                DeviceAndDriverInstallation::{
                    CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_GETIDLIST_FILTER_CLASS,
                    CM_GETIDLIST_FILTER_PRESENT, CM_Get_DevNode_Status,
                    CM_Get_Device_ID_List_SizeW, CM_Get_Device_ID_ListW,
                    CM_Get_Device_Interface_List_SizeW, CM_Get_Device_Interface_ListW,
                    CM_Get_Device_Interface_PropertyW, CM_LOCATE_DEVNODE_NORMAL,
                    CM_Locate_DevNodeW, CM_Open_DevNode_Key, CM_REGISTRY_HARDWARE, CR_BUFFER_SMALL,
                    CR_DEVICE_NOT_THERE, CR_NO_SUCH_DEVICE_INTERFACE, CR_NO_SUCH_DEVNODE,
                    CR_NO_SUCH_REGISTRY_KEY, CR_SUCCESS, RegDisposition_OpenExisting,
                },
                Properties::{DEVPKEY_Device_InstanceId, DEVPROP_TYPE_STRING},
            },
            Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND},
            System::Registry::{
                HKEY, KEY_QUERY_VALUE, REG_SZ, RegCloseKey, RegOpenKeyExW, RegQueryValueExW,
            },
        },
        core::GUID,
    };

    const COMPORT: GUID = GUID::from_u128(0x86e0d1e0_8089_11d0_9ce4_08003e301f73);
    const CLASS_FILTERS: [&str; 2] = [
        "{4d36e978-e325-11ce-bfc1-08002be10318}", // Ports
        "{4d36e96d-e325-11ce-bfc1-08002be10318}", // Modem
    ];

    struct DeviceKey(HKEY);
    impl Drop for DeviceKey {
        fn drop(&mut self) {
            unsafe { RegCloseKey(self.0) };
        }
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }
    fn check(result: u32, action: &str) -> Result<()> {
        ensure!(result == CR_SUCCESS, "{action}（CM 错误 0x{result:X}）");
        Ok(())
    }
    fn disappeared(result: u32) -> bool {
        matches!(
            result,
            CR_NO_SUCH_DEVNODE | CR_NO_SUCH_DEVICE_INTERFACE | CR_DEVICE_NOT_THERE
        )
    }

    pub(super) fn entries() -> Result<Vec<PresentPort>> {
        // Read the local PnP manager's present-node cache directly. Constructing
        // three SetupAPI device-information sets per settling pass can wait on
        // device installation/co-installers; it is unnecessary for COM metadata.
        // Preserve Ports + Modem + nonstandard-class COM interfaces, not VID/PID
        // deduplication, registry SERIALCOMM ghosts, or serial I/O probes.
        let mut instances = BTreeSet::new();
        for class in CLASS_FILTERS {
            let filter = wide(class);
            let flags = CM_GETIDLIST_FILTER_CLASS | CM_GETIDLIST_FILTER_PRESENT;
            let list = read_pnp_list(
                || {
                    let mut count = 0;
                    check(
                        unsafe { CM_Get_Device_ID_List_SizeW(&mut count, filter.as_ptr(), flags) },
                        "无法读取当前串口节点列表大小",
                    )?;
                    Ok(count)
                },
                |buffer| {
                    let result = unsafe {
                        CM_Get_Device_ID_ListW(
                            filter.as_ptr(),
                            buffer.as_mut_ptr(),
                            buffer.len() as u32,
                            flags,
                        )
                    };
                    if result == CR_BUFFER_SMALL {
                        return Ok(false);
                    }
                    check(result, "无法读取当前串口节点列表")?;
                    Ok(true)
                },
            )?;
            instances.extend(list);
        }
        let interfaces = read_pnp_list(
            || {
                let mut count = 0;
                check(
                    unsafe {
                        CM_Get_Device_Interface_List_SizeW(
                            &mut count,
                            &COMPORT,
                            null(),
                            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
                        )
                    },
                    "无法读取当前 COM 接口列表大小",
                )?;
                Ok(count)
            },
            |buffer| {
                let result = unsafe {
                    CM_Get_Device_Interface_ListW(
                        &COMPORT,
                        null(),
                        buffer.as_mut_ptr(),
                        buffer.len() as u32,
                        CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
                    )
                };
                if result == CR_BUFFER_SMALL {
                    return Ok(false);
                }
                check(result, "无法读取当前 COM 接口列表")?;
                Ok(true)
            },
        )?;
        for interface in interfaces {
            if let Some(instance) = interface_instance(&interface)? {
                instances.insert(instance);
            }
        }
        let mut ports = Vec::new();
        for instance_id in instances {
            let id = wide(&instance_id);
            let mut node = 0;
            let result =
                unsafe { CM_Locate_DevNodeW(&mut node, id.as_ptr(), CM_LOCATE_DEVNODE_NORMAL) };
            if disappeared(result) {
                continue;
            }
            check(result, "无法定位当前串口节点")?;
            let (mut status, mut problem) = (0, 0);
            let result = unsafe { CM_Get_DevNode_Status(&mut status, &mut problem, node, 0) };
            if disappeared(result) {
                continue;
            }
            check(result, "无法读取当前串口状态")?;
            if problem == 0
                && let Some(name) = port_name(node)?
            {
                ports.push(PresentPort {
                    info: serial_info_from_instance(name, &instance_id),
                    instance_id,
                });
            }
        }
        Ok(ports)
    }

    fn interface_instance(interface: &str) -> Result<Option<String>> {
        let interface = wide(interface);
        let mut buffer = [0_u16; 512];
        let mut byte_len = mem::size_of_val(&buffer) as u32;
        let mut property_type = 0;
        let result = unsafe {
            CM_Get_Device_Interface_PropertyW(
                interface.as_ptr(),
                &DEVPKEY_Device_InstanceId,
                &mut property_type,
                buffer.as_mut_ptr().cast(),
                &mut byte_len,
                0,
            )
        };
        if disappeared(result) {
            return Ok(None);
        }
        check(result, "无法读取 COM 接口对应的设备实例")?;
        ensure!(
            property_type == DEVPROP_TYPE_STRING
                && byte_len >= 2
                && byte_len.is_multiple_of(2)
                && byte_len as usize <= mem::size_of_val(&buffer),
            "COM 接口实例属性类型或长度无效"
        );
        let words = &buffer[..byte_len as usize / 2];
        ensure!(
            words.last() == Some(&0) && !words[..words.len() - 1].contains(&0),
            "COM 接口实例 ID 终止符无效"
        );
        let instance = String::from_utf16(&words[..words.len() - 1])
            .context("COM 接口实例 ID 不是有效 UTF-16")?;
        ensure!(!instance.is_empty(), "COM 接口实例 ID 为空");
        Ok(Some(instance))
    }

    fn port_name(node: u32) -> Result<Option<String>> {
        let mut raw = null_mut();
        let result = unsafe {
            CM_Open_DevNode_Key(
                node,
                KEY_QUERY_VALUE,
                0,
                RegDisposition_OpenExisting,
                &mut raw,
                CM_REGISTRY_HARDWARE,
            )
        };
        if disappeared(result) || result == CR_NO_SUCH_REGISTRY_KEY {
            return Ok(None);
        }
        check(result, "无法读取在线设备的硬件注册表项")?;
        ensure!(
            !raw.is_null() && raw as isize != -1,
            "Windows 返回无效的设备注册表句柄"
        );
        let key = DeviceKey(raw);
        if let Some(name) = query_port_name(&key)? {
            return Ok(Some(name));
        }
        // Some registry APIs/drivers expose the hardware root rather than its
        // Device Parameters child. Both reads are metadata-only and read-only.
        let parameters = wide("Device Parameters");
        let mut child = null_mut();
        let result =
            unsafe { RegOpenKeyExW(key.0, parameters.as_ptr(), 0, KEY_QUERY_VALUE, &mut child) };
        if matches!(result, ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) {
            return Ok(None);
        }
        if result != 0 {
            return Err(std::io::Error::from_raw_os_error(result as i32))
                .context("无法读取设备的 Device Parameters");
        }
        query_port_name(&DeviceKey(child))
    }

    fn query_port_name(key: &DeviceKey) -> Result<Option<String>> {
        let value = wide("PortName");
        let mut buffer = [0_u16; 256];
        let mut byte_len = mem::size_of_val(&buffer) as u32;
        let mut value_type = 0;
        let result = unsafe {
            RegQueryValueExW(
                key.0,
                value.as_ptr(),
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
                && byte_len >= 2
                && byte_len.is_multiple_of(2)
                && byte_len as usize <= mem::size_of_val(&buffer),
            "在线设备的 PortName 类型或长度无效"
        );
        let words = &buffer[..byte_len as usize / 2];
        ensure!(
            words.last() == Some(&0) && !words[..words.len() - 1].contains(&0),
            "在线设备的 PortName 终止符无效"
        );
        let name = String::from_utf16(&words[..words.len() - 1])
            .context("在线设备的 PortName 不是有效 UTF-16")?;
        Ok((!name.is_empty()).then_some(name))
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

fn describe_detected_port(detected: DetectedPort) -> Port {
    let identity = identity_for_detected(&detected.info, detected.instance_id.as_deref());
    let mut port = describe_port(detected.info);
    port.identity = identity;
    port
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

fn identity_for_detected(info: &SerialPortInfo, instance_id: Option<&str>) -> String {
    if let Some(instance_id) = instance_id {
        serde_json::json!({
            "kind": "windows_pnp", "port": port_name_key(&info.port_name),
            "instance_id": instance_id.to_ascii_uppercase(),
        })
        .to_string()
    } else {
        identity_for_info(info)
    }
}

fn open_checked<T>(
    expected: Option<&Port>,
    actual: &SerialPortInfo,
    instance_id: Option<&str>,
    open: impl FnOnce(&SerialPortInfo) -> Result<T>,
) -> Result<T> {
    if let Some(expected) = expected
        && (expected.identity.is_empty()
            || port_name_key(&expected.name) != port_name_key(&actual.port_name)
            || expected.identity != identity_for_detected(actual, instance_id))
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
    Identity(DeviceIdentity),
    /// ESP32-C3's independent 128-bit OPTIONAL_UNIQUE_ID, in the same byte
    /// order as esp_efuse_read_field_blob(..., uid, 128), uppercase 32 hex.
    UniqueDeviceId(String),
    Writing {
        fraction: f32,
        address: u32,
    },
    Verifying,
    Message(String),
}

/// Both fields are representations of the same read-only factory eFuse MAC.
/// This is not an eFuse dump or a separate chip serial/key identifier.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct DeviceIdentity {
    pub efuse_id: Option<String>,
    pub mac: Option<String>,
}

fn factory_identity(mac: Option<&str>) -> Result<DeviceIdentity> {
    let Some(mac) = mac else {
        return Ok(DeviceIdentity::default());
    };
    let parts: Vec<_> = mac.split(':').collect();
    if parts.len() != 6
        || parts
            .iter()
            .any(|part| part.len() != 2 || !part.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        bail!("芯片返回的 factory eFuse MAC 格式无效，未生成设备标识");
    }
    let bytes = parts
        .into_iter()
        .map(|part| u8::from_str_radix(part, 16))
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("芯片返回的 factory eFuse MAC 不是有效十六进制")?;
    if bytes.iter().all(|byte| *byte == 0) || bytes.iter().all(|byte| *byte == 0xff) {
        bail!("芯片的 factory eFuse MAC 未分配或无效，不能作为唯一设备标识");
    }
    Ok(DeviceIdentity {
        efuse_id: Some(
            bytes
                .iter()
                .map(|byte| format!("{byte:02X}"))
                .collect::<String>(),
        ),
        mac: Some(
            bytes
                .iter()
                .map(|byte| format!("{byte:02X}"))
                .collect::<Vec<_>>()
                .join(":"),
        ),
    })
}

fn emit_factory_identity(mac: Option<&str>, callback: &mut dyn FnMut(FlashProgress)) -> Result<()> {
    let identity = factory_identity(mac)?;
    callback(FlashProgress::Identity(identity.clone()));
    if let (Some(efuse_id), Some(mac)) = (&identity.efuse_id, &identity.mac) {
        callback(FlashProgress::Message(
            "芯片标识来源：factory eFuse MAC；EFUSE_ID 为相同 MAC 的原始 48 位".into(),
        ));
        callback(FlashProgress::Message(format!(
            "EFUSE_ID={efuse_id} MAC={mac}"
        )));
    } else {
        callback(FlashProgress::Message(
            "factory eFuse MAC 不可用，未生成设备标识".into(),
        ));
    }
    Ok(())
}

// ESP-IDF v5.5.1: OPTIONAL_UNIQUE_ID is BLK2 bits 0..127, distinct from MAC.
// https://github.com/espressif/esp-idf/blob/v5.5.1/components/efuse/esp32c3/esp_efuse_table.csv#L178
// utility_fill_buff stores each ascending register's least-significant byte
// first, matching pinned espflash read_efuse_le::<[u8; 16]> on our x64 targets:
// https://github.com/espressif/esp-idf/blob/v5.5.1/components/efuse/src/esp_efuse_utility.c#L90
const C3_RS_ERR0_OFFSET: u32 = 0x1c0;
const C3_BLOCK2_RS_FAIL: u32 = 1 << 11;

fn c3_block2_corrections(status: u32) -> Result<u8> {
    // C3 fail bits are shifted relative to error-count fields: BLK2 fail is
    // bit 11, not bit 7; its corrected-byte count is bits 6:4. Do not reject
    // errors in MAC, user-data or key blocks. Corrected shadow bytes remain
    // usable; only the documented uncorrectable flag suppresses this ID.
    // https://github.com/espressif/esp-idf/blob/v5.5.1/components/hal/esp32c3/efuse_hal.c#L83
    if status & C3_BLOCK2_RS_FAIL != 0 {
        bail!("ESP32-C3 eFuse BLOCK2 的 RS 校验报告不可纠正错误，未使用 OPTIONAL_UNIQUE_ID");
    }
    Ok(((status >> 4) & 0x7) as u8)
}

fn optional_unique_id(bytes: [u8; 16]) -> Option<String> {
    // This field is optional: all-zero is unprovisioned. Preserve every other
    // byte pattern, including FF; do not replace missing data with a MAC/hash.
    if bytes.iter().all(|byte| *byte == 0) {
        None
    } else {
        Some(bytes.iter().map(|byte| format!("{byte:02X}")).collect())
    }
}

fn emit_optional_unique_id(
    chip: Chip,
    read: impl FnOnce() -> Result<([u8; 16], u8)>,
    callback: &mut dyn FnMut(FlashProgress),
) {
    if chip != Chip::Esp32c3 {
        return;
    }
    // Optional metadata cannot fail an otherwise healthy flash session. There
    // is one probe and no retry/reset, ROM eFuse command, or eFuse register write.
    match read() {
        Ok((bytes, corrected)) => {
            if corrected != 0 {
                callback(FlashProgress::Message(format!(
                    "OPTIONAL_UNIQUE_ID：BLOCK2 报告 {corrected} 个已纠正错误字节，保留纠正后的读取值"
                )));
            }
            if let Some(id) = optional_unique_id(bytes) {
                callback(FlashProgress::Message(
                    "设备 ID 来源：ESP32-C3 eFuse OPTIONAL_UNIQUE_ID（16 字节）".into(),
                ));
                callback(FlashProgress::UniqueDeviceId(id));
            } else {
                callback(FlashProgress::Message(
                    "ESP32-C3 OPTIONAL_UNIQUE_ID 全为 0，视为未配置".into(),
                ));
            }
        }
        Err(error) => callback(FlashProgress::Message(format!(
            "ESP32-C3 OPTIONAL_UNIQUE_ID 不可用：{error:#}"
        ))),
    }
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
        .find(|detected| port_name_key(&detected.info.port_name) == port_name_key(port))
        .with_context(|| format!("串口 {port} 已断开或不存在，请刷新设备列表"))?;
    let serial = open_checked(
        expected,
        &info.info,
        info.instance_id.as_deref(),
        |actual| {
            serialport::new(&actual.port_name, 115_200)
                .flow_control(FlowControl::None)
                .timeout(Duration::from_secs(3))
                .open_native()
                .with_context(|| format!("无法打开 {port}，请检查驱动、连接及串口占用"))
        },
    )?;
    let usb = match info.info.port_type {
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
    // espflash Chip::mac_address reads only the chip-specific MAC0/MAC1 eFuse
    // fields. Do not read secret blocks, change keys, or program any eFuse.
    emit_factory_identity(device.mac_address.as_deref(), callback)?;
    emit_optional_unique_id(
        target_chip,
        || {
            let status = flasher
                .connection()
                .read_reg(target_chip.efuse_reg() + C3_RS_ERR0_OFFSET)
                .context("无法读取 BLOCK2 RS 状态")?;
            let corrected = c3_block2_corrections(status)?;
            // This public pinned API issues exactly four ReadReg operations
            // for this 128-bit aligned field, using existing command timeouts.
            // RD_DIS protects BLK4-10, not BLK2; do not read keys or interpret
            // unrelated read-protection bits as protection of this public ID.
            let bytes = target_chip
                .read_efuse_le::<[u8; 16]>(
                    flasher.connection(),
                    espflash::target::efuse::esp32c3::OPTIONAL_UNIQUE_ID,
                )
                .context("无法读取 ESP_EFUSE_OPTIONAL_UNIQUE_ID 的 16 字节")?;
            Ok((bytes, corrected))
        },
        callback,
    );
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

    #[test]
    fn pinned_optional_unique_id_layout_and_blob_byte_order_match_esp_idf() {
        let field = espflash::target::efuse::esp32c3::OPTIONAL_UNIQUE_ID;
        assert_eq!(
            (field.block, field.word, field.bit_start, field.bit_count),
            (2, 0, 0, 128)
        );
        assert_eq!(Chip::Esp32c3.efuse_reg(), 0x6000_8800);
        // Synthetic register image. IDF fill_buff emits each low byte first;
        // formatting each word as eight hex digits or a u128 reverses the ID.
        let words = [0x0123_4567_u32, 0x89ab_cdef, 0x1020_3040, 0xaabb_ccdd];
        let bytes: [u8; 16] = words
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .collect::<Vec<_>>()
            .try_into()
            .unwrap();
        assert_eq!(
            optional_unique_id(bytes).as_deref(),
            Some("67452301EFCDAB8940302010DDCCBBAA")
        );
        let leading_zero = [0x3322_1100_u32, 0x7766_5544, 0xbbaa_9988, 0xffee_ddcc];
        let bytes: [u8; 16] = leading_zero
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .collect::<Vec<_>>()
            .try_into()
            .unwrap();
        assert_eq!(
            optional_unique_id(bytes).as_deref(),
            Some("00112233445566778899AABBCCDDEEFF")
        );
    }

    #[test]
    fn optional_unique_id_is_emitted_once_and_preserves_all_nonzero_patterns() {
        for bytes in [[0x12; 16], [0xff; 16]] {
            let mut calls = 0;
            let mut events = Vec::new();
            emit_optional_unique_id(
                Chip::Esp32c3,
                || {
                    calls += 1;
                    Ok((bytes, 0))
                },
                &mut |event| events.push(event),
            );
            let ids: Vec<_> = events
                .iter()
                .filter_map(|event| match event {
                    FlashProgress::UniqueDeviceId(id) => Some(id),
                    _ => None,
                })
                .collect();
            assert_eq!(calls, 1);
            assert_eq!(ids.len(), 1);
            assert_eq!(ids[0].len(), 32);
            assert_eq!(
                *ids[0],
                bytes
                    .iter()
                    .map(|byte| format!("{byte:02X}"))
                    .collect::<String>()
            );
        }
    }

    #[test]
    fn blank_or_failed_optional_id_is_diagnostic_only_and_never_becomes_a_mac() {
        for (result, reason) in [
            (Ok(([0; 16], 0)), "未配置"),
            (
                Err(anyhow::anyhow!("synthetic read timeout")),
                "synthetic read timeout",
            ),
        ] {
            let mut events = Vec::new();
            emit_optional_unique_id(Chip::Esp32c3, || result, &mut |event| events.push(event));
            assert!(matches!(
                events.as_slice(),
                [FlashProgress::Message(message)] if message.contains(reason)
            ));
        }
    }

    #[test]
    fn only_block2_uncorrectable_rs_flag_suppresses_the_optional_id() {
        assert!(c3_block2_corrections(1 << 11).is_err());
        assert_eq!(
            c3_block2_corrections(1 << 7).unwrap(),
            0,
            "C3 bit7 belongs to another block"
        );
        let unrelated = !(C3_BLOCK2_RS_FAIL | (7 << 4));
        assert_eq!(c3_block2_corrections(unrelated).unwrap(), 0);
        let mut events = Vec::new();
        emit_optional_unique_id(
            Chip::Esp32c3,
            || Ok(([0x34; 16], c3_block2_corrections(unrelated | (2 << 4))?)),
            &mut |event| events.push(event),
        );
        assert!(events.iter().any(|event| matches!(event, FlashProgress::UniqueDeviceId(id) if id == "34343434343434343434343434343434")));
        assert!(events.iter().any(
            |event| matches!(event, FlashProgress::Message(message) if message.contains("已纠正"))
        ));
        let mut events = Vec::new();
        emit_optional_unique_id(
            Chip::Esp32c3,
            || Ok(([0x34; 16], c3_block2_corrections(1 << 11)?)),
            &mut |event| events.push(event),
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, FlashProgress::UniqueDeviceId(_)))
        );
    }

    #[test]
    fn other_chips_never_read_c3_registers_or_fabricate_optional_ids() {
        for chip in [
            Chip::Esp32,
            Chip::Esp32s2,
            Chip::Esp32s3,
            Chip::Esp32c2,
            Chip::Esp32c6,
            Chip::Esp32h2,
        ] {
            emit_optional_unique_id(
                chip,
                || panic!("must not read C3 fields from another chip"),
                &mut |_| panic!("unsupported chip must not emit an ID"),
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn native_config_manager_enumeration_returns_a_valid_snapshot_without_opening_ports() {
        let ports = list_ports().expect("Windows present Ports/Modem/COMPORT metadata enumeration");
        let mut identities = std::collections::BTreeSet::new();
        for port in ports {
            assert!(is_com_name(&port_name_key(&port.name)));
            assert!(identities.insert(port.identity));
        }
    }

    #[test]
    fn config_manager_buffer_growth_is_retried_and_a_burst_preserves_every_instance() {
        let calls = std::cell::Cell::new(0);
        let contents: Vec<_> = "USB\\VID_10C4&PID_EA60\\A\0USB\\VID_10C4&PID_EA60\\B\0\0"
            .encode_utf16()
            .collect();
        let ports = read_pnp_list(
            || Ok(contents.len() as u32),
            |buffer| {
                calls.set(calls.get() + 1);
                if calls.get() == 1 {
                    return Ok(false);
                }
                buffer.copy_from_slice(&contents);
                Ok(true)
            },
        )
        .unwrap();
        assert_eq!(calls.get(), 2);
        assert_eq!(ports.len(), 2);
        assert_ne!(ports[0], ports[1]);
    }

    #[test]
    fn config_manager_churn_and_errors_never_become_empty_snapshots_or_unbounded_retries() {
        let mut calls = 0;
        assert!(
            read_pnp_list(
                || Ok(2),
                |_| {
                    calls += 1;
                    Ok(false)
                }
            )
            .is_err()
        );
        assert_eq!(calls, 4);
        assert!(
            read_pnp_list(
                || anyhow::bail!("access denied"),
                |_| panic!("must not read after size failure")
            )
            .unwrap_err()
            .to_string()
            .contains("access denied")
        );
        assert!(
            read_pnp_list(
                || Ok(u32::MAX),
                |_| panic!("must not allocate an unbounded list")
            )
            .is_err()
        );
        assert!(
            read_pnp_list(
                || Ok(3),
                |buffer| {
                    buffer.copy_from_slice(&[0xd800, 0, 0]);
                    Ok(true)
                }
            )
            .is_err()
        );
        assert!(
            read_pnp_list(
                || Ok(2),
                |buffer| {
                    buffer.copy_from_slice(&[0, 0]);
                    Ok(true)
                }
            )
            .unwrap()
            .is_empty()
        );
    }

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

    fn present(names: &[&str]) -> Result<Vec<PresentPort>> {
        Ok(names
            .iter()
            .map(|name| PresentPort {
                info: serial_info_from_instance((*name).to_owned(), &format!("ROOT\\PORT\\{name}")),
                instance_id: format!("ROOT\\PORT\\{name}"),
            })
            .collect())
    }

    fn retain_present_ports(
        ports: Vec<SerialPortInfo>,
        present: Result<Vec<PresentPort>>,
    ) -> Result<Vec<SerialPortInfo>> {
        Ok(merge_present_ports(Ok(ports), present)?
            .into_iter()
            .map(|port| port.info)
            .collect())
    }

    fn pnp(name: &str, instance: &str) -> PresentPort {
        PresentPort {
            info: serial_info_from_instance(name.into(), instance),
            instance_id: instance.into(),
        }
    }

    #[test]
    fn every_distinct_com_is_preserved_for_identical_drivers_and_duplicate_or_missing_usb_serials()
    {
        for serial in [None, Some("cloned-driver-serial")] {
            let first = usb_info("COM3", serial);
            let second = usb_info("COM4", serial);
            assert_eq!(first.port_type, second.port_type);
            let ports = merge_present_ports(
                Ok(vec![first, second]),
                Ok(vec![
                    pnp("COM3", "USB\\VID_10C4&PID_EA60\\NODE_A"),
                    pnp("COM4", "USB\\VID_10C4&PID_EA60\\NODE_B"),
                ]),
            )
            .unwrap();
            assert_eq!(ports.len(), 2);
            let first = describe_detected_port(ports[0].clone());
            let second = describe_detected_port(ports[1].clone());
            assert_ne!(first.name, second.name);
            assert_ne!(first.identity, second.identity);
            assert!(first.usb && second.usb && first.suggested && second.suggested);
        }
    }

    #[test]
    fn supplements_a_missing_library_port_from_authoritative_pnp_without_a_fake_usb_serial() {
        let ports = merge_present_ports(
            Ok(vec![usb_info("COM3", None)]),
            Ok(vec![
                pnp("COM3", "USB\\VID_10C4&PID_EA60\\NODE_A"),
                pnp("COM4", "USB\\VID_10C4&PID_EA60\\NODE_B"),
            ]),
        )
        .unwrap();
        assert_eq!(ports.len(), 2);
        let extra = ports
            .iter()
            .find(|port| port.info.port_name == "COM4")
            .unwrap();
        let SerialPortType::UsbPort(usb) = &extra.info.port_type else {
            panic!("PnP USB IDs must identify the adapter")
        };
        assert_eq!((usb.vid, usb.pid), (0x10c4, 0xea60));
        assert!(usb.serial_number.is_none());
    }

    #[test]
    fn optional_library_metadata_failure_does_not_hide_authoritative_present_devices() {
        let ports = merge_present_ports(
            Err(serialport::Error::new(
                serialport::ErrorKind::Unknown,
                "metadata unavailable",
            )),
            Ok(vec![pnp("COM4", "USB\\VID_1A86&PID_55D4\\NODE_B")]),
        )
        .unwrap();
        let port = describe_detected_port(ports[0].clone());
        assert_eq!(port.name, "COM4");
        assert!(port.usb && port.suggested);
    }

    #[test]
    fn fast_discovery_is_complete_without_optional_library_metadata_and_keeps_native_usb_reset_pid()
    {
        let ports = present_only_ports(Ok(vec![
            pnp("COM3", "USB\\VID_303A&PID_1001&MI_00\\NATIVE_A"),
            pnp("COM4", "USB\\VID_10C4&PID_EA60\\UART_B"),
        ]))
        .unwrap();
        assert_eq!(ports.len(), 2);
        let SerialPortType::UsbPort(native) = &ports[0].info.port_type else {
            panic!("native USB metadata required")
        };
        assert_eq!((native.vid, native.pid), (0x303a, 0x1001));
        for detected in ports {
            let instance = detected.instance_id.clone().unwrap();
            let port = describe_detected_port(detected.clone());
            assert!(port.usb && port.suggested);
            assert!(port.identity.contains("windows_pnp"));
            open_checked(Some(&port), &detected.info, Some(&instance), |_| Ok(())).unwrap();
        }
    }

    #[test]
    fn stale_library_usb_metadata_cannot_choose_the_wrong_reset_pid_for_a_replaced_com() {
        let old = SerialPortInfo {
            port_name: "COM3".into(),
            port_type: SerialPortType::UsbPort(UsbPortInfo {
                vid: 0x303a,
                pid: 0x1001,
                serial_number: None,
                manufacturer: None,
                product: None,
            }),
        };
        let ports = merge_present_ports(
            Ok(vec![old.clone()]),
            Ok(vec![pnp("COM3", "USB\\VID_10C4&PID_EA60\\REPLACEMENT")]),
        )
        .unwrap();
        let SerialPortType::UsbPort(actual) = &ports[0].info.port_type else {
            panic!("USB metadata expected")
        };
        assert_eq!((actual.vid, actual.pid), (0x10c4, 0xea60));
        let non_usb =
            merge_present_ports(Ok(vec![old]), Ok(vec![pnp("COM3", "ACPI\\PNP0501\\UART")]))
                .unwrap();
        assert!(matches!(non_usb[0].info.port_type, SerialPortType::Unknown));
    }

    #[test]
    fn duplicate_pnp_views_merge_only_the_same_com_and_same_instance() {
        let node = pnp("COM3", "USB\\VID_10C4&PID_EA60\\NODE_A");
        assert_eq!(
            merge_present_ports(Ok(vec![]), Ok(vec![node.clone(), node.clone()]))
                .unwrap()
                .len(),
            1
        );
        let conflict = pnp("COM3", "USB\\VID_10C4&PID_EA60\\NODE_B");
        assert!(merge_present_ports(Ok(vec![]), Ok(vec![node, conflict])).is_err());
    }

    #[test]
    fn pnp_identity_survives_metadata_enrichment_but_rejects_a_replacement_before_opening() {
        let old = pnp("COM3", "USB\\VID_10C4&PID_EA60\\NODE_A");
        let expected = describe_detected_port(DetectedPort {
            info: old.info.clone(),
            instance_id: Some(old.instance_id.clone()),
        });
        let metadata = usb_info("COM3", Some("newly available friendly metadata"));
        let mut opened = false;
        open_checked(Some(&expected), &metadata, Some(&old.instance_id), |_| {
            opened = true;
            Ok(())
        })
        .unwrap();
        assert!(opened);
        opened = false;
        assert!(
            open_checked(
                Some(&expected),
                &metadata,
                Some("USB\\VID_10C4&PID_EA60\\NODE_B"),
                |_| {
                    opened = true;
                    Ok(())
                }
            )
            .is_err()
        );
        assert!(!opened);
    }

    #[test]
    fn factory_identifiers_emit_typed_metadata_and_the_canonical_log_line() {
        let mut events = Vec::new();
        emit_factory_identity(Some("aa:bb:0c:00:01:02"), &mut |event| events.push(event)).unwrap();
        let FlashProgress::Identity(identity) = &events[0] else {
            panic!("typed identity must be emitted")
        };
        assert_eq!(identity.efuse_id.as_deref(), Some("AABB0C000102"));
        assert_eq!(identity.mac.as_deref(), Some("AA:BB:0C:00:01:02"));
        assert!(events.iter().any(|event| matches!(event, FlashProgress::Message(line) if line == "EFUSE_ID=AABB0C000102 MAC=AA:BB:0C:00:01:02")));
        assert_eq!(factory_identity(None).unwrap(), DeviceIdentity::default());
    }

    #[test]
    fn invalid_or_unassigned_factory_mac_does_not_fabricate_an_identifier() {
        for invalid in [
            "00:00:00:00:00:00",
            "ff:ff:ff:ff:ff:ff",
            "1:22:33:44:55:66",
            "+1:22:33:44:55:66",
            "not-a-mac",
        ] {
            assert!(factory_identity(Some(invalid)).is_err());
        }
    }

    #[test]
    fn diagnostics_report_only_the_com_devices_actually_enumerated_by_windows() {
        let metadata = Ok(vec![usb_info("COM3", None), usb_info("COM4", None)]);
        let report =
            format_windows_diagnostics(&metadata, &[pnp("COM3", "USB\\VID_10C4&PID_EA60\\NODE_A")]);
        assert!(report.contains("WINDOWS_HEALTHY_COM_COUNT=1\n"));
        assert!(report.contains("LIBRARY_ONLY_NOT_PRESENT=COM4\n"));
        assert!(!report.lines().any(|line| line.starts_with("COM4\t")));
    }

    #[test]
    fn diagnostics_explain_missing_library_metadata_without_hiding_the_second_present_com() {
        let metadata = Ok(vec![usb_info("COM3", None)]);
        let report = format_windows_diagnostics(
            &metadata,
            &[
                pnp("COM3", "USB\\VID_10C4&PID_EA60\\NODE_A"),
                pnp("COM4", "USB\\VID_10C4&PID_EA60\\NODE_B"),
            ],
        );
        assert!(report.contains("WINDOWS_HEALTHY_COM_COUNT=2\n"));
        assert!(report.contains("PNP_WITHOUT_LIBRARY_METADATA=COM4\n"));
        assert!(
            report
                .lines()
                .any(|line| line.starts_with("COM4\tUSB VID=10C4 PID=EA60"))
        );
    }

    #[test]
    fn excludes_registry_only_ghost_com_without_dropping_present_usb() {
        let ghost = SerialPortInfo {
            port_name: "COM99".into(),
            port_type: SerialPortType::Unknown,
        };
        let ports = retain_present_ports(
            vec![usb_info("COM3", Some("A")), ghost],
            Ok(vec![pnp("COM3", "USB\\VID_10C4&PID_EA60\\A")]),
        )
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
        let connected = retain_present_ports(
            vec![usb_info("COM3", Some("original"))],
            Ok(vec![pnp("COM3", "USB\\VID_10C4&PID_EA60\\ORIGINAL")]),
        )
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
            Ok(vec![pnp("COM3", "USB\\VID_10C4&PID_EA60\\REPLACEMENT")]),
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
        let result = open_checked(Some(&expected), &replacement, None, |_| {
            opened = true;
            Ok(())
        });
        assert!(result.is_err());
        assert!(!opened);
        let result = open_checked(
            Some(&expected),
            &usb_info("COM3", Some("device-A")),
            None,
            |_| {
                opened = true;
                Ok(())
            },
        );
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
