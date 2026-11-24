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
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::rc::Rc;

use crate::config::{VmConfig, VmStore};
use crate::vm::config_builder::{build_usb_iso_config, build_vz_configuration};
use crate::vm::console::SerialBridge;
use crate::vm::{VmEvent, VmState, is_virtualization_supported};

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
    balloon_restore_timer: RefCell<Option<Retained<NSTimer>>>,
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
        let on_event: EventCallback = Rc::new(RefCell::new(Box::new(on_event)));
        let delegate = RustVmDelegate::new(Rc::clone(&on_event));

        let configured_memory_bytes = unsafe { vz_config.memorySize() };
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
            on_event,
            configured_memory_bytes,
            balloon_restore_timer: RefCell::new(None),
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

    /// Dynamically reconfigures the Virtio-GPU scanout resolution in pixels (used for Smooth 1x mode).
    pub fn reconfigure_display(&self, width: usize, height: usize) {
        let w = ((width / 8) * 8).clamp(800, 3840) as f64;
        let h = ((height / 8) * 8).clamp(600, 2160) as f64;
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
            if (current.width - w).abs() < 4.0 && (current.height - h).abs() < 4.0 {
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

    pub fn shutdown_serial(&mut self) {
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
