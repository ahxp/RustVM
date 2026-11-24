use anyhow::Result;
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertSecondButtonReturn, NSAlertStyle,
    NSAlertThirdButtonReturn, NSAutoresizingMaskOptions, NSBackingStoreType, NSBox, NSButton,
    NSColor, NSScreen, NSScrollView, NSTextField, NSTextView, NSWindow, NSWindowDelegate,
    NSWindowStyleMask,
};
use objc2_foundation::{NSNotification, NSObject, NSObjectProtocol, NSString, NSTimer};
use objc2_virtualization::VZVirtualMachineView;
use std::cell::{Cell, RefCell};
use std::fs;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::rc::Rc;

use crate::config::{DiskIoMode, VmConfig, VmStore, format_memory_mb};
use crate::gui::widgets::{
    self, FlippedView, TargetStore, pick_iso_file, rect, show_error_alert, size,
};
use crate::vm::console::SerialBridge;
use crate::vm::runner::VmRunner;
use crate::vm::{VmEvent, VmState};

struct DisplayWindowDelegateIvars {
    on_should_close: RefCell<Box<dyn Fn() -> bool>>,
    on_will_close: RefCell<Box<dyn Fn()>>,
    on_resize: RefCell<Box<dyn Fn()>>,
    on_minimize: RefCell<Box<dyn Fn()>>,
    on_resign_key: RefCell<Box<dyn Fn()>>,
    on_become_key: RefCell<Box<dyn Fn()>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = DisplayWindowDelegateIvars]
    struct DisplayWindowDelegate;

    unsafe impl NSObjectProtocol for DisplayWindowDelegate {}

    unsafe impl NSWindowDelegate for DisplayWindowDelegate {
        #[unsafe(method(windowShouldClose:))]
        fn window_should_close(&self, _sender: &NSWindow) -> bool {
            (self.ivars().on_should_close.borrow())()
        }

        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _notification: &NSNotification) {
            (self.ivars().on_will_close.borrow())();
        }

        #[unsafe(method(windowDidEndLiveResize:))]
        fn window_did_end_live_resize(&self, _notification: &NSNotification) {
            (self.ivars().on_resize.borrow())();
        }

        #[unsafe(method(windowDidEnterFullScreen:))]
        fn window_did_enter_full_screen(&self, _notification: &NSNotification) {
            (self.ivars().on_resize.borrow())();
        }

        #[unsafe(method(windowDidExitFullScreen:))]
        fn window_did_exit_full_screen(&self, _notification: &NSNotification) {
            (self.ivars().on_resize.borrow())();
        }

        #[unsafe(method(windowDidMiniaturize:))]
        fn window_did_miniaturize(&self, _notification: &NSNotification) {
            (self.ivars().on_minimize.borrow())();
        }

        #[unsafe(method(windowDidResignKey:))]
        fn window_did_resign_key(&self, _notification: &NSNotification) {
            (self.ivars().on_resign_key.borrow())();
        }

        #[unsafe(method(windowDidBecomeKey:))]
        fn window_did_become_key(&self, _notification: &NSNotification) {
            (self.ivars().on_become_key.borrow())();
        }
    }
);

impl DisplayWindowDelegate {
    fn new<F, G, H, M, R, B>(
        mtm: MainThreadMarker,
        on_should_close: F,
        on_will_close: G,
        on_resize: H,
        on_minimize: M,
        on_resign_key: R,
        on_become_key: B,
    ) -> Retained<Self>
    where
        F: Fn() -> bool + 'static,
        G: Fn() + 'static,
        H: Fn() + 'static,
        M: Fn() + 'static,
        R: Fn() + 'static,
        B: Fn() + 'static,
    {
        let this = mtm.alloc::<Self>().set_ivars(DisplayWindowDelegateIvars {
            on_should_close: RefCell::new(Box::new(on_should_close)),
            on_will_close: RefCell::new(Box::new(on_will_close)),
            on_resize: RefCell::new(Box::new(on_resize)),
            on_minimize: RefCell::new(Box::new(on_minimize)),
            on_resign_key: RefCell::new(Box::new(on_resign_key)),
            on_become_key: RefCell::new(Box::new(on_become_key)),
        });
        unsafe { msg_send![super(this), init] }
    }
}

