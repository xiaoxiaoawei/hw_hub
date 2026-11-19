//! Windows Plug and Play notifications, without a polling timer or hidden window.
//!
//! Registration, teardown and user wake callbacks all run outside the UI and
//! Config Manager callbacks. Successful registration requests one fresh scan
//! to close the startup race; subsequent scans remain event driven.
use anyhow::Result;

#[cfg(any(windows, test))]
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender, SyncSender},
    },
    thread,
};

pub struct DeviceWatcher {
    #[cfg(windows)]
    registration: RegistrationWorker,
    #[cfg(windows)]
    state: Arc<EventSignal>,
}

impl DeviceWatcher {
    pub fn supported() -> bool {
        cfg!(windows)
    }

    /// `wake` runs on a relay thread, never a Config Manager callback. Creating
    /// this object only starts workers. Native registration and unregistration
    /// can wait inside Windows, so neither is allowed on the UI thread.
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Result<Self> {
        #[cfg(windows)]
        {
            let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(wake);
            let relay_wake = wake.clone();
            let state = EventSignal::start(move || relay_wake())?;
            let registration =
                match RegistrationWorker::start(state.clone(), wake, native::Registration::new) {
                    Ok(registration) => registration,
                    Err(error) => {
                        state.close();
                        return Err(error);
                    }
                };
            Ok(Self {
                registration,
                state,
            })
        }
        #[cfg(not(windows))]
        {
            let _ = wake;
            anyhow::bail!("当前平台不支持 Windows USB 插拔通知，请使用手动刷新串口")
        }
    }

    /// One asynchronous registration result. On failure the UI should expose
    /// the error and offer manual refresh; construction cannot hide an OS wait.
    pub fn poll_registration(&mut self) -> Option<Result<()>> {
        #[cfg(windows)]
        {
            self.registration.poll()
        }
        #[cfg(not(windows))]
        {
            None
        }
    }

    /// True once for one or more unconsumed arrival/removal events. If a scan
    /// is active, the UI must retain this result as a pending rescan instead of
    /// discarding it because request_scan() returns false. No scan is performed
    /// here, and consuming a flag cannot clear a subsequent concurrent event.
    pub fn take_pending(&self) -> bool {
        #[cfg(windows)]
        {
            self.state.take_pending()
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    pub fn poll_changed(&self) -> bool {
        self.take_pending()
    }
}

#[cfg(windows)]
impl Drop for DeviceWatcher {
    fn drop(&mut self) {
        self.state.close();
        // RegistrationWorker only sends a stop token. Its owner unregisters
        // native handles and releases callback state after callbacks quiesce.
    }
}

#[cfg(any(windows, test))]
struct RegistrationWorker {
    stop: Sender<()>,
    result: Receiver<Result<()>>,
}

#[cfg(any(windows, test))]
impl RegistrationWorker {
    fn start<T: 'static>(
        state: Arc<EventSignal>,
        wake: Arc<dyn Fn() + Send + Sync>,
        register: impl FnOnce(Arc<EventSignal>) -> Result<T> + Send + 'static,
    ) -> Result<Self> {
        use anyhow::Context;
        let (stop, stop_receiver) = mpsc::channel();
        let (sender, result) = mpsc::channel();
        thread::Builder::new()
            .name("flashtool-device-owner".into())
            .spawn(move || {
                let registered = catch_unwind(AssertUnwindSafe(|| register(state.clone())))
                    .unwrap_or_else(|_| {
                        Err(anyhow::anyhow!(
                            "Windows 插拔通知注册发生异常，请使用手动刷新"
                        ))
                    });
                let registration = match registered {
                    Ok(registration) => {
                        if sender.send(Ok(())).is_err() {
                            return;
                        }
                        // A scan may have finished before registration became
                        // active. This single event reconciles that startup gap.
                        state.notify();
                        Some(registration)
                    }
                    Err(error) => {
                        let _ = sender.send(Err(error));
                        None
                    }
                };
                // A partial native registration may already have closed the
                // relay. Wake directly from this owner, never the CM callback.
                let _ = catch_unwind(AssertUnwindSafe(|| wake()));
                let _ = stop_receiver.recv();
                state.close();
                drop(registration);
            })
            .context("无法启动 Windows 插拔通知管理线程")?;
        Ok(Self { stop, result })
    }

    fn poll(&mut self) -> Option<Result<()>> {
        self.result.try_recv().ok()
    }
}

#[cfg(any(windows, test))]
impl Drop for RegistrationWorker {
    fn drop(&mut self) {
        let _ = self.stop.send(());
    }
}

#[cfg(any(windows, test))]
struct EventSignal {
    pending: AtomicBool,
    closed: Arc<AtomicBool>,
    relay: SyncSender<()>,
}

#[cfg(any(windows, test))]
impl EventSignal {
    fn start(wake: impl Fn() + Send + Sync + 'static) -> Result<Arc<Self>> {
        use anyhow::Context;
        let (sender, receiver) = mpsc::sync_channel(1);
        let closed = Arc::new(AtomicBool::new(false));
        let relay_closed = closed.clone();
        // The relay deliberately does not own EventSignal (and its sender),
        // avoiding an Arc/channel cycle that would keep this thread alive.
        thread::Builder::new()
            .name("flashtool-device-events".into())
            .spawn(move || {
                while receiver.recv().is_ok() {
                    if relay_closed.load(Ordering::Acquire) {
                        break;
                    }
                    // A caller's wake panic must neither cross the CM FFI boundary
                    // nor poison future notifications on the relay.
                    let _ = catch_unwind(AssertUnwindSafe(&wake));
                }
            })
            .context("无法启动 USB 插拔事件唤醒线程")?;
        Ok(Arc::new(Self {
            pending: AtomicBool::new(false),
            closed,
            relay: sender,
        }))
    }

