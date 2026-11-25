use anyhow::{Result, bail};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_core_foundation::CGSize;
use objc2_foundation::{NSError, NSObject, NSObjectProtocol, NSTimer};
use objc2_virtualization::{
    VZUSBMassStorageDevice, VZVirtioTraditionalMemoryBalloonDevice, VZVirtualMachine,
    VZVirtualMachineConfiguration, VZVirtualMachineDelegate,
};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::rc::Rc;

use crate::config::{VmConfig, VmStore};
use crate::vm::config_builder::{build_usb_iso_config, build_vz_configuration, file_url};
use crate::vm::console::SerialBridge;
use crate::vm::power::SleepInhibitor;
use crate::vm::{VmEvent, VmState, is_virtualization_supported};
use std::fs;

type EventCallback = Rc<RefCell<Box<dyn FnMut(VmEvent)>>>;

pub struct VmDelegateIvars {
    on_event: EventCallback,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[ivars = VmDelegateIvars]
    pub struct RustVmDelegate;

    unsafe impl NSObjectProtocol for RustVmDelegate {}

    unsafe impl VZVirtualMachineDelegate for RustVmDelegate {
        #[unsafe(method(guestDidStopVirtualMachine:))]
        fn guest_did_stop(&self, _vm: &VZVirtualMachine) {
            (self.ivars().on_event.borrow_mut())(VmEvent::GuestStopped);
        }

        #[unsafe(method(virtualMachine:didStopWithError:))]
        fn did_stop_with_error(&self, _vm: &VZVirtualMachine, error: &NSError) {
            let msg = error.localizedDescription().to_string();
            (self.ivars().on_event.borrow_mut())(VmEvent::StoppedWithError(msg));
        }
    }
);

impl RustVmDelegate {
    fn new(on_event: EventCallback) -> Retained<Self> {
        let this = Self::alloc().set_ivars(VmDelegateIvars { on_event });
        unsafe { msg_send![super(this), init] }
    }
}

pub struct VmRunner {
    vm: Retained<VZVirtualMachine>,
    _delegate: Retained<RustVmDelegate>,
    serial: SerialBridge,
    on_event: EventCallback,
    configured_memory_bytes: u64,
    save_restore_support: Result<(), String>,
    balloon_restore_timer: RefCell<Option<Retained<NSTimer>>>,
    sleep_inhibitor: Rc<RefCell<SleepInhibitor>>,
    prevent_sleep: Rc<Cell<bool>>,
    sleep_reason: String,
}

impl VmRunner {
    /// Creates a new `VmRunner` on the main dispatch queue.
    pub fn new<F>(
        store: &VmStore,
        cfg: &VmConfig,
        serial: SerialBridge,
        headless: bool,
        on_event: F,
    ) -> Result<Self>
    where
        F: FnMut(VmEvent) + 'static,
    {
        if !is_virtualization_supported() {
            bail!("Apple Virtualization.framework is not supported on this Mac.");
        }

        let vz_config = build_vz_configuration(store, cfg, &serial, headless)?;
        let sleep_inhibitor = Rc::new(RefCell::new(SleepInhibitor::new()));
        let prevent_sleep = Rc::new(Cell::new(cfg.prevent_sleep));
        let sleep_reason = format!("RustVM: {} is running", cfg.name);

        let inh_ev = Rc::clone(&sleep_inhibitor);
        let prev_ev = Rc::clone(&prevent_sleep);
        let reason_ev = sleep_reason.clone();
        let mut user_cb = on_event;
        let wrapped_cb: EventCallback = Rc::new(RefCell::new(Box::new(move |event: VmEvent| {
            match &event {
                VmEvent::StateChanged(VmState::Running) => {
                    if prev_ev.get() {
                        inh_ev.borrow_mut().acquire(&reason_ev);
                    }
                }
                VmEvent::StateChanged(
                    VmState::Paused
                    | VmState::Stopped
                    | VmState::Error
                    | VmState::Saving
                    | VmState::Stopping,
                )
                | VmEvent::GuestStopped
                | VmEvent::StoppedWithError(_) => {
                    inh_ev.borrow_mut().release();
                }
                _ => {}
            }
            (user_cb)(event);
        })));

        let delegate = RustVmDelegate::new(Rc::clone(&wrapped_cb));

        let configured_memory_bytes = unsafe { vz_config.memorySize() };
        let save_restore_support = unsafe {
            vz_config
                .validateSaveRestoreSupportWithError()
                .map_err(|e| e.localizedDescription().to_string())
        };
        let vm = unsafe {
            let vm = VZVirtualMachine::initWithConfiguration(
                VZVirtualMachine::alloc(),
                &vz_config,
            );
            let proto = ProtocolObject::from_ref(&*delegate);
            vm.setDelegate(Some(proto));
            vm
        };

        Ok(Self {
            vm,
            _delegate: delegate,
            serial,
            on_event: wrapped_cb,
            configured_memory_bytes,
            save_restore_support,
            balloon_restore_timer: RefCell::new(None),
            sleep_inhibitor,
            prevent_sleep,
            sleep_reason,
        })
    }