struct DisplayControls {
    window: Retained<NSWindow>,
    status_dot: Retained<NSBox>,
    status_label: Retained<NSTextField>,
    pause_btn: Retained<NSButton>,
    save_btn: Retained<NSButton>,
    shutdown_btn: Retained<NSButton>,
    stop_btn: Retained<NSButton>,
    iso_btn: Retained<NSButton>,
    reclaim_btn: Retained<NSButton>,
    footer_label: Retained<NSTextField>,
    vm_name: String,
    vm_cpus: usize,
    vm_memory_mb: u64,
    disk_io_mode: DiskIoMode,
    can_save_state: Cell<bool>,
    shared_folder: Option<PathBuf>,
    hidpi_mode: Cell<bool>,
    current_iso: RefCell<Option<PathBuf>>,
    current_state: Cell<VmState>,
    log_window: RefCell<Option<Retained<NSWindow>>>,
    idle_reclaim_timer: RefCell<Option<Retained<NSTimer>>>,
    boot_sync_timers: RefCell<Vec<Retained<NSTimer>>>,
}

impl DisplayControls {
    fn cancel_timers(&self) {
        if let Some(t) = self.idle_reclaim_timer.borrow_mut().take() {
            t.invalidate();
        }
        for t in self.boot_sync_timers.borrow_mut().drain(..) {
            t.invalidate();
        }
    }

    fn update_state(&self, state: VmState) {
        self.current_state.set(state);
        self.window.setTitle(&NSString::from_str(&format!(
            "RustVM — {} [{}]",
            self.vm_name,
            state.label()
        )));
        self.status_label
            .setStringValue(&NSString::from_str(state.label()));
        let dot_color = match state {
            VmState::Running => NSColor::systemGreenColor(),
            VmState::Paused => NSColor::systemOrangeColor(),
            VmState::Saving | VmState::Restoring => NSColor::systemPurpleColor(),
            VmState::Starting | VmState::Resuming | VmState::Pausing | VmState::Stopping => {
                NSColor::systemBlueColor()
            }
            VmState::Error => NSColor::systemRedColor(),
            VmState::Stopped => NSColor::systemGrayColor(),
        };
        self.status_dot.setFillColor(&dot_color);

        match state {
            VmState::Running => {
                self.pause_btn.setTitle(&NSString::from_str("Pause"));
                self.pause_btn.setEnabled(true);
                self.save_btn.setEnabled(self.can_save_state.get());
                self.shutdown_btn.setEnabled(true);
                self.stop_btn.setEnabled(true);
                self.iso_btn.setEnabled(true);
                self.reclaim_btn.setEnabled(true);
            }
            VmState::Paused => {
                self.pause_btn.setTitle(&NSString::from_str("Resume"));
                self.pause_btn.setEnabled(true);
                self.save_btn.setEnabled(self.can_save_state.get());
                self.shutdown_btn.setEnabled(false);
                self.stop_btn.setEnabled(true);
                self.iso_btn.setEnabled(false);
                self.reclaim_btn.setEnabled(false);
            }
            VmState::Starting
            | VmState::Resuming
            | VmState::Pausing
            | VmState::Saving
            | VmState::Restoring
            | VmState::Stopping => {
                self.pause_btn.setEnabled(false);
                self.save_btn.setEnabled(false);
                self.shutdown_btn.setEnabled(false);
                self.stop_btn.setEnabled(!matches!(state, VmState::Saving | VmState::Restoring));
                self.iso_btn.setEnabled(false);
                self.reclaim_btn.setEnabled(false);
            }
            VmState::Stopped | VmState::Error => {
                self.pause_btn.setTitle(&NSString::from_str("Pause"));
                self.pause_btn.setEnabled(false);
                self.save_btn.setEnabled(false);
                self.shutdown_btn.setEnabled(false);
                self.stop_btn.setEnabled(false);
                self.iso_btn.setEnabled(false);
                self.reclaim_btn.setEnabled(false);
            }
        }
        self.refresh_iso_and_footer();
    }