    /// Callback-safe: no enumeration, GUI calls, waiting send, or user closure.
    fn notify(&self) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        if !self.pending.swap(true, Ordering::AcqRel) {
            let _ = self.relay.try_send(());
        }
    }

    fn take_pending(&self) -> bool {
        self.pending.swap(false, Ordering::AcqRel)
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        // A full channel already has a token that wakes recv; never wait for
        // the relay or egui from UI Drop / Config Manager unregistration.
        let _ = self.relay.try_send(());
    }
}

#[cfg(windows)]
mod native {
    use super::*;
    use std::{ffi::c_void, mem, ptr::null_mut};
    use windows_sys::{
        Win32::Devices::DeviceAndDriverInstallation::{
            CM_NOTIFY_ACTION, CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL,
            CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL, CM_NOTIFY_EVENT_DATA, CM_NOTIFY_FILTER,
            CM_NOTIFY_FILTER_0_2, CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE, CM_Register_Notification,
            CM_Unregister_Notification, CR_SUCCESS, HCMNOTIFICATION,
        },
        core::GUID,
    };

    // Official Windows SDK GUID_DEVINTERFACE_COMPORT / USB_DEVICE values.
    // Defining their values avoids additional Ioctl/Usb binding features.
    const COMPORT: GUID = GUID::from_u128(0x86e0d1e0_8089_11d0_9ce4_08003e301f73);
    const USB_DEVICE: GUID = GUID::from_u128(0xa5dcbf10_6530_11d2_901f_00c04fb951ed);

    pub(super) struct Registration {
        handles: Vec<HCMNOTIFICATION>,
        // Stable Arc allocation exists before CM_Register_Notification, which
        // may synchronously invoke our callback before returning its handle.
        state: Option<Arc<EventSignal>>,
    }

    impl Registration {
        pub(super) fn new(state: Arc<EventSignal>) -> Result<Self> {
            let mut registration = Self {
                handles: Vec::with_capacity(2),
                state: Some(state),
            };
            for class in [COMPORT, USB_DEVICE] {
                // All reserved/unused union bytes must be zero as well.
                let mut filter: CM_NOTIFY_FILTER = unsafe { mem::zeroed() };
                filter.cbSize = mem::size_of::<CM_NOTIFY_FILTER>() as u32;
                filter.FilterType = CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE;
                filter.u.DeviceInterface = CM_NOTIFY_FILTER_0_2 { ClassGuid: class };
                let context = Arc::as_ptr(registration.state.as_ref().unwrap()).cast::<c_void>();
                let mut handle = null_mut();
                let result = unsafe {
                    CM_Register_Notification(&filter, context, Some(callback), &mut handle)
                };
                if result != CR_SUCCESS {
                    // Registration's Drop rolls back all earlier registrations
                    // and stops the relay before its callback state is freed.
                    anyhow::bail!(
                        "无法注册 Windows USB/COM 插拔通知（CM 错误 0x{result:X}），请使用手动刷新"
                    );
                }
                if handle.is_null() {
                    // A successful native registration must return its handle.
                    // If the OS violates that contract, retain the context
                    // rather than free memory a callback could still access.
                    registration.retain_context();
                    anyhow::bail!("Windows 未返回有效的 USB 通知句柄，请使用手动刷新");
                }
                registration.handles.push(handle);
            }
            Ok(registration)
        }

