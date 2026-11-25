use anyhow::{Result, bail};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_core_foundation::CGSize;
use objc2_foundation::{NSError, NSObject, NSObjectProtocol, NSTimer};
use objc2_virtualization::{
    VZNATNetworkDeviceAttachment, VZNetworkDevice, VZNetworkDeviceAttachment,
    VZUSBMassStorageDevice, VZVirtioTraditionalMemoryBalloonDevice, VZVirtualMachine,
    VZVirtualMachineConfiguration, VZVirtualMachineDelegate,
};
use std::cell::{Cell, RefCell};
use std::fs;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::rc::Rc;
use std::time::Instant;

use crate::config::{VmConfig, VmStore, format_bytes};
use crate::disk;
use crate::vm::config_builder::{build_usb_iso_config, build_vz_configuration, file_url};
use crate::vm::console::SerialBridge;
use crate::vm::lock::VmLock;
use crate::vm::power::SleepInhibitor;
use crate::vm::{VmEvent, VmState, is_virtualization_supported};

/// Immutable `Fn` callback so nested Objective-C delegate / modal NSAlert callbacks
/// can never trigger a `RefCell` `BorrowMutError` panic across FFI boundaries.
type EventCallback = Rc<dyn Fn(VmEvent)>;

pub struct VmDelegateIvars {
    on_event: EventCallback,
    net_recovery_window: Cell<(Instant, u32)>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[ivars = VmDelegateIvars]
    pub struct RustVmDelegate;

    unsafe impl NSObjectProtocol for RustVmDelegate {}

    unsafe impl VZVirtualMachineDelegate for RustVmDelegate {
        #[unsafe(method(guestDidStopVirtualMachine:))]
        fn guest_did_stop(&self, _vm: &VZVirtualMachine) {
            (self.ivars().on_event)(VmEvent::GuestStopped);
        }

        #[unsafe(method(virtualMachine:didStopWithError:))]
        fn did_stop_with_error(&self, _vm: &VZVirtualMachine, error: &NSError) {
            let msg = error.localizedDescription().to_string();
            (self.ivars().on_event)(VmEvent::StoppedWithError(msg));
        }

        #[unsafe(method(virtualMachine:networkDevice:attachmentWasDisconnectedWithError:))]
        fn network_was_disconnected(
            &self,
            _vm: &VZVirtualMachine,
            network_device: &VZNetworkDevice,
            error: &NSError,
        ) {
            let now = Instant::now();
            let (win_start, count) = self.ivars().net_recovery_window.get();
            let next_count = if now.duration_since(win_start).as_secs() > 15 {
                self.ivars().net_recovery_window.set((now, 1));
                1
            } else {
                let c = count + 1;
                self.ivars().net_recovery_window.set((win_start, c));
                c
            };

            if next_count <= 5 {
                unsafe {
                    let nat = VZNATNetworkDeviceAttachment::new();
                    let nat_super: Retained<VZNetworkDeviceAttachment> = Retained::into_super(nat);
                    network_device.setAttachment(Some(&nat_super));
                }
                eprintln!(
                    "[RustVM] Guest network disconnected ({}); automatically reattached macOS NAT.",
                    error.localizedDescription()
                );
            } else {
                (self.ivars().on_event)(VmEvent::ActionError(format!(
                    "Guest network disconnected repeatedly: {}",
                    error.localizedDescription()
                )));
            }
        }
    }
);

impl RustVmDelegate {
    fn new(on_event: EventCallback) -> Retained<Self> {
        let this = Self::alloc().set_ivars(VmDelegateIvars {
            on_event,
            net_recovery_window: Cell::new((Instant::now(), 0)),
        });
        unsafe { msg_send![super(this), init] }
    }
}