    pub fn vz_vm(&self) -> &VZVirtualMachine {
        &self.vm
    }

    pub fn state(&self) -> VmState {
        let raw = unsafe { self.vm.state() };
        VmState::from_vz(raw)
    }

    pub fn can_pause(&self) -> bool {
        unsafe { self.vm.canPause() }
    }

    pub fn can_resume(&self) -> bool {
        unsafe { self.vm.canResume() }
    }

    pub fn can_request_stop(&self) -> bool {
        unsafe { self.vm.canRequestStop() }
    }

    pub fn can_stop(&self) -> bool {
        unsafe { self.vm.canStop() }
    }

    pub fn can_save_state(&self) -> bool {
        self.save_restore_support.is_ok()
    }

    pub fn save_restore_unsupported_reason(&self) -> Option<&str> {
        self.save_restore_support.as_ref().err().map(String::as_str)
    }

    /// Starts the VM — or instantly restores it if a saved state snapshot exists and is compatible.
    pub fn start_or_restore(&self, saved_state_path: &Path) {
        if saved_state_path.is_file() && self.can_save_state() {
            if let Ok(url) = file_url(saved_state_path) {
                let cb = Rc::clone(&self.on_event);
                let vm_clone = self.vm.clone();
                let state_path = saved_state_path.to_path_buf();
                (cb.borrow_mut())(VmEvent::StateChanged(VmState::Restoring));

                let block = RcBlock::new(move |err: *mut NSError| {
                    let _ = fs::remove_file(&state_path);
                    if unsafe { err.as_ref() }.is_some() {
                        // Snapshot incompatible (e.g., macOS update or config edit) — fall back to clean boot
                        let cb2 = Rc::clone(&cb);
                        (cb2.borrow_mut())(VmEvent::StateChanged(VmState::Starting));
                        let start_blk = RcBlock::new(move |start_err: *mut NSError| {
                            if let Some(e) = unsafe { start_err.as_ref() } {
                                (cb2.borrow_mut())(VmEvent::ActionError(format!(
                                    "Failed to start VM: {}",
                                    e.localizedDescription()
                                )));
                                (cb2.borrow_mut())(VmEvent::StateChanged(VmState::Stopped));
                            } else {
                                (cb2.borrow_mut())(VmEvent::StateChanged(VmState::Running));
                            }
                        });
                        unsafe {
                            vm_clone.startWithCompletionHandler(&start_blk);
                        }
                    } else {
                        // Restore succeeded (VM is now Paused) -> immediately resume!
                        let cb2 = Rc::clone(&cb);
                        (cb2.borrow_mut())(VmEvent::StateChanged(VmState::Resuming));
                        let resume_blk = RcBlock::new(move |res_err: *mut NSError| {
                            if let Some(e) = unsafe { res_err.as_ref() } {
                                (cb2.borrow_mut())(VmEvent::ActionError(format!(
                                    "Failed to resume restored VM: {}",
                                    e.localizedDescription()
                                )));
                                (cb2.borrow_mut())(VmEvent::StateChanged(VmState::Paused));
                            } else {
                                (cb2.borrow_mut())(VmEvent::StateChanged(VmState::Running));
                            }
                        });
                        unsafe {
                            vm_clone.resumeWithCompletionHandler(&resume_blk);
                        }
                    }
                });

                unsafe {
                    self.vm.restoreMachineStateFromURL_completionHandler(&url, &block);
                }
                return;
            }
        }
        self.start();
    }