        fn retain_context(&mut self) {
            if let Some(state) = self.state.take() {
                state.close();
                let _ = Arc::into_raw(state);
            }
        }
    }

    impl Drop for Registration {
        fn drop(&mut self) {
            if let Some(state) = &self.state {
                state.close();
            }
            let mut failed = false;
            for handle in self.handles.drain(..).rev() {
                // Never run this from callback(). The API synchronously waits
                // for pending callbacks, which only perform atomic/try_send.
                let result = unsafe { CM_Unregister_Notification(handle) };
                if result != CR_SUCCESS {
                    failed = true;
                    eprintln!("USB 事件注销失败（CM 错误 0x{result:X}）");
                }
            }
            if failed {
                // A failed OS unregister cannot establish callback quiescence.
                // Only this exceptional path retains one context allocation;
                // its closed relay is still stopped. Freeing it would be UAF.
                // Normal shutdown and successful partial-registration rollback
                // release every CM handle and all callback state.
                self.retain_context();
            }
        }
    }

    unsafe extern "system" fn callback(
        _handle: HCMNOTIFICATION,
        context: *const c_void,
        action: CM_NOTIFY_ACTION,
        _event: *const CM_NOTIFY_EVENT_DATA,
        _event_size: u32,
    ) -> u32 {
        let _ = catch_unwind(AssertUnwindSafe(|| {
            if !context.is_null()
                && matches!(
                    action,
                    CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL
                        | CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL
                )
            {
                // Registration retains this Arc allocation until all native
                // handles have been synchronously unregistered. EventData is
                // callback-scoped and is neither dereferenced nor retained.
                let signal = unsafe { &*context.cast::<EventSignal>() };
                signal.notify();
            }
        }));
        0 // ERROR_SUCCESS; no Rust panic may cross the system ABI boundary.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::atomic::AtomicUsize, time::Duration};
    const WAIT: Duration = Duration::from_secs(3);

    #[test]
    fn slow_native_registration_and_teardown_never_block_the_owner_or_dropper() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (created_tx, created_rx) = mpsc::channel();
        let (dropped_tx, dropped_rx) = mpsc::channel();
        let (native_drop_tx, native_drop_rx) = mpsc::channel();
        struct NativeRegistration(mpsc::Sender<thread::ThreadId>);
        impl Drop for NativeRegistration {
            fn drop(&mut self) {
                let _ = self.0.send(thread::current().id());
            }
        }
        thread::spawn(move || {
            let state = EventSignal::start(|| {}).unwrap();
            let worker = RegistrationWorker::start(state.clone(), Arc::new(|| {}), move |_| {
                let id = thread::current().id();
                entered_tx.send(id)?;
                release_rx.recv()?;
                Ok(NativeRegistration(native_drop_tx))
            })
            .unwrap();
            created_tx.send((worker, state)).unwrap();
        });
        let (worker, state) = created_rx
            .recv_timeout(WAIT)
            .expect("constructor must not wait for native registration");
        let native_thread = entered_rx.recv_timeout(WAIT).unwrap();
        thread::spawn(move || {
            drop(worker);
            state.close();
            let _ = dropped_tx.send(());
        });
        let dropped = dropped_rx.recv_timeout(WAIT);
        // Release the OS stand-in even if a broken synchronous Drop would fail.
        release_tx.send(()).unwrap();
        dropped.expect("Drop must not wait for registration/unregistration");
        assert_eq!(native_drop_rx.recv_timeout(WAIT).unwrap(), native_thread);
    }