    fn update_iso(&self, iso: Option<PathBuf>) {
        *self.current_iso.borrow_mut() = iso;
        self.refresh_iso_and_footer();
    }

    fn refresh_iso_and_footer(&self) {
        let iso = self.current_iso.borrow();
        if iso.is_some() {
            self.iso_btn.setTitle(&NSString::from_str("Eject ISO"));
        } else {
            self.iso_btn.setTitle(&NSString::from_str("Attach ISO…"));
        }
        let iso_desc = iso
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| format!("ISO: {}", n.to_string_lossy()))
            .unwrap_or_else(|| "ISO: None (Disk)".to_string());
        let display_mode = if self.hidpi_mode.get() {
            "HiDPI 2×"
        } else {
            "Smooth 1× (1:1)"
        };
        let share_desc = if self.shared_folder.is_some() {
            " · Shared: virtiofs"
        } else {
            ""
        };
        self.footer_label.setStringValue(&NSString::from_str(&format!(
            "{} vCPUs · {} · {} · {} · NAT · {}{}",
            self.vm_cpus,
            format_memory_mb(self.vm_memory_mb),
            display_mode,
            self.disk_io_mode.short_label(),
            iso_desc,
            share_desc
        )));
    }
}

pub struct VmDisplaySession {
    window: Retained<NSWindow>,
    runner: Rc<RefCell<VmRunner>>,
    controls: Rc<DisplayControls>,
    saved_state_path: PathBuf,
    _delegate: Retained<DisplayWindowDelegate>,
    _targets: TargetStore,
}

impl VmDisplaySession {
    pub fn launch<F, G>(
        mtm: MainThreadMarker,
        store: &VmStore,
        cfg: &VmConfig,
        on_state_change: F,
        on_closed: G,
    ) -> Result<Self>
    where
        F: Fn(VmState) + 'static,
        G: Fn() + 'static,
    {
        let toolbar_h = 46.0;
        let footer_h = 26.0;

        // Size the window cleanly for exact 1:1 pixel mapping inside the current macOS screen
        let (max_view_w, max_view_h) = NSScreen::mainScreen(mtm)
            .map(|s| {
                let vf = s.visibleFrame();
                (
                    (vf.size.width - 40.0).max(960.0),
                    (vf.size.height - toolbar_h - footer_h - 48.0).max(600.0),
                )
            })
            .unwrap_or((1440.0, 900.0));

        let view_w = (cfg.display_width as f64).clamp(960.0, max_view_w);
        let view_h = (cfg.display_height as f64).clamp(600.0, max_view_h);
        let width = view_w;
        let height = view_h + toolbar_h + footer_h;

        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable;
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                mtm.alloc(),
                rect(80.0, 60.0, width, height),
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe {
            window.setReleasedWhenClosed(false);
        }
        window.setMinSize(size(940.0, 560.0));
        window.setTitle(&NSString::from_str(&format!(
            "RustVM — {} [Starting…]",
            cfg.name
        )));
        window.center();

        let root = FlippedView::new(mtm, rect(0.0, 0.0, width, height));
        root.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );

        let toolbar = FlippedView::new(mtm, rect(0.0, 0.0, width, toolbar_h));
        toolbar.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMaxYMargin,
        );
        root.addSubview(&toolbar);

        let top_sep = widgets::separator(mtm, rect(0.0, toolbar_h - 1.0, width, 1.0));
        top_sep.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMaxYMargin,
        );
        root.addSubview(&top_sep);

        let vm_view_rect = rect(0.0, toolbar_h, width, height - toolbar_h - footer_h);
        let vm_view = unsafe {
            let v = VZVirtualMachineView::initWithFrame(mtm.alloc(), vm_view_rect);
            v.setCapturesSystemKeys(true);
            v.setAutomaticallyReconfiguresDisplay(cfg.hidpi_display);
            v
        };
        vm_view.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        root.addSubview(&vm_view);

        let bottom_sep = widgets::separator(mtm, rect(0.0, height - footer_h, width, 1.0));
        bottom_sep.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMinYMargin,
        );
        root.addSubview(&bottom_sep);

        let footer_label = widgets::secondary_label(
            mtm,
            rect(12.0, height - footer_h + 4.0, width - 24.0, 18.0),
            "",
            11.0,
        );
        footer_label.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMinYMargin,
        );
        root.addSubview(&footer_label);

        let mut targets = TargetStore::new();

        // Toolbar controls
        let status_dot = widgets::status_dot(mtm, 12.0, 18.0, &NSColor::systemBlueColor());
        toolbar.addSubview(&status_dot);

        let status_label =
            widgets::semibold_label(mtm, rect(28.0, 14.0, 72.0, 18.0), "Starting…", 12.0);
        toolbar.addSubview(&status_label);

        let pause_btn =
            widgets::push_button(mtm, rect(102.0, 9.0, 66.0, 28.0), "Pause", &mut targets, || {});
        let save_btn = widgets::push_button(
            mtm,
            rect(172.0, 9.0, 94.0, 28.0),
            "Save & Close",
            &mut targets,
            || {},
        );
        let shutdown_btn = widgets::push_button(
            mtm,
            rect(270.0, 9.0, 80.0, 28.0),
            "Shutdown",
            &mut targets,
            || {},
        );
        let stop_btn =
            widgets::push_button(mtm, rect(354.0, 9.0, 56.0, 28.0), "Stop", &mut targets, || {});
        let iso_btn = widgets::push_button(
            mtm,
            rect(416.0, 9.0, 90.0, 28.0),
            if cfg.iso_path.is_some() {
                "Eject ISO"
            } else {
                "Attach ISO…"
            },
            &mut targets,
            || {},
        );
        let reclaim_btn = widgets::push_button(
            mtm,
            rect(510.0, 9.0, 80.0, 28.0),
            "Free RAM",
            &mut targets,
            || {},
        );
        let log_btn = widgets::push_button(
            mtm,
            rect(594.0, 9.0, 74.0, 28.0),
            "Serial Log",
            &mut targets,
            || {},
        );

        let hidpi_cb = widgets::checkbox(
            mtm,
            rect(678.0, 13.0, 88.0, 20.0),
            "HiDPI (2×)",
            cfg.hidpi_display,
            &mut targets,
            |_| {},
        );

        let vm_view_for_keys = vm_view.clone();
        let keys_cb = widgets::checkbox(
            mtm,
            rect(770.0, 13.0, 114.0, 20.0),
            "Capture Keys",
            true,
            &mut targets,
            move |checked| unsafe {
                vm_view_for_keys.setCapturesSystemKeys(checked);
            },
        );

        toolbar.addSubview(&pause_btn);
        toolbar.addSubview(&save_btn);
        toolbar.addSubview(&shutdown_btn);
        toolbar.addSubview(&stop_btn);
        toolbar.addSubview(&iso_btn);
        toolbar.addSubview(&reclaim_btn);
        toolbar.addSubview(&log_btn);
        toolbar.addSubview(&hidpi_cb);
        toolbar.addSubview(&keys_cb);

        window.setContentView(Some(&root));

        let serial_log_path = store.serial_log_path(&cfg.id);
        let saved_state_path = store.saved_state_path(&cfg.id);
        let serial = SerialBridge::new_log_only(&serial_log_path)?;

        let controls = Rc::new(DisplayControls {
            window: window.clone(),
            status_dot,
            status_label,
            pause_btn: pause_btn.clone(),
            save_btn: save_btn.clone(),
            shutdown_btn: shutdown_btn.clone(),
            stop_btn: stop_btn.clone(),
            iso_btn: iso_btn.clone(),
            reclaim_btn: reclaim_btn.clone(),
            footer_label,
            vm_name: cfg.name.clone(),
            vm_cpus: cfg.cpus,
            vm_memory_mb: cfg.memory_mb,
            disk_io_mode: cfg.disk_io_mode,
            can_save_state: Cell::new(false),
            shared_folder: cfg.shared_folder.clone(),
            hidpi_mode: Cell::new(cfg.hidpi_display),
            current_iso: RefCell::new(cfg.iso_path.clone()),
            current_state: Cell::new(VmState::Starting),
            log_window: RefCell::new(None),
            idle_reclaim_timer: RefCell::new(None),
            boot_sync_timers: RefCell::new(Vec::new()),
        });

        let controls_ev = Rc::clone(&controls);
        let vm_id_for_iso = cfg.id.clone();
        let on_state_rc = Rc::new(on_state_change);
        let on_state_ev = Rc::clone(&on_state_rc);
        let sync_cb_slot: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));
        let sync_slot_ev = Rc::clone(&sync_cb_slot);

        let runner = VmRunner::new(store, cfg, serial, false, move |event| match event {
            VmEvent::StateChanged(st) => {
                controls_ev.update_state(st);
                on_state_ev(st);
                if st == VmState::Running && !controls_ev.hidpi_mode.get() {
                    // Auto-sync 1:1 Smooth 1x resolution shortly after boot so Linux DRM picks it up cleanly
                    if let Some(sync_fn) = sync_slot_ev.borrow().clone() {
                        sync_fn();
                        for delay in [1.5_f64, 4.5_f64] {
                            let s = Rc::clone(&sync_fn);
                            let blk = RcBlock::new(move |_t: NonNull<NSTimer>| {
                                s();
                            });
                            let t = unsafe {
                                NSTimer::scheduledTimerWithTimeInterval_repeats_block(
                                    delay, false, &blk,
                                )
                            };
                            controls_ev.boot_sync_timers.borrow_mut().push(t);
                        }
                    }
                }
            }
            VmEvent::GuestStopped => {
                controls_ev.cancel_timers();
                controls_ev.update_state(VmState::Stopped);
                on_state_ev(VmState::Stopped);
            }
            VmEvent::StoppedWithError(msg) => {
                controls_ev.cancel_timers();
                controls_ev.update_state(VmState::Error);
                on_state_ev(VmState::Error);
                show_error_alert(mtm, "Virtual Machine Stopped", &msg);
            }
            VmEvent::ActionError(msg) => {
                show_error_alert(mtm, "Virtual Machine Error", &msg);
            }
            VmEvent::IsoChanged(new_iso) => {
                controls_ev.update_iso(new_iso.clone());
                if let Ok(st) = VmStore::open_default() {
                    if let Ok(mut saved) = st.load_vm(&vm_id_for_iso) {
                        saved.iso_path = new_iso;
                        let _ = st.save_vm(&saved);
                    }
                }
            }
        })?;

        let can_save = runner.can_save_state();
        controls.can_save_state.set(can_save);
        if let Some(reason) = runner.save_restore_unsupported_reason() {
            save_btn.setToolTip(Some(&NSString::from_str(&format!(
                "Save State unavailable for this configuration: {reason}"
            ))));
        } else {
            save_btn.setToolTip(Some(&NSString::from_str(
                "Freeze VM state to disk for ~1-2s instant resume and 0 MB RAM while closed",
            )));
        }
        save_btn.setEnabled(false);
        controls.refresh_iso_and_footer();

        unsafe {
            vm_view.setVirtualMachine(Some(runner.vz_vm()));
        }

        let runner = Rc::new(RefCell::new(runner));

        let sync_1x_resolution: Rc<dyn Fn()> = {
            let r = Rc::clone(&runner);
            let c = Rc::clone(&controls);
            let view = vm_view.clone();
            Rc::new(move || {
                if c.hidpi_mode.get() {
                    return;
                }
                if c.current_state.get() != VmState::Running {
                    return;
                }
                let bounds = view.bounds();
                let w = (bounds.size.width.round() as usize).clamp(800, 1920);
                let h = (bounds.size.height.round() as usize).clamp(600, 1200);
                r.borrow().reconfigure_display(w, h);
            })
        };
        *sync_cb_slot.borrow_mut() = Some(Rc::clone(&sync_1x_resolution));

        let trigger_save_and_close: Rc<dyn Fn()> = {
            let r = Rc::clone(&runner);
            let win = window.clone();
            let sp = saved_state_path.clone();
            Rc::new(move || {
                let win_done = win.clone();
                r.borrow().save_state_and_stop(&sp, move |res| match res {
                    Ok(()) => {
                        win_done.close();
                    }
                    Err(msg) => {
                        show_error_alert(mtm, "Save State Failed", &msg);
                    }
                });
            })
        };

        // Bind toolbar actions
        {
            let r = Rc::clone(&runner);
            targets.bind_click(mtm, &pause_btn, move || {
                let runner = r.borrow();
                if runner.can_pause() {
                    runner.pause();
                } else if runner.can_resume() {
                    runner.resume();
                }
            });
        }
        {
            let save_fn = Rc::clone(&trigger_save_and_close);
            targets.bind_click(mtm, &save_btn, move || {
                if can_save {
                    save_fn();
                } else {
                    show_error_alert(
                        mtm,
                        "Save State Unavailable",
                        "This VM configuration does not support saving machine state (e.g., an attached USB ISO or unsupported device).",
                    );
                }
            });
        }
        {
            let r = Rc::clone(&runner);
            targets.bind_click(mtm, &shutdown_btn, move || {
                r.borrow().request_stop();
            });
        }
        {
            let r = Rc::clone(&runner);
            targets.bind_click(mtm, &stop_btn, move || {
                r.borrow().force_stop();
            });
        }
        {
            let r = Rc::clone(&runner);
            let c = Rc::clone(&controls);
            targets.bind_click(mtm, &iso_btn, move || {
                let has_iso = c.current_iso.borrow().is_some();
                if has_iso {
                    r.borrow().hot_eject_iso();
                } else if let Some(path) = pick_iso_file(mtm) {
                    if let Err(e) = r.borrow().hot_attach_iso(&path) {
                        show_error_alert(mtm, "Failed to Attach ISO", &e.to_string());
                    }
                }
            });
        }
        {
            let r = Rc::clone(&runner);
            let btn = reclaim_btn.clone();
            targets.bind_click(mtm, &reclaim_btn, move || {
                let btn_done = btn.clone();
                if r.borrow().reclaim_idle_memory(move || {
                    btn_done.setTitle(&NSString::from_str("Free RAM"));
                    btn_done.setEnabled(true);
                }) {
                    btn.setTitle(&NSString::from_str("Freeing…"));
                    btn.setEnabled(false);
                }
            });
        }
        {
            let c = Rc::clone(&controls);
            let log_path = serial_log_path.clone();
            let vm_name = cfg.name.clone();
            targets.bind_click(mtm, &log_btn, move || {
                let win = open_serial_log_window(mtm, &vm_name, &log_path);
                *c.log_window.borrow_mut() = Some(win);
            });
        }
        {
            let c = Rc::clone(&controls);
            let view = vm_view.clone();
            let sync_1x = Rc::clone(&sync_1x_resolution);
            let vm_id_hidpi = cfg.id.clone();
            targets.bind(mtm, &hidpi_cb, move |sender| {
                let state: isize = unsafe { msg_send![sender, state] };
                let enabled = state == objc2_app_kit::NSControlStateValueOn;
                c.hidpi_mode.set(enabled);
                unsafe {
                    view.setAutomaticallyReconfiguresDisplay(enabled);
                }
                if !enabled {
                    sync_1x();
                }
                c.refresh_iso_and_footer();
                if let Ok(st) = VmStore::open_default() {
                    if let Ok(mut saved) = st.load_vm(&vm_id_hidpi) {
                        saved.hidpi_display = enabled;
                        let _ = st.save_vm(&saved);
                    }
                }
            });
        }

        let r_close = Rc::clone(&runner);
        let c_close = Rc::clone(&controls);
        let vm_name_close = cfg.name.clone();
        let save_on_close = Rc::clone(&trigger_save_and_close);
        let on_closed_rc = Rc::new(on_closed);
        let on_closed_cb = Rc::clone(&on_closed_rc);
        let r_will_close = Rc::clone(&runner);
        let c_will_close = Rc::clone(&controls);
        let on_resize = Rc::clone(&sync_1x_resolution);

        // Auto-reclaim idle RAM when minimized or unfocused
        let auto_reclaim = cfg.auto_reclaim_ram;
        let r_min = Rc::clone(&runner);
        let c_min = Rc::clone(&controls);
        let r_resign = Rc::clone(&runner);
        let c_resign = Rc::clone(&controls);
        let c_become = Rc::clone(&controls);

        let delegate = DisplayWindowDelegate::new(
            mtm,
            move || {
                let st = c_close.current_state.get();
                if !st.is_active() {
                    return true;
                }
                if matches!(st, VmState::Saving | VmState::Restoring) {
                    return false;
                }
                let alert = NSAlert::new(mtm);
                alert.setAlertStyle(NSAlertStyle::Warning);
                alert.setMessageText(&NSString::from_str(&format!(
                    "Close '{}'?",
                    vm_name_close
                )));
                if can_save {
                    alert.setInformativeText(&NSString::from_str(
                        "Save State freezes Linux to disk (0 MB RAM while closed, ~1-2s instant resume). Or shut down / power off.",
                    ));
                    alert.addButtonWithTitle(&NSString::from_str("Save State & Close"));
                    alert.addButtonWithTitle(&NSString::from_str("Force Power Off"));
                    alert.addButtonWithTitle(&NSString::from_str("Graceful Shutdown"));
                    alert.addButtonWithTitle(&NSString::from_str("Cancel"));
                    let resp = alert.runModal();
                    if resp == NSAlertFirstButtonReturn {
                        save_on_close();
                        false
                    } else if resp == NSAlertSecondButtonReturn {
                        r_close.borrow().force_stop();
                        true
                    } else if resp == NSAlertThirdButtonReturn {
                        r_close.borrow().request_stop();
                        false
                    } else {
                        false
                    }
                } else {
                    alert.setInformativeText(&NSString::from_str(
                        "This virtual machine is still running. You can send a graceful shutdown signal or force power it off.",
                    ));
                    alert.addButtonWithTitle(&NSString::from_str("Force Power Off"));
                    alert.addButtonWithTitle(&NSString::from_str("Graceful Shutdown"));
                    alert.addButtonWithTitle(&NSString::from_str("Cancel"));
                    let resp = alert.runModal();
                    if resp == NSAlertFirstButtonReturn {
                        r_close.borrow().force_stop();
                        true
                    } else if resp == NSAlertSecondButtonReturn {
                        r_close.borrow().request_stop();
                        false
                    } else {
                        false
                    }
                }
            },
            move || {
                c_will_close.cancel_timers();
                if r_will_close.borrow().state().is_active() {
                    r_will_close.borrow().force_stop();
                }
                r_will_close.borrow_mut().shutdown_serial();
                on_closed_cb();
            },
            move || {
                on_resize();
            },
            move || {
                if auto_reclaim && c_min.current_state.get() == VmState::Running {
                    let _ = r_min.borrow().reclaim_idle_memory(|| {});
                }
            },
            move || {
                if !auto_reclaim || c_resign.current_state.get() != VmState::Running {
                    return;
                }
                if let Some(prev) = c_resign.idle_reclaim_timer.borrow_mut().take() {
                    prev.invalidate();
                }
                let r_idle = Rc::clone(&r_resign);
                let c_idle = Rc::clone(&c_resign);
                let blk = RcBlock::new(move |_t: NonNull<NSTimer>| {
                    if c_idle.current_state.get() == VmState::Running {
                        let _ = r_idle.borrow().reclaim_idle_memory(|| {});
                    }
                });
                let timer = unsafe {
                    NSTimer::scheduledTimerWithTimeInterval_repeats_block(45.0, false, &blk)
                };
                *c_resign.idle_reclaim_timer.borrow_mut() = Some(timer);
            },
            move || {
                if let Some(t) = c_become.idle_reclaim_timer.borrow_mut().take() {
                    t.invalidate();
                }
            },
        );
        window.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));

        window.makeKeyAndOrderFront(None);
        runner.borrow().start_or_restore(&saved_state_path);

        Ok(Self {
            window,
            runner,
            controls,
            saved_state_path,
            _delegate: delegate,
            _targets: targets,
        })
    }

    pub fn focus(&self) {
        self.window.makeKeyAndOrderFront(None);
    }

    pub fn state(&self) -> VmState {
        self.controls.current_state.get()
    }

    pub fn can_save_state(&self) -> bool {
        self.runner.borrow().can_save_state()
    }

    pub fn save_state_and_close(&self) {
        let win = self.window.clone();
        self.runner
            .borrow()
            .save_state_and_stop(&self.saved_state_path, move |res| {
                if res.is_ok() {
                    win.close();
                }
            });
    }

    pub fn pause_or_resume(&self) {
        let r = self.runner.borrow();
        if r.can_pause() {
            r.pause();
        } else if r.can_resume() {
            r.resume();
        }
    }

    pub fn request_stop(&self) {
        self.runner.borrow().request_stop();
    }

    pub fn force_stop(&self) {
        self.runner.borrow().force_stop();
        self.window.close();
    }

    pub fn hot_attach_iso(&self, path: &PathBuf) -> Result<()> {
        self.runner.borrow().hot_attach_iso(path)
    }

    pub fn hot_eject_iso(&self) {
        self.runner.borrow().hot_eject_iso();
    }
}

