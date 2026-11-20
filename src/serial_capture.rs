//! Read running firmware's serial console without bootloader commands or flash
//! operations. The caller owns concurrency and keeps each callback per port.
//!
//! Reset is opt-in. Opening a serial device can itself change modem-line states
//! in its OS/driver; `reset = false` promises no explicit reset pulse or serial
//! output, rather than a hardware guarantee about opening/closing a UART.
use crate::backend::{self, Port};
use anyhow::{Context, Result, ensure};
use espflash::connection::{Connection, ResetAfterOperation, ResetBeforeOperation};
use serialport::{DataBits, FlowControl, Parity, SerialPort, StopBits, UsbPortInfo};
use std::{
    io::{self, Read},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

/// Hard limits apply independently to every port/session, including sessions
/// that never receive a newline. Reaching any limit is explicit in the result.
pub const MAX_CAPTURE_BYTES: usize = 512 * 1024;
/// UTF-8 callback log bytes, counting one normalized LF per line. Reserve
/// space for the caller's fixed header in its 512 KiB device log buffer.
pub const MAX_CAPTURE_DECODED_BYTES: usize = MAX_CAPTURE_BYTES - 128;
pub const MAX_CAPTURE_LINES: usize = 8_192;
pub const MAX_LINE_BYTES: usize = 16 * 1024;
const READ_SLICE: Duration = Duration::from_millis(100);

#[derive(Clone, Debug)]
pub struct CaptureOptions {
    pub baud: u32,
    /// Listening time after opening and an optional reset, not an OS-driver
    /// deadline for enumeration/open. Every read has a <=100 ms timeout.
    pub timeout: Duration,
    /// Explicit normal-boot hard reset, never a flash/download-mode handshake.
    /// UART adapters need a compatible RTS-to-EN reset circuit. Native USB can
    /// disappear/re-enumerate; an unsupported modem control returns an error.
    pub reset: bool,
}

impl Default for CaptureOptions {
    fn default() -> Self {
        Self {
            baud: 115_200,
            timeout: Duration::from_secs(10),
            reset: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureEvent {
    Opened,
    Resetting,
    /// CR, LF and CRLF become one line boundary. An unterminated last line is
    /// delivered on timeout/cancellation. Invalid UTF-8 is replaced only once
    /// a complete line (or final fragment) has been assembled.
    Line(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureEnd {
    TimedOut,
    Stopped,
    Cancelled,
    LimitReached,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureLimit {
    TotalBytes,
    DecodedBytes,
    LineCount,
    LineBytes,
}

#[derive(Clone, Debug)]
pub struct CaptureSummary {
    pub bytes_read: usize,
    /// Exactly sum(line.len() + 1) for emitted Line callbacks, after lossy
    /// UTF-8 decoding. This also bounds replacement-character expansion.
    pub decoded_bytes: usize,
    pub lines: usize,
    pub elapsed: Duration,
    pub end: CaptureEnd,
    /// Set only for LimitReached. Such an incomplete console must not be
    /// reported as a successful unique-ID extraction without caller review.
    pub limit: Option<CaptureLimit>,
}

/// Capture one frozen device identity. Call from a worker, never the UI thread.
/// Callback `true` continues; `false` stops immediately (e.g. required fields
/// were captured). No UART bytes are written, no buffers are purged and no ROM,
/// stub, erase, flash or eFuse command is sent. Errors retain already streamed
/// lines in the caller; a missing/changed device is rejected before opening it.
pub fn capture(
    expected: &Port,
    options: &CaptureOptions,
    cancel: &AtomicBool,
    callback: &mut dyn FnMut(CaptureEvent) -> bool,
) -> Result<CaptureSummary> {
    capture_impl(expected, options, cancel, callback, false)
}

/// Reset this frozen device into normal firmware boot, then listen for up to
/// ten seconds. The native handle is open before the pulse and RX is never
/// purged. A compatible UART reset circuit maps deasserted DTR to released
/// GPIO9 and RTS to EN. Arbitrary/custom wiring cannot be inferred from VID/PID.
/// Native Espressif USB keeps its dedicated reset sequence; if it disappears,
/// this operation reports the error rather than rebinding a different device.
pub fn capture_startup_log(
    expected: &Port,
    baud: u32,
    cancel: &AtomicBool,
    callback: &mut dyn FnMut(CaptureEvent) -> bool,
) -> Result<CaptureSummary> {
    let options = CaptureOptions {
        baud,
        reset: true,
        ..CaptureOptions::default()
    };
    capture_impl(expected, &options, cancel, callback, true)
}

fn capture_impl(
    expected: &Port,
    options: &CaptureOptions,
    cancel: &AtomicBool,
    callback: &mut dyn FnMut(CaptureEvent) -> bool,
    startup_reset: bool,
) -> Result<CaptureSummary> {
    validate_options(options)?;
    if cancel.load(Ordering::Acquire) {
        return Ok(empty_summary(CaptureEnd::Cancelled));
    }
    let current = backend::list_ports().context("监听前无法确认当前在线串口")?;
    let actual = expected_port(expected, &current)?;
    if cancel.load(Ordering::Acquire) {
        return Ok(empty_summary(CaptureEnd::Cancelled));
    }
    let mut serial = serialport::new(&actual.name, options.baud)
        .data_bits(DataBits::Eight)
        .parity(Parity::None)
        .stop_bits(StopBits::One)
        .flow_control(FlowControl::None)
        .preserve_dtr_on_open()
        .timeout(READ_SLICE)
        .open_native()
        .with_context(|| {
            format!(
                "无法打开 {} 读取运行日志，请检查驱动、连接或其他程序占用",
                actual.name
            )
        })?;
    if startup_reset {
        // Opening can race a hotplug/reassignment after the first snapshot.
        // Revalidate the full frozen identity before any explicit modem pulse.
        // Keep the handle; do not retry or bind a newly enumerated replacement.
        let after_open = backend::list_ports().context("打开后无法再次确认启动复位设备身份")?;
        expected_port(expected, &after_open)?;
    }
    if !callback(CaptureEvent::Opened) {
        return Ok(empty_summary(CaptureEnd::Stopped));
    }
    if options.reset {
        if cancel.load(Ordering::Acquire) {
            return Ok(empty_summary(CaptureEnd::Cancelled));
        }
        if !callback(CaptureEvent::Resetting) {
            return Ok(empty_summary(CaptureEnd::Stopped));
        }
        if startup_reset {
            let native_usb = reset_usb_info(actual).pid == 0x1001;
            let outcome = startup_reset_lines(&mut serial, native_usb, cancel, thread::sleep)
                .with_context(|| {
                    format!(
                        "{} 启动复位失败。请检查自动复位接线。原生 USB 端口可能已重新枚举",
                        actual.name
                    )
                })?;
            if outcome == ResetOutcome::Cancelled {
                return Ok(empty_summary(CaptureEnd::Cancelled));
            }
        } else {
            // Preserve the existing batch-read reset behavior. In pinned
            // espflash 4.7.0 this only changes modem controls and sleeps.
            // Do not call begin/reset_to_flash/sync/command on the connection.
            let mut connection = Connection::new(
                serial,
                reset_usb_info(actual),
                ResetAfterOperation::HardReset,
                ResetBeforeOperation::NoResetNoSync,
                options.baud,
            );
            connection.reset().with_context(|| format!("{} 显式复位失败；该端口可能不支持 DTR/RTS 或已重新枚举。请关闭自动复位，手动按复位键后重试", actual.name))?;
            serial = connection.into_serial();
        }
    }
    capture_reader(&mut serial, options, cancel, callback)
        .with_context(|| format!("{} 串口日志采集失败", actual.name))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResetOutcome {
    Complete,
    Cancelled,
}

trait ResetLines {
    fn dtr(&mut self, asserted: bool) -> Result<()>;
    fn rts(&mut self, asserted: bool) -> Result<()>;
}
impl<T: SerialPort> ResetLines for T {
    fn dtr(&mut self, asserted: bool) -> Result<()> {
        Ok(self.write_data_terminal_ready(asserted)?)
    }
    fn rts(&mut self, asserted: bool) -> Result<()> {
        Ok(self.write_request_to_send(asserted)?)
    }
}

struct ReleaseEn<'a, T: ResetLines> {
    lines: &'a mut T,
    armed: bool,
}
impl<T: ResetLines> ReleaseEn<'_, T> {
    fn release(&mut self) -> Result<()> {
        self.lines
            .rts(false)
            .context("释放 RTS/EN 失败，请检查连接并手动复位设备")?;
        self.armed = false;
        Ok(())
    }
}
impl<T: ResetLines> Drop for ReleaseEn<'_, T> {
    fn drop(&mut self) {
        if self.armed {
            // Also release on panic/unwind or a failed first release attempt.
            // A disconnected driver can reject this; never report that as a
            // successful reset/cancellation through the normal Result path.
            let _ = self.lines.rts(false);
        }
    }
}

fn startup_reset_lines(
    lines: &mut impl ResetLines,
    native_usb: bool,
    cancel: &AtomicBool,
    mut wait: impl FnMut(Duration),
) -> Result<ResetOutcome> {
    if cancel.load(Ordering::Acquire) {
        return Ok(ResetOutcome::Cancelled);
    }
    let mut guard = ReleaseEn { lines, armed: true };
    let mut settle = || {
        for _ in 0..10 {
            if cancel.load(Ordering::Acquire) {
                return false;
            }
            wait(Duration::from_millis(10));
        }
        !cancel.load(Ordering::Acquire)
    };
    let result = (|| -> Result<ResetOutcome> {
        if native_usb {
            // Exact order from pinned espflash 4.7.0 reset_after_flash() for
            // Espressif VID303A/PID1001, with cancellable waits and cleanup.
            if !settle() {
                return Ok(ResetOutcome::Cancelled);
            }
            guard.lines.dtr(false).context("原生 USB 释放 DTR 失败")?;
            if !settle() {
                return Ok(ResetOutcome::Cancelled);
            }
            guard.lines.rts(true).context("原生 USB 复位 RTS 失败")?;
            guard
                .lines
                .dtr(false)
                .context("原生 USB 复位期间释放 DTR 失败")?;
            guard
                .lines
                .rts(true)
                .context("原生 USB 复位 RTS 确认失败")?;
        } else {
            // Unlike a download-mode reset, GPIO9 must remain released while
            // EN rises. DTR polarity assumes the standard ESP auto-reset circuit.
            guard
                .lines
                .dtr(false)
                .context("释放 DTR/BOOT(GPIO9) 失败")?;
            if !settle() {
                return Ok(ResetOutcome::Cancelled);
            }
            guard.lines.rts(true).context("拉低 RTS/EN 复位失败")?;
        }
        if !settle() {
            return Ok(ResetOutcome::Cancelled);
        }
        Ok(ResetOutcome::Complete)
    })();
    let released = guard.release();
    match (result, released) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(release_error)) => Err(error.context(format!("{release_error:#}"))),
    }
}

fn validate_options(options: &CaptureOptions) -> Result<()> {
    ensure!(options.baud > 0, "日志波特率必须大于 0");
    ensure!(!options.timeout.is_zero(), "日志监听超时必须大于 0");
    ensure!(
        Instant::now().checked_add(options.timeout).is_some(),
        "日志监听超时时间过大"
    );
    Ok(())
}

fn expected_port<'a>(expected: &Port, current: &'a [Port]) -> Result<&'a Port> {
    ensure!(
        !expected.name.trim().is_empty() && !expected.identity.is_empty(),
        "未选择有效的串口设备身份"
    );
    let actual = current
        .iter()
        .find(|port| backend::port_name_key(&port.name) == backend::port_name_key(&expected.name))
        .with_context(|| format!("{} 已断开或不存在，请刷新后重新选择", expected.name))?;
    ensure!(
        actual.identity == expected.identity,
        "{} 的设备身份已变化，本次日志采集已停止，请重新选择设备",
        expected.name
    );
    Ok(actual)
}

fn reset_usb_info(port: &Port) -> UsbPortInfo {
    // Port.identity is generated by backend. Never infer native USB from a
    // display description or fabricate a USB serial from a location suffix.
    let value = serde_json::from_str::<serde_json::Value>(&port.identity).unwrap_or_default();
    let ids = value
        .get("vid")
        .and_then(serde_json::Value::as_u64)
        .zip(value.get("pid").and_then(serde_json::Value::as_u64))
        .and_then(|(vid, pid)| Some((u16::try_from(vid).ok()?, u16::try_from(pid).ok()?)))
        .or_else(|| {
            let id = value.get("instance_id")?.as_str()?.to_ascii_uppercase();
            if !id.starts_with("USB\\") {
                return None;
            }
            let parse = |key: &str| {
                let start = id.find(key)? + key.len();
                u16::from_str_radix(id.get(start..start + 4)?, 16).ok()
            };
            parse("VID_").zip(parse("PID_"))
        })
        .unwrap_or((0, 0));
    // espflash selects its USB Serial/JTAG reset solely by PID 0x1001. Restrict
    // that sequence to the actual Espressif VID so unrelated adapters cannot
    // accidentally receive a native-USB reset sequence.
    let (vid, pid) = if ids == (0x303a, 0x1001) { ids } else { (0, 0) };
    UsbPortInfo {
        vid,
        pid,
        serial_number: None,
        manufacturer: None,
        product: None,
    }
}

fn empty_summary(end: CaptureEnd) -> CaptureSummary {
    CaptureSummary {
        bytes_read: 0,
        decoded_bytes: 0,
        lines: 0,
        elapsed: Duration::ZERO,
        end,
        limit: None,
    }
}

trait CaptureRead: Read {
    fn read_timeout(&mut self, timeout: Duration) -> Result<()>;
}
impl<T: SerialPort> CaptureRead for T {
    fn read_timeout(&mut self, timeout: Duration) -> Result<()> {
        Ok(self.set_timeout(timeout)?)
    }
}

fn capture_reader(
    serial: &mut impl CaptureRead,
    options: &CaptureOptions,
    cancel: &AtomicBool,
    callback: &mut dyn FnMut(CaptureEvent) -> bool,
) -> Result<CaptureSummary> {
    let start = Instant::now();
    let deadline = start
        .checked_add(options.timeout)
        .context("日志监听超时时间过大")?;
    let mut summary = empty_summary(CaptureEnd::TimedOut);
    let mut lines = LineDecoder::default();
    let mut buffer = [0_u8; 4_096];
    loop {
        if cancel.load(Ordering::Acquire) {
            summary.end = CaptureEnd::Cancelled;
            break;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        // Windows serialport rounds to milliseconds. Never let a final
        // sub-millisecond slice turn into the special zero-timeout setting.
        serial
            .read_timeout(remaining.min(READ_SLICE).max(Duration::from_millis(1)))
            .context("设置串口读取超时失败")?;
        let capacity = buffer.len().min(MAX_CAPTURE_BYTES - summary.bytes_read);
        if capacity == 0 {
            summary.end = CaptureEnd::LimitReached;
            summary.limit = Some(CaptureLimit::TotalBytes);
            break;
        }
        let length = match serial.read(&mut buffer[..capacity]) {
            Ok(length) => length,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::Interrupted
                ) =>
            {
                if error.kind() != io::ErrorKind::Interrupted {
                    thread::sleep(Duration::from_millis(1).min(remaining));
                }
                continue;
            }
            Err(error) => {
                if !lines.bytes.is_empty() {
                    emit_line(&mut lines.bytes, &mut summary, callback);
                }
                return Err(error).context("读取串口失败，设备可能已拔出或驱动断开");
            }
        };
        if length == 0 {
            // Avoid a busy loop with drivers that return immediately on no data.
            thread::sleep(Duration::from_millis(1).min(remaining));
            continue;
        }
        summary.bytes_read += length;
        if let Some(end) = lines.feed(&buffer[..length], &mut summary, callback) {
            summary.end = end;
            break;
        }
        if summary.bytes_read == MAX_CAPTURE_BYTES {
            summary.end = CaptureEnd::LimitReached;
            summary.limit = Some(CaptureLimit::TotalBytes);
            break;
        }
    }
    // Do not turn an oversized/partial line into an apparently complete ID.
    if summary.end != CaptureEnd::LimitReached
        && summary.end != CaptureEnd::Stopped
        && !lines.bytes.is_empty()
    {
        let keep_reading = emit_line(&mut lines.bytes, &mut summary, callback);
        if summary.limit.is_some() {
            summary.end = CaptureEnd::LimitReached;
        } else if !keep_reading && summary.end != CaptureEnd::Cancelled {
            summary.end = CaptureEnd::Stopped;
        }
    }
    summary.elapsed = start.elapsed();
    Ok(summary)
}

#[derive(Default)]
struct LineDecoder {
    bytes: Vec<u8>,
    skip_lf: bool,
}
impl LineDecoder {
    fn feed(
        &mut self,
        data: &[u8],
        summary: &mut CaptureSummary,
        callback: &mut dyn FnMut(CaptureEvent) -> bool,
    ) -> Option<CaptureEnd> {
        for &byte in data {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            if matches!(byte, b'\r' | b'\n') {
                self.skip_lf = byte == b'\r';
                let keep_reading = emit_line(&mut self.bytes, summary, callback);
                if summary.limit.is_some() {
                    return Some(CaptureEnd::LimitReached);
                }
                if !keep_reading {
                    return Some(CaptureEnd::Stopped);
                }
            } else {
                if self.bytes.len() == MAX_LINE_BYTES {
                    summary.limit = Some(CaptureLimit::LineBytes);
                    return Some(CaptureEnd::LimitReached);
                }
                self.bytes.push(byte);
            }
        }
        None
    }
}

fn emit_line(
    bytes: &mut Vec<u8>,
    summary: &mut CaptureSummary,
    callback: &mut dyn FnMut(CaptureEvent) -> bool,
) -> bool {
    let line = String::from_utf8_lossy(bytes).into_owned();
    bytes.clear();
    let decoded_bytes = summary.decoded_bytes + line.len() + 1;
    if decoded_bytes > MAX_CAPTURE_DECODED_BYTES {
        summary.limit = Some(CaptureLimit::DecodedBytes);
        return false;
    }
    summary.decoded_bytes = decoded_bytes;
    summary.lines += 1;
    if summary.lines == MAX_CAPTURE_LINES {
        summary.limit = Some(CaptureLimit::LineCount);
    }
    callback(CaptureEvent::Line(line))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, collections::VecDeque, rc::Rc};

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum ControlEvent {
        Dtr(bool),
        Rts(bool),
    }
    #[derive(Default)]
    struct FakeLines {
        events: Vec<ControlEvent>,
        en_asserted: Rc<Cell<bool>>,
        fail_on: Vec<usize>,
    }
    impl FakeLines {
        fn control(&mut self, event: ControlEvent) -> Result<()> {
            if let ControlEvent::Rts(asserted) = event {
                self.en_asserted.set(asserted);
            }
            self.events.push(event);
            // A driver may report failure after applying the level. Exercise
            // cleanup against that case, rather than assuming errors do nothing.
            ensure!(
                !self.fail_on.contains(&self.events.len()),
                "synthetic modem control failure"
            );
            Ok(())
        }
    }
    impl ResetLines for FakeLines {
        fn dtr(&mut self, asserted: bool) -> Result<()> {
            self.control(ControlEvent::Dtr(asserted))
        }
        fn rts(&mut self, asserted: bool) -> Result<()> {
            self.control(ControlEvent::Rts(asserted))
        }
    }

    #[test]
    fn startup_bridge_releases_gpio9_before_en_pulse_and_native_keeps_its_dedicated_order() {
        for native in [false, true] {
            let mut lines = FakeLines::default();
            let mut waited = Duration::ZERO;
            assert_eq!(
                startup_reset_lines(&mut lines, native, &AtomicBool::new(false), |delay| {
                    waited += delay
                })
                .unwrap(),
                ResetOutcome::Complete
            );
            let expected = if native {
                vec![
                    ControlEvent::Dtr(false),
                    ControlEvent::Rts(true),
                    ControlEvent::Dtr(false),
                    ControlEvent::Rts(true),
                    ControlEvent::Rts(false),
                ]
            } else {
                vec![
                    ControlEvent::Dtr(false),
                    ControlEvent::Rts(true),
                    ControlEvent::Rts(false),
                ]
            };
            assert_eq!(lines.events, expected);
            assert_eq!(
                waited,
                Duration::from_millis(if native { 300 } else { 200 })
            );
            assert!(!lines.en_asserted.get());
        }
    }

    #[test]
    fn startup_cancel_before_pulse_does_nothing_and_cancelling_during_hold_releases_en() {
        let mut untouched = FakeLines::default();
        assert_eq!(
            startup_reset_lines(&mut untouched, false, &AtomicBool::new(true), |_| panic!(
                "cancelled operation must not wait"
            ))
            .unwrap(),
            ResetOutcome::Cancelled
        );
        assert!(untouched.events.is_empty());
        for native in [false, true] {
            let mut lines = FakeLines::default();
            let asserted = lines.en_asserted.clone();
            let cancel = AtomicBool::new(false);
            let result = startup_reset_lines(&mut lines, native, &cancel, |_| {
                if asserted.get() {
                    cancel.store(true, Ordering::Release);
                }
            })
            .unwrap();
            assert_eq!(result, ResetOutcome::Cancelled);
            assert_eq!(lines.events.last(), Some(&ControlEvent::Rts(false)));
            assert!(!lines.en_asserted.get());
        }
    }

    #[test]
    fn modem_failures_and_unwind_attempt_en_release_and_failed_release_is_not_success() {
        for (native, fail_on) in [
            (false, vec![1]),
            (false, vec![2]),
            (true, vec![3]),
            (false, vec![3]),
            (false, vec![2, 3]),
        ] {
            let mut lines = FakeLines {
                fail_on,
                ..FakeLines::default()
            };
            let error = startup_reset_lines(&mut lines, native, &AtomicBool::new(false), |_| {})
                .unwrap_err();
            assert!(format!("{error:#}").contains("synthetic modem control failure"));
            assert_eq!(lines.events.last(), Some(&ControlEvent::Rts(false)));
            assert!(!lines.en_asserted.get());
        }
        let mut lines = FakeLines::default();
        let asserted = lines.en_asserted.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = startup_reset_lines(&mut lines, false, &AtomicBool::new(false), |_| {
                assert!(!asserted.get(), "synthetic panic during EN hold");
            });
        }));
        assert!(result.is_err());
        assert_eq!(lines.events.last(), Some(&ControlEvent::Rts(false)));
        assert!(!lines.en_asserted.get());
    }

    struct ScriptedRead {
        chunks: VecDeque<io::Result<Vec<u8>>>,
        timeouts: Vec<Duration>,
    }
    impl ScriptedRead {
        fn data(chunks: &[&[u8]]) -> Self {
            Self {
                chunks: chunks.iter().map(|chunk| Ok(chunk.to_vec())).collect(),
                timeouts: Vec::new(),
            }
        }
    }
    impl Read for ScriptedRead {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            let bytes = self
                .chunks
                .pop_front()
                .unwrap_or_else(|| Err(io::ErrorKind::TimedOut.into()))?;
            let length = bytes.len().min(output.len());
            output[..length].copy_from_slice(&bytes[..length]);
            if length < bytes.len() {
                self.chunks.push_front(Ok(bytes[length..].to_vec()));
            }
            Ok(length)
        }
    }
    impl CaptureRead for ScriptedRead {
        fn read_timeout(&mut self, timeout: Duration) -> Result<()> {
            self.timeouts.push(timeout);
            Ok(())
        }
    }

    struct CancelAfterData<'a> {
        inner: ScriptedRead,
        cancel: &'a AtomicBool,
    }
    impl Read for CancelAfterData<'_> {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            let result = self.inner.read(output);
            if self.inner.chunks.is_empty() {
                self.cancel.store(true, Ordering::Release);
            }
            result
        }
    }
    impl CaptureRead for CancelAfterData<'_> {
        fn read_timeout(&mut self, timeout: Duration) -> Result<()> {
            self.inner.read_timeout(timeout)
        }
    }

    fn options() -> CaptureOptions {
        CaptureOptions {
            timeout: Duration::from_secs(2),
            ..CaptureOptions::default()
        }
    }
    fn port(name: &str, identity: &str) -> Port {
        Port {
            name: name.into(),
            identity: identity.into(),
            description: "test".into(),
            usb: true,
            suggested: false,
        }
    }

    #[test]
    fn fragmented_utf8_crlf_and_early_field_completion_preserve_exact_lines() {
        let mut reader = ScriptedRead::data(&[
            b"hello \xe4",
            b"\xbd\xa0\r",
            b"\nUNIQUE DEVICE ",
            b"ID: ABC123\nignored\n",
        ]);
        let mut seen = Vec::new();
        let summary = capture_reader(
            &mut reader,
            &options(),
            &AtomicBool::new(false),
            &mut |event| {
                if let CaptureEvent::Line(line) = event {
                    seen.push(line);
                }
                seen.len() < 2
            },
        )
        .unwrap();
        assert_eq!(seen, ["hello 你", "UNIQUE DEVICE ID: ABC123"]);
        assert_eq!(summary.end, CaptureEnd::Stopped);
        assert_eq!(summary.lines, 2);
        assert_eq!(summary.limit, None);
        assert!(
            reader
                .timeouts
                .iter()
                .all(|timeout| *timeout >= Duration::from_millis(1) && *timeout <= READ_SLICE)
        );
    }

    #[test]
    fn cr_lf_blank_lines_and_invalid_utf8_are_decoded_once_per_line() {
        let mut decoder = LineDecoder::default();
        let mut summary = empty_summary(CaptureEnd::TimedOut);
        let mut seen = Vec::new();
        let mut callback = |event| {
            if let CaptureEvent::Line(line) = event {
                seen.push(line);
            }
            true
        };
        assert_eq!(decoder.feed(b"one\r", &mut summary, &mut callback), None);
        assert_eq!(
            decoder.feed(b"\n\r\ntwo\n\xff\n", &mut summary, &mut callback),
            None
        );
        assert_eq!(seen, ["one", "", "two", "�"]);
    }

    #[test]
    fn timeout_flushes_a_final_unterminated_line_and_read_error_preserves_its_fragment() {
        let mut reader = ScriptedRead::data(&[b"ID: tail \xe4", b"\xbd\xa0"]);
        let options = CaptureOptions {
            timeout: Duration::from_millis(100),
            ..options()
        };
        let mut seen = Vec::new();
        let summary = capture_reader(
            &mut reader,
            &options,
            &AtomicBool::new(false),
            &mut |event| {
                if let CaptureEvent::Line(line) = event {
                    seen.push(line);
                }
                true
            },
        )
        .unwrap();
        assert_eq!(summary.end, CaptureEnd::TimedOut);
        assert_eq!(seen, ["ID: tail 你"]);
        let mut broken = ScriptedRead::data(&[b"ID: disconnected-tail"]);
        broken
            .chunks
            .push_back(Err(io::ErrorKind::BrokenPipe.into()));
        let error = capture_reader(
            &mut broken,
            &options,
            &AtomicBool::new(false),
            &mut |event| {
                if let CaptureEvent::Line(line) = event {
                    seen.push(line);
                }
                true
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("读取串口失败"));
        assert_eq!(seen.last().unwrap(), "ID: disconnected-tail");
    }

    #[test]
    fn explicit_limits_bound_total_bytes_lines_and_unterminated_line_memory() {
        let cases = [
            (vec![b'x'; MAX_LINE_BYTES + 1], CaptureLimit::LineBytes),
            (vec![b'\n'; MAX_CAPTURE_LINES + 1], CaptureLimit::LineCount),
            (
                [vec![b'x'; MAX_LINE_BYTES], vec![b'\n']]
                    .concat()
                    .repeat(33),
                CaptureLimit::TotalBytes,
            ),
            (
                [vec![0xff; MAX_LINE_BYTES], vec![b'\n']]
                    .concat()
                    .repeat(12),
                CaptureLimit::DecodedBytes,
            ),
        ];
        for (data, expected) in cases {
            let mut reader = ScriptedRead::data(&[&data]);
            let mut emitted = 0;
            let mut emitted_bytes = 0;
            let summary = capture_reader(
                &mut reader,
                &options(),
                &AtomicBool::new(false),
                &mut |event| {
                    emitted += 1;
                    if let CaptureEvent::Line(line) = event {
                        emitted_bytes += line.len() + 1;
                    }
                    true
                },
            )
            .unwrap();
            assert_eq!(summary.end, CaptureEnd::LimitReached);
            assert_eq!(summary.limit, Some(expected));
            assert!(summary.bytes_read <= MAX_CAPTURE_BYTES && summary.lines <= MAX_CAPTURE_LINES);
            assert!(summary.decoded_bytes <= MAX_CAPTURE_DECODED_BYTES);
            assert_eq!(summary.decoded_bytes, emitted_bytes);
            assert_eq!(emitted, summary.lines);
            if expected == CaptureLimit::LineBytes {
                assert_eq!(emitted, 0, "never emit a truncated identifier line");
            }
        }
    }

    #[test]
    fn live_cancel_flushes_fragmented_utf8_once_and_final_fragments_obey_both_output_limits() {
        let cancel = AtomicBool::new(false);
        let mut reader = CancelAfterData {
            inner: ScriptedRead::data(&[b"tail \xe4", b"\xbd\xa0"]),
            cancel: &cancel,
        };
        let mut seen = Vec::new();
        let result = capture_reader(&mut reader, &options(), &cancel, &mut |event| {
            if let CaptureEvent::Line(line) = event {
                seen.push(line);
            }
            true
        })
        .unwrap();
        assert_eq!(result.end, CaptureEnd::Cancelled);
        assert_eq!(seen, ["tail 你"]);

        let line_limit = [vec![b'\n'; MAX_CAPTURE_LINES - 1], b"tail".to_vec()].concat();
        let decoded_limit = [
            [vec![0xff; MAX_LINE_BYTES], vec![b'\n']]
                .concat()
                .repeat(10),
            vec![0xff; 10_900],
        ]
        .concat();
        for (data, limit) in [
            (line_limit, CaptureLimit::LineCount),
            (decoded_limit, CaptureLimit::DecodedBytes),
        ] {
            let cancel = AtomicBool::new(false);
            let mut reader = CancelAfterData {
                inner: ScriptedRead::data(&[&data]),
                cancel: &cancel,
            };
            let summary = capture_reader(&mut reader, &options(), &cancel, &mut |_| true).unwrap();
            assert_eq!(summary.end, CaptureEnd::LimitReached);
            assert_eq!(summary.limit, Some(limit));
            assert!(
                summary.lines <= MAX_CAPTURE_LINES
                    && summary.decoded_bytes <= MAX_CAPTURE_DECODED_BYTES
            );
        }
        let mut summary = empty_summary(CaptureEnd::TimedOut);
        summary.lines = MAX_CAPTURE_LINES - 1;
        assert_eq!(
            LineDecoder::default().feed(b"last\n", &mut summary, &mut |_| false),
            Some(CaptureEnd::LimitReached)
        );
    }

    #[test]
    fn invalid_options_and_pre_cancelled_capture_never_open_a_device() {
        assert_eq!(CaptureOptions::default().baud, 115_200);
        assert!(!CaptureOptions::default().reset);
        assert!(
            validate_options(&CaptureOptions {
                baud: 0,
                ..options()
            })
            .is_err()
        );
        assert!(
            validate_options(&CaptureOptions {
                timeout: Duration::ZERO,
                ..options()
            })
            .is_err()
        );
        let summary = capture(
            &port("NOT_A_REAL_PORT", "frozen"),
            &options(),
            &AtomicBool::new(true),
            &mut |_| panic!("cancelled capture must not open or report a device"),
        )
        .unwrap();
        assert_eq!(summary.end, CaptureEnd::Cancelled);
        assert_eq!(summary.bytes_read, 0);
    }

    #[test]
    fn same_com_with_new_identity_is_rejected_and_identical_adapters_remain_distinct() {
        let first = port("COM3", "physical-A");
        let second = port("COM4", "physical-B");
        let current = [first.clone(), second.clone()];
        assert_eq!(
            expected_port(&first, &current).unwrap().identity,
            "physical-A"
        );
        assert_eq!(
            expected_port(&second, &current).unwrap().identity,
            "physical-B"
        );
        assert!(expected_port(&first, &[port("COM3", "replacement")]).is_err());
        assert!(expected_port(&first, &[]).is_err());
    }

    #[test]
    fn only_authentic_espressif_native_usb_metadata_selects_jtag_reset() {
        for identity in [
            r#"{"kind":"usb","vid":12346,"pid":4097}"#,
            r#"{"kind":"windows_pnp","instance_id":"USB\\VID_303A&PID_1001&MI_00\\location"}"#,
        ] {
            assert_eq!(reset_usb_info(&port("COM3", identity)).pid, 0x1001);
        }
        for identity in [
            r#"{"kind":"usb","vid":6790,"pid":4097}"#,
            r#"{"kind":"windows_pnp","instance_id":"USB\\VID_1A86&PID_55D4\\location"}"#,
            "not JSON",
        ] {
            assert_eq!(reset_usb_info(&port("COM3", identity)).pid, 0);
        }
    }
}