    #[test]
    fn registration_reports_ready_and_reconciles_the_startup_gap_or_reports_failure() {
        for fail in [false, true] {
            let (wake_tx, wake_rx) = mpsc::channel();
            let state = EventSignal::start(|| {}).unwrap();
            let mut owner = RegistrationWorker::start(
                state.clone(),
                Arc::new(move || {
                    let _ = wake_tx.send(());
                }),
                move |_| {
                    if fail {
                        anyhow::bail!("mock CM registration failure");
                    }
                    Ok(())
                },
            )
            .unwrap();
            wake_rx.recv_timeout(WAIT).unwrap();
            let result = owner.poll().expect("wake follows registration result");
            assert_eq!(result.is_err(), fail);
            if fail {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("mock CM registration failure")
                );
            } else {
                assert!(
                    state.take_pending(),
                    "ready must request a snapshot after async registration"
                );
            }
            drop(owner);
            state.close();
        }
    }

    #[test]
    fn a_device_burst_coalesces_until_the_consumer_takes_the_event() {
        let (tx, rx) = mpsc::channel();
        let signal = EventSignal::start(move || {
            let _ = tx.send(());
        })
        .unwrap();
        signal.notify();
        rx.recv_timeout(WAIT).unwrap();
        for _ in 0..100 {
            signal.notify();
        }
        assert!(signal.take_pending());
        assert!(!signal.take_pending());
        assert!(rx.try_recv().is_err());
        signal.notify();
        rx.recv_timeout(WAIT).unwrap();
        assert!(signal.take_pending());
        signal.close();
    }

    #[test]
    fn a_new_event_during_an_existing_scan_remains_dirty_for_a_followup_scan() {
        let signal = EventSignal::start(|| {}).unwrap();
        signal.notify();
        assert!(signal.take_pending()); // The initial event starts scan A.
        signal.notify(); // Physical change while scan A is still in flight.
        signal.notify(); // COM arrival can follow its USB interface arrival.
        // Completing scan A does not reset EventSignal: scan B is still due.
        assert!(signal.take_pending());
        assert!(!signal.take_pending());
        signal.close();
    }

    #[test]
    fn a_second_device_event_does_not_wait_for_the_first_wake_and_full_relay_keeps_the_latest_change()
     {
        use std::sync::Barrier;
        let (tx, rx) = mpsc::channel();
        let gate = Arc::new(Barrier::new(2));
        let first_wake = gate.clone();
        let calls = AtomicUsize::new(0);
        let signal = EventSignal::start(move || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            let _ = tx.send(n);
            if n == 0 {
                first_wake.wait();
            }
        })
        .unwrap();
        signal.notify();
        assert_eq!(rx.recv_timeout(WAIT).unwrap(), 0);
        assert!(signal.take_pending());
        // A second insertion is observable immediately, even while the first
        // UI wake is busy; the native callback has no debounce/sleep/wait.
        signal.notify();
        assert!(signal.take_pending());
        // Its relay token is already queued. A third event encounters a full
        // one-slot channel but must still retain dirty state for that token.
        signal.notify();
        gate.wait();
        assert_eq!(rx.recv_timeout(WAIT).unwrap(), 1);
        assert!(signal.take_pending());
        assert!(!signal.take_pending());
        signal.close();
    }

    #[test]
    fn shutdown_stops_the_relay_and_does_not_retain_a_sender_cycle() {
        let (done_tx, done_rx) = mpsc::channel();
        struct WakeDrop(mpsc::Sender<()>);
        impl Drop for WakeDrop {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }
        let done = WakeDrop(done_tx);
        let signal = EventSignal::start(move || {
            let _ = &done;
        })
        .unwrap();
        let weak = Arc::downgrade(&signal);
        signal.close();
        signal.notify(); // Notifications racing with shutdown are ignored.
        assert!(!signal.take_pending());
        drop(signal);
        assert!(weak.upgrade().is_none());
        done_rx
            .recv_timeout(WAIT)
            .expect("relay must stop without joining from UI Drop");
    }

    #[test]
    fn user_wake_panics_are_contained_on_the_relay() {
        let count = AtomicUsize::new(0);
        let (tx, rx) = mpsc::channel();
        let signal = EventSignal::start(move || {
            let n = count.fetch_add(1, Ordering::SeqCst);
            let _ = tx.send(n);
            if n == 0 {
                panic!("mock repaint panic");
            }
        })
        .unwrap();
        signal.notify();
        assert_eq!(rx.recv_timeout(WAIT).unwrap(), 0);
        assert!(signal.take_pending());
        signal.notify();
        assert_eq!(rx.recv_timeout(WAIT).unwrap(), 1);
        signal.close();
    }

    #[cfg(not(windows))]
    #[test]
    fn unsupported_platform_does_not_pretend_to_watch_devices() {
        assert!(!DeviceWatcher::supported());
        assert!(DeviceWatcher::new(|| {}).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn native_notifications_register_and_unregister_without_device_access() {
        assert!(DeviceWatcher::supported());
        let mut watcher =
            DeviceWatcher::new(|| {}).expect("Windows Config Manager notification registration");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(result) = watcher.poll_registration() {
                result.expect("native notification registration must succeed");
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "native registration timed out"
            );
            thread::sleep(Duration::from_millis(10));
        }
        drop(watcher); // No enumeration, serial open, driver install, or physical USB test.
    }
}