fn open_serial_log_window(
    mtm: MainThreadMarker,
    vm_name: &str,
    log_path: &PathBuf,
) -> Retained<NSWindow> {
    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Resizable;
    let win = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            mtm.alloc(),
            rect(180.0, 140.0, 740.0, 480.0),
            style,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe {
        win.setReleasedWhenClosed(false);
    }
    win.setTitle(&NSString::from_str(&format!(
        "Serial Console Log — {vm_name}"
    )));
    win.center();

    let scroll = NSScrollView::initWithFrame(mtm.alloc(), rect(0.0, 0.0, 740.0, 480.0));
    scroll.setHasVerticalScroller(true);
    scroll.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );

    let text_view = NSTextView::initWithFrame(mtm.alloc(), rect(0.0, 0.0, 720.0, 480.0));
    text_view.setEditable(false);
    text_view.setSelectable(true);
    text_view.setFont(Some(&objc2_app_kit::NSFont::monospacedSystemFontOfSize_weight(
        11.5,
        unsafe { objc2_app_kit::NSFontWeightRegular },
    )));

    let content = fs::read_to_string(log_path).unwrap_or_else(|_| {
        "(Serial log is empty — graphical Linux installers output to the VM display window.)\n"
            .to_string()
    });
    let display_text = if content.trim().is_empty() {
        format!(
            "(No serial console output yet. Log path: {})\n",
            log_path.display()
        )
    } else {
        let bytes = content.as_bytes();
        if bytes.len() > 65536 {
            String::from_utf8_lossy(&bytes[bytes.len() - 65536..]).into_owned()
        } else {
            content
        }
    };
    text_view.setString(&NSString::from_str(&display_text));
    scroll.setDocumentView(Some(&text_view));
    win.setContentView(Some(&scroll));
    win.makeKeyAndOrderFront(None);
    win
}