    /// Pauses (if running), saves complete VM memory & CPU state to disk, and powers off to free 100% of host RAM.
    pub fn save_state_and_stop<F>(&self, saved_state_path: &Path, on_done: F)
    where
        F: Fn(Result<(), String>) + 'static,
    {
        if let Err(ref reason) = self.save_restore_support {
            on_done(Err(format!(
                "Save State is not supported for this VM configuration: {reason}"
            )));
            return;
        }
        let tmp_path = saved_state_path.with_extension("vzsave.tmp");
        let _ = fs::remove_file(&tmp_path);
        let Ok(tmp_url) = file_url(&tmp_path) else {
            on_done(Err("Invalid save state file path".into()));
            return;
        };

        let final_path = saved_state_path.to_path_buf();
        let cb = Rc::clone(&self.on_event);
        let vm = self.vm.clone();
        let on_done = Rc::new(on_done);
        (cb.borrow_mut())(VmEvent::StateChanged(VmState::Saving));

        let run_save = Rc::new(move || {
            let cb_save = Rc::clone(&cb);
            let vm_save = vm.clone();
            let done_save = Rc::clone(&on_done);
            let tmp_p = tmp_path.clone();
            let fin_p = final_path.clone();
            let save_block = RcBlock::new(move |err: *mut NSError| {
                if let Some(e) = unsafe { err.as_ref() } {
                    let _ = fs::remove_file(&tmp_p);
                    let msg = format!("Failed to save VM state: {}", e.localizedDescription());
                    (cb_save.borrow_mut())(VmEvent::StateChanged(VmState::Paused));
                    (done_save)(Err(msg));
                } else {
                    if let Err(io_err) = fs::rename(&tmp_p, &fin_p) {
                        let _ = fs::remove_file(&tmp_p);
                        (cb_save.borrow_mut())(VmEvent::StateChanged(VmState::Paused));
                        (done_save)(Err(format!("Failed to finalize saved state: {io_err}")));
                        return;
                    }
                    let cb_stop = Rc::clone(&cb_save);
                    let done_stop = Rc::clone(&done_save);
                    let stop_block = RcBlock::new(move |_stop_err: *mut NSError| {
                        (cb_stop.borrow_mut())(VmEvent::GuestStopped);
                        (done_stop)(Ok(()));
                    });
                    unsafe {
                        vm_save.stopWithCompletionHandler(&stop_block);
                    }
                }
            });
            unsafe {
                vm.saveMachineStateToURL_completionHandler(&tmp_url, &save_block);
            }
        });

        if self.can_pause() {
            let save_after_pause = Rc::clone(&run_save);
            let cb_pause = Rc::clone(&self.on_event);
            let pause_block = RcBlock::new(move |err: *mut NSError| {
                if let Some(e) = unsafe { err.as_ref() } {
                    let msg = format!("Failed to pause VM before saving: {}", e.localizedDescription());
                    (cb_pause.borrow_mut())(VmEvent::ActionError(msg));
                    (cb_pause.borrow_mut())(VmEvent::StateChanged(VmState::Running));
                } else {
                    (save_after_pause)();
                }
            });
            unsafe {
                self.vm.pauseWithCompletionHandler(&pause_block);
            }
        } else if self.state() == VmState::Paused {
            (run_save)();
        }
    }

    pub fn start(&self) {
        let cb = Rc::clone(&self.on_event);
        (cb.borrow_mut())(VmEvent::StateChanged(VmState::Starting));
        let block = RcBlock::new(move |err: *mut NSError| {
            if let Some(err_ref) = unsafe { err.as_ref() } {
                let msg = err_ref.localizedDescription().to_string();
                let hint = if msg.contains("entitlement") || msg.contains("permission") || msg.contains("privileg") {
                    format!(
                        "{msg}\n\nTip: Sign the binary with `./setup.sh` or `codesign --entitlements entitlements.plist --force -s - <binary>`."
                    )
                } else {
                    msg
                };
                (cb.borrow_mut())(VmEvent::ActionError(format!("Failed to start VM: {hint}")));
                (cb.borrow_mut())(VmEvent::StateChanged(VmState::Stopped));
            } else {
                (cb.borrow_mut())(VmEvent::StateChanged(VmState::Running));
            }
        });
        unsafe {
            self.vm.startWithCompletionHandler(&block);
        }
    }