pub struct VmRunner {
    _vm_lock: VmLock,
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
    /// Creates a new `VmRunner` on the main dispatch queue after acquiring an exclusive `vm.lock`.
    pub fn new<F>(
        store: &VmStore,
        cfg: &VmConfig,
        serial: SerialBridge,
        headless: bool,
        on_event: F,
    ) -> Result<Self>
    where
        F: Fn(VmEvent) + 'static,
    {
        if !is_virtualization_supported() {
            bail!("Apple Virtualization.framework is not supported on this Mac.");
        }

        // Acquire exclusive kernel lock before touching NVRAM or virtual disk image
        let vm_lock = VmLock::acquire(&store.lock_path(&cfg.id), &cfg.name)?;

        let vz_config = build_vz_configuration(store, cfg, &serial, headless)?;
        let sleep_inhibitor = Rc::new(RefCell::new(SleepInhibitor::new()));
        let prevent_sleep = Rc::new(Cell::new(cfg.prevent_sleep));
        let sleep_reason = format!("RustVM: {} is running", cfg.name);

        let inh_ev = Rc::clone(&sleep_inhibitor);
        let prev_ev = Rc::clone(&prevent_sleep);
        let reason_ev = sleep_reason.clone();
        let wrapped_cb: EventCallback = Rc::new(move |event: VmEvent| {
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
            on_event(event);
        });

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
            _vm_lock: vm_lock,
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
                (cb)(VmEvent::StateChanged(VmState::Restoring));

                let block = RcBlock::new(move |err: *mut NSError| {
                    let _ = fs::remove_file(&state_path);
                    if unsafe { err.as_ref() }.is_some() {
                        // Snapshot incompatible (e.g., macOS update or config edit) — fall back to clean boot
                        let cb2 = Rc::clone(&cb);
                        (cb2)(VmEvent::StateChanged(VmState::Starting));
                        let start_blk = RcBlock::new(move |start_err: *mut NSError| {
                            if let Some(e) = unsafe { start_err.as_ref() } {
                                (cb2)(VmEvent::ActionError(format!(
                                    "Failed to start VM: {}",
                                    e.localizedDescription()
                                )));
                                (cb2)(VmEvent::StateChanged(VmState::Stopped));
                            } else {
                                (cb2)(VmEvent::StateChanged(VmState::Running));
                            }
                        });
                        unsafe {
                            vm_clone.startWithCompletionHandler(&start_blk);
                        }
                    } else {
                        // Restore succeeded (VM is now Paused) -> immediately resume!
                        let cb2 = Rc::clone(&cb);
                        (cb2)(VmEvent::StateChanged(VmState::Resuming));
                        let resume_blk = RcBlock::new(move |res_err: *mut NSError| {
                            if let Some(e) = unsafe { res_err.as_ref() } {
                                (cb2)(VmEvent::ActionError(format!(
                                    "Failed to resume restored VM: {}",
                                    e.localizedDescription()
                                )));
                                (cb2)(VmEvent::StateChanged(VmState::Paused));
                            } else {
                                (cb2)(VmEvent::StateChanged(VmState::Running));
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

        // Preflight free disk space check so we never fill the host volume mid-snapshot
        let min_required = (self.configured_memory_bytes / 2).max(512 * 1024 * 1024);
        if let Some(avail) = disk::available_space_bytes(saved_state_path) {
            if avail < min_required {
                on_done(Err(format!(
                    "Not enough free disk space on your Mac to save VM state ({} free, need at least {}).",
                    format_bytes(avail),
                    format_bytes(min_required)
                )));
                return;
            }
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
        let on_done_run = Rc::clone(&on_done);
        (cb)(VmEvent::StateChanged(VmState::Saving));

        let run_save = Rc::new(move || {
            let cb_save = Rc::clone(&cb);
            let vm_save = vm.clone();
            let done_save = Rc::clone(&on_done_run);
            let tmp_p = tmp_path.clone();
            let fin_p = final_path.clone();
            let save_block = RcBlock::new(move |err: *mut NSError| {
                if let Some(e) = unsafe { err.as_ref() } {
                    let _ = fs::remove_file(&tmp_p);
                    let msg = format!("Failed to save VM state: {}", e.localizedDescription());
                    (cb_save)(VmEvent::StateChanged(VmState::Paused));
                    (done_save)(Err(msg));
                } else {
                    if let Err(io_err) = fs::rename(&tmp_p, &fin_p) {
                        let _ = fs::remove_file(&tmp_p);
                        (cb_save)(VmEvent::StateChanged(VmState::Paused));
                        (done_save)(Err(format!("Failed to finalize saved state: {io_err}")));
                        return;
                    }
                    let cb_stop = Rc::clone(&cb_save);
                    let done_stop = Rc::clone(&done_save);
                    let stop_block = RcBlock::new(move |_stop_err: *mut NSError| {
                        (cb_stop)(VmEvent::GuestStopped);
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
            let done_pause = Rc::clone(&on_done);
            let pause_block = RcBlock::new(move |err: *mut NSError| {
                if let Some(e) = unsafe { err.as_ref() } {
                    let msg = format!(
                        "Failed to pause VM before saving: {}",
                        e.localizedDescription()
                    );
                    (cb_pause)(VmEvent::StateChanged(VmState::Running));
                    (done_pause)(Err(msg));
                } else {
                    (save_after_pause)();
                }
            });
            unsafe {
                self.vm.pauseWithCompletionHandler(&pause_block);
            }
        } else if self.state() == VmState::Paused {
            (run_save)();
        } else {
            on_done(Err("VM must be running or paused to save state.".into()));
        }
    }

    pub fn start(&self) {
        let cb = Rc::clone(&self.on_event);
        (cb)(VmEvent::StateChanged(VmState::Starting));
        let block = RcBlock::new(move |err: *mut NSError| {
            if let Some(err_ref) = unsafe { err.as_ref() } {
                let msg = err_ref.localizedDescription().to_string();
                let hint = if msg.contains("entitlement")
                    || msg.contains("permission")
                    || msg.contains("privileg")
                {
                    format!(
                        "{msg}\n\nTip: Sign the binary with `./setup.sh` or `codesign --entitlements entitlements.plist --force -s - <binary>`."
                    )
                } else {
                    msg
                };
                (cb)(VmEvent::ActionError(format!("Failed to start VM: {hint}")));
                (cb)(VmEvent::StateChanged(VmState::Stopped));
            } else {
                (cb)(VmEvent::StateChanged(VmState::Running));
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
        (cb)(VmEvent::StateChanged(VmState::Pausing));
        let block = RcBlock::new(move |err: *mut NSError| {
            if let Some(err_ref) = unsafe { err.as_ref() } {
                (cb)(VmEvent::ActionError(format!(
                    "Failed to pause VM: {}",
                    err_ref.localizedDescription()
                )));
                (cb)(VmEvent::StateChanged(VmState::Running));
            } else {
                (cb)(VmEvent::StateChanged(VmState::Paused));
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
        (cb)(VmEvent::StateChanged(VmState::Resuming));
        let block = RcBlock::new(move |err: *mut NSError| {
            if let Some(err_ref) = unsafe { err.as_ref() } {
                (cb)(VmEvent::ActionError(format!(
                    "Failed to resume VM: {}",
                    err_ref.localizedDescription()
                )));
                (cb)(VmEvent::StateChanged(VmState::Paused));
            } else {
                (cb)(VmEvent::StateChanged(VmState::Running));
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
            (self.on_event)(VmEvent::ActionError(format!(
                "Graceful shutdown failed: {}",
                e.localizedDescription()
            )));
        }
    }

    /// Immediately powers off the VM (equivalent to pulling the power cord).
    pub fn force_stop(&self) {
        if !self.can_stop() {
            (self.on_event)(VmEvent::GuestStopped);
            return;
        }
        let cb = Rc::clone(&self.on_event);
        (cb)(VmEvent::StateChanged(VmState::Stopping));
        let block = RcBlock::new(move |err: *mut NSError| {
            if let Some(err_ref) = unsafe { err.as_ref() } {
                (cb)(VmEvent::ActionError(format!(
                    "Force stop error: {}",
                    err_ref.localizedDescription()
                )));
            }
            (cb)(VmEvent::GuestStopped);
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
                (self.on_event)(VmEvent::IsoChanged(None));
                return;
            }
            let ctrl = controllers.objectAtIndex(0);
            let devices = ctrl.usbDevices();
            if devices.is_empty() {
                (self.on_event)(VmEvent::IsoChanged(None));
                return;
            }
            let total = devices.count();
            for i in 0..total {
                let dev = devices.objectAtIndex(i);
                let cb = Rc::clone(&self.on_event);
                let is_last = i + 1 == total;
                let block = RcBlock::new(move |err: *mut NSError| {
                    if let Some(err_ref) = err.as_ref() {
                        (cb)(VmEvent::ActionError(format!(
                            "Failed to eject ISO: {}",
                            err_ref.localizedDescription()
                        )));
                    } else if is_last {
                        (cb)(VmEvent::IsoChanged(None));
                    }
                });
                ctrl.detachDevice_completionHandler(&dev, &block);
            }
        }
    }

    /// Hot-attaches an ISO image as a USB Mass Storage device to the running VM,
    /// cleanly waiting for any existing USB ISO detach completion before attaching.
    pub fn hot_attach_iso(&self, iso_path: &Path) -> Result<()> {
        let usb_cfg = build_usb_iso_config(iso_path)?;
        let iso_owned: PathBuf = iso_path.to_path_buf();
        unsafe {
            let controllers = self.vm.usbControllers();
            if controllers.is_empty() {
                bail!("VM has no USB controller");
            }
            let ctrl = controllers.objectAtIndex(0);
            let usb_dev = VZUSBMassStorageDevice::initWithConfiguration(
                VZUSBMassStorageDevice::alloc(),
                &usb_cfg,
            );
            let usb_proto = ProtocolObject::from_retained(usb_dev);
            let cb = Rc::clone(&self.on_event);

            let do_attach = {
                let ctrl_att = ctrl.clone();
                Rc::new(move || {
                    let cb_att = Rc::clone(&cb);
                    let iso_done = iso_owned.clone();
                    let attach_blk = RcBlock::new(move |err: *mut NSError| {
                        if let Some(err_ref) = err.as_ref() {
                            (cb_att)(VmEvent::ActionError(format!(
                                "Failed to attach ISO: {}",
                                err_ref.localizedDescription()
                            )));
                        } else {
                            (cb_att)(VmEvent::IsoChanged(Some(iso_done.clone())));
                        }
                    });
                    ctrl_att.attachDevice_completionHandler(&usb_proto, &attach_blk);
                })
            };

            let existing = ctrl.usbDevices();
            let total = existing.count();
            if total == 0 {
                (do_attach)();
            } else {
                let remaining = Rc::new(Cell::new(total));
                for i in 0..total {
                    let dev = existing.objectAtIndex(i);
                    let rem = Rc::clone(&remaining);
                    let attach_after = Rc::clone(&do_attach);
                    let detach_blk = RcBlock::new(move |_err: *mut NSError| {
                        let left = rem.get().saturating_sub(1);
                        rem.set(left);
                        if left == 0 {
                            (attach_after)();
                        }
                    });
                    ctrl.detachDevice_completionHandler(&dev, &detach_blk);
                }
            }
        }
        Ok(())
    }

    /// Dynamically reconfigures the Virtio-GPU scanout resolution in pixels.
    /// Proportionally scales within Linux `virtio_gpu`'s 4096×4096 ceiling so 5K/6K 16:9
    /// displays (e.g. 6016×3384) keep their exact 16:9 aspect ratio (4096×2304) with zero side bars.
    pub fn reconfigure_display_ext(&self, width: usize, height: usize, force_hotplug: bool) {
        let (even_w, even_h) = fit_guest_pixels(width, height);
        let w = even_w as f64;
        let h = even_h as f64;
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
            let same_size = (current.width - w).abs() < 2.0 && (current.height - h).abs() < 2.0;
            if same_size {
                if force_hotplug && even_w >= 644 {
                    // Nudge by 2px and back so Linux virtio-gpu emits a fresh DRM hotplug event
                    let _ = display.reconfigureWithSizeInPixels_error(CGSize::new(w - 2.0, h));
                    let _ = display.reconfigureWithSizeInPixels_error(CGSize::new(w, h));
                }
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
        self.sleep_inhibitor.borrow_mut().release();
        if let Some(t) = self.balloon_restore_timer.borrow_mut().take() {
            t.invalidate();
        }
        self.serial.shutdown();
    }
}

/// Proportionally fits `(width, height)` inside Linux `virtio_gpu`'s `4096×4096` DRM max
/// with 2-pixel alignment, preserving the exact screen/window aspect ratio on 4K/5K/6K monitors.
pub fn fit_guest_pixels(width: usize, height: usize) -> (usize, usize) {
    const MAX_DIM: f64 = 4096.0;
    let w = width.max(640) as f64;
    let h = height.max(480) as f64;
    let scale = (MAX_DIM / w).min(MAX_DIM / h).min(1.0);
    let sw = (((w * scale).round() as usize) & !1).clamp(640, 4096);
    let sh = (((h * scale).round() as usize) & !1).clamp(480, 4096);
    (sw, sh)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_guest_pixels_preserves_6k_16_by_9_aspect_ratio() {
        // 6016x3384 (Pro Display XDR / 4K scaled 3008x1692 @ 2x) must become 4096x2304 (exact 16:9),
        // never 4096x2560 (16:10)!
        assert_eq!(fit_guest_pixels(6016, 3384), (4096, 2304));
        // MacBook Pro 14" below notch @ 2x (3024x1900) stays untouched
        assert_eq!(fit_guest_pixels(3024, 1900), (3024, 1900));
    }
}