    pub fn pause(&self) {
        if !self.can_pause() {
            return;
        }
        let cb = Rc::clone(&self.on_event);
        (cb.borrow_mut())(VmEvent::StateChanged(VmState::Pausing));
        let block = RcBlock::new(move |err: *mut NSError| {
            if let Some(err_ref) = unsafe { err.as_ref() } {
                (cb.borrow_mut())(VmEvent::ActionError(format!(
                    "Failed to pause VM: {}",
                    err_ref.localizedDescription()
                )));
                (cb.borrow_mut())(VmEvent::StateChanged(VmState::Running));
            } else {
                (cb.borrow_mut())(VmEvent::StateChanged(VmState::Paused));
            }
        });
        unsafe {
            self.vm.pauseWithCompletionHandler(&block);
        }
    }

    pub fn resume(&self) {
        if !self.can_resume() {
            return;
        }
        let cb = Rc::clone(&self.on_event);
        (cb.borrow_mut())(VmEvent::StateChanged(VmState::Resuming));
        let block = RcBlock::new(move |err: *mut NSError| {
            if let Some(err_ref) = unsafe { err.as_ref() } {
                (cb.borrow_mut())(VmEvent::ActionError(format!(
                    "Failed to resume VM: {}",
                    err_ref.localizedDescription()
                )));
                (cb.borrow_mut())(VmEvent::StateChanged(VmState::Paused));
            } else {
                (cb.borrow_mut())(VmEvent::StateChanged(VmState::Running));
            }
        });
        unsafe {
            self.vm.resumeWithCompletionHandler(&block);
        }
    }

    /// Sends a graceful ACPI power-button shutdown signal to the Linux guest.
    pub fn request_stop(&self) {
        if !self.can_request_stop() {
            return;
        }
        let res = unsafe { self.vm.requestStopWithError() };
        if let Err(e) = res {
            (self.on_event.borrow_mut())(VmEvent::ActionError(format!(
                "Graceful shutdown failed: {}",
                e.localizedDescription()
            )));
        }
    }

    /// Immediately powers off the VM (equivalent to pulling the power cord).
    pub fn force_stop(&self) {
        if !self.can_stop() {
            (self.on_event.borrow_mut())(VmEvent::GuestStopped);
            return;
        }
        let cb = Rc::clone(&self.on_event);
        (cb.borrow_mut())(VmEvent::StateChanged(VmState::Stopping));
        let block = RcBlock::new(move |err: *mut NSError| {
            if let Some(err_ref) = unsafe { err.as_ref() } {
                (cb.borrow_mut())(VmEvent::ActionError(format!(
                    "Force stop error: {}",
                    err_ref.localizedDescription()
                )));
            }
            (cb.borrow_mut())(VmEvent::GuestStopped);
        });
        unsafe {
            self.vm.stopWithCompletionHandler(&block);
        }
    }

    /// Hot-ejects all currently attached USB Mass Storage ISO devices from the running VM.
    pub fn hot_eject_iso(&self) {
        unsafe {
            let controllers = self.vm.usbControllers();
            if controllers.is_empty() {
                (self.on_event.borrow_mut())(VmEvent::IsoChanged(None));
                return;
            }
            let ctrl = controllers.objectAtIndex(0);
            let devices = ctrl.usbDevices();
            if devices.is_empty() {
                (self.on_event.borrow_mut())(VmEvent::IsoChanged(None));
                return;
            }
            let total = devices.count();
            for i in 0..total {
                let dev = devices.objectAtIndex(i);
                let cb = Rc::clone(&self.on_event);
                let is_last = i + 1 == total;
                let block = RcBlock::new(move |err: *mut NSError| {
                    if let Some(err_ref) = err.as_ref() {
                        (cb.borrow_mut())(VmEvent::ActionError(format!(
                            "Failed to eject ISO: {}",
                            err_ref.localizedDescription()
                        )));
                    } else if is_last {
                        (cb.borrow_mut())(VmEvent::IsoChanged(None));
                    }
                });
                ctrl.detachDevice_completionHandler(&dev, &block);
            }
        }
    }

    /// Hot-attaches an ISO image as a USB Mass Storage device to the running VM
    /// (detaching any existing USB ISO first).
    pub fn hot_attach_iso(&self, iso_path: &Path) -> Result<()> {
        let usb_cfg = build_usb_iso_config(iso_path)?;
        let iso_owned: PathBuf = iso_path.to_path_buf();
        unsafe {
            let controllers = self.vm.usbControllers();
            if controllers.is_empty() {
                bail!("VM has no USB controller");
            }
            let ctrl = controllers.objectAtIndex(0);
            // Detach existing USB storage devices if any
            let existing = ctrl.usbDevices();
            for i in 0..existing.count() {
                let dev = existing.objectAtIndex(i);
                let noop = RcBlock::new(|_err: *mut NSError| {});
                ctrl.detachDevice_completionHandler(&dev, &noop);
            }
            let usb_dev = VZUSBMassStorageDevice::initWithConfiguration(
                VZUSBMassStorageDevice::alloc(),
                &usb_cfg,
            );
            let usb_proto = ProtocolObject::from_retained(usb_dev);
            let cb = Rc::clone(&self.on_event);
            let block = RcBlock::new(move |err: *mut NSError| {
                if let Some(err_ref) = err.as_ref() {
                    (cb.borrow_mut())(VmEvent::ActionError(format!(
                        "Failed to attach ISO: {}",
                        err_ref.localizedDescription()
                    )));
                } else {
                    (cb.borrow_mut())(VmEvent::IsoChanged(Some(iso_owned.clone())));
                }
            });
            ctrl.attachDevice_completionHandler(&usb_proto, &block);
        }
        Ok(())
    }

    /// Dynamically reconfigures the Virtio-GPU scanout resolution in pixels.
    /// Uses 2-pixel alignment (preserving exact fullscreen aspect ratio with zero black bars).
    pub fn reconfigure_display(&self, width: usize, height: usize) {
        let w = (width & !1).clamp(800, 4096) as f64;
        let h = (height & !1).clamp(600, 2560) as f64;
        unsafe {
            let gpus = self.vm.graphicsDevices();
            if gpus.is_empty() {
                return;
            }
            let displays = gpus.objectAtIndex(0).displays();
            if displays.is_empty() {
                return;
            }
            let display = displays.objectAtIndex(0);
            let current = display.sizeInPixels();
            if (current.width - w).abs() < 2.0 && (current.height - h).abs() < 2.0 {
                return;
            }
            let _ = display.reconfigureWithSizeInPixels_error(CGSize::new(w, h));
        }
    }

    /// Triggers a 3.5-second Virtio memory balloon pulse so Linux flushes idle page cache
    /// back to macOS, then restores the VM's full memory ceiling (freed pages stay unmapped
    /// on macOS until Linux actually needs them again).
    pub fn reclaim_idle_memory<F>(&self, on_done: F) -> bool
    where
        F: Fn() + 'static,
    {
        unsafe {
            let balloons = self.vm.memoryBalloonDevices();
            if balloons.is_empty() {
                return false;
            }
            let dev = balloons.objectAtIndex(0);
            let Ok(balloon) = dev.downcast::<VZVirtioTraditionalMemoryBalloonDevice>() else {
                return false;
            };

            let min_mem = VZVirtualMachineConfiguration::minimumAllowedMemorySize();
            let full_mem = self.configured_memory_bytes;
            // Temporarily target ~45% of configured RAM (aligned to 1 MiB) to flush cold cache
            let target = ((full_mem * 45 / 100) / (1024 * 1024)) * (1024 * 1024);
            let clamped = target.clamp(min_mem, full_mem);
            if clamped >= full_mem {
                return false;
            }

            if let Some(prev) = self.balloon_restore_timer.borrow_mut().take() {
                prev.invalidate();
            }

            balloon.setTargetVirtualMachineMemorySize(clamped);

            let balloon_restore = balloon.clone();
            let block = RcBlock::new(move |_timer: NonNull<NSTimer>| {
                balloon_restore.setTargetVirtualMachineMemorySize(full_mem);
                on_done();
            });
            let timer = NSTimer::scheduledTimerWithTimeInterval_repeats_block(3.5, false, &block);
            *self.balloon_restore_timer.borrow_mut() = Some(timer);
            true
        }
    }

    /// Live-toggles macOS display & system sleep prevention while the VM is running.
    pub fn set_prevent_sleep(&self, enabled: bool) {
        self.prevent_sleep.set(enabled);
        if enabled && self.state() == VmState::Running {
            self.sleep_inhibitor.borrow_mut().acquire(&self.sleep_reason);
        } else {
            self.sleep_inhibitor.borrow_mut().release();
        }
    }

    pub fn shutdown_serial(&mut self) {
        self.sleep_inhibitor.borrow_mut().release();
        if let Some(t) = self.balloon_restore_timer.borrow_mut().take() {
            t.invalidate();
        }
        self.serial.shutdown();
    }
}

impl Drop for VmRunner {
    fn drop(&mut self) {
        self.serial.shutdown();
    }
}
