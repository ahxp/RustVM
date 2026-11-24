use anyhow::Result;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertSecondButtonReturn, NSAlertStyle,
    NSAutoresizingMaskOptions, NSBackingStoreType, NSBox, NSButton, NSColor, NSScrollView,
    NSTextField, NSTextView, NSWindow, NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::{NSNotification, NSObject, NSObjectProtocol, NSString};
use objc2_virtualization::VZVirtualMachineView;
use std::cell::{Cell, RefCell};
use std::fs;
use std::path::PathBuf;
use std::rc::Rc;

use crate::config::{VmConfig, VmStore, format_memory_mb};
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
    }
);

impl DisplayWindowDelegate {
    fn new<F, G, H>(
        mtm: MainThreadMarker,
        on_should_close: F,
        on_will_close: G,
        on_resize: H,
    ) -> Retained<Self>
    where
        F: Fn() -> bool + 'static,
        G: Fn() + 'static,
        H: Fn() + 'static,
    {
        let this = mtm.alloc::<Self>().set_ivars(DisplayWindowDelegateIvars {
            on_should_close: RefCell::new(Box::new(on_should_close)),
            on_will_close: RefCell::new(Box::new(on_will_close)),
            on_resize: RefCell::new(Box::new(on_resize)),
        });
        unsafe { msg_send![super(this), init] }
    }
}

struct DisplayControls {
    window: Retained<NSWindow>,
    status_dot: Retained<NSBox>,
    status_label: Retained<NSTextField>,
    pause_btn: Retained<NSButton>,
    shutdown_btn: Retained<NSButton>,
    stop_btn: Retained<NSButton>,
    iso_btn: Retained<NSButton>,
    reclaim_btn: Retained<NSButton>,
    footer_label: Retained<NSTextField>,
    vm_name: String,
    vm_cpus: usize,
    vm_memory_mb: u64,
    fast_disk_io: bool,
    shared_folder: Option<PathBuf>,
    hidpi_mode: Cell<bool>,
    current_iso: RefCell<Option<PathBuf>>,
    current_state: Cell<VmState>,
    log_window: RefCell<Option<Retained<NSWindow>>>,
}

impl DisplayControls {
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
                self.shutdown_btn.setEnabled(true);
                self.stop_btn.setEnabled(true);
                self.iso_btn.setEnabled(true);
                self.reclaim_btn.setEnabled(true);
            }
            VmState::Paused => {
                self.pause_btn.setTitle(&NSString::from_str("Resume"));
                self.pause_btn.setEnabled(true);
                self.shutdown_btn.setEnabled(false);
                self.stop_btn.setEnabled(true);
                self.iso_btn.setEnabled(false);
                self.reclaim_btn.setEnabled(false);
            }
            VmState::Starting | VmState::Resuming | VmState::Pausing | VmState::Stopping => {
                self.pause_btn.setEnabled(false);
                self.shutdown_btn.setEnabled(false);
                self.stop_btn.setEnabled(true);
                self.iso_btn.setEnabled(false);
                self.reclaim_btn.setEnabled(false);
            }
            VmState::Stopped | VmState::Error => {
                self.pause_btn.setTitle(&NSString::from_str("Pause"));
                self.pause_btn.setEnabled(false);
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
            "Smooth 1×"
        };
        let disk_mode = if self.fast_disk_io {
            "Fast Disk"
        } else {
            "Strict Disk"
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
            disk_mode,
            iso_desc,
            share_desc
        )));
    }
}

pub struct VmDisplaySession {
    window: Retained<NSWindow>,
    runner: Rc<RefCell<VmRunner>>,
    controls: Rc<DisplayControls>,
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
        let width = (cfg.display_width as f64).clamp(960.0, 1360.0);
        let height = (cfg.display_height as f64).clamp(620.0, 820.0) + 72.0;

        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable;
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                mtm.alloc(),
                rect(100.0, 80.0, width, height),
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe {
            window.setReleasedWhenClosed(false);
        }
        window.setMinSize(size(880.0, 560.0));
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

        let toolbar_h = 46.0;
        let footer_h = 26.0;

        // Top toolbar container
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

        // VZVirtualMachineView:
        // In Smooth 1x mode (default), we disable Apple's Retina 2x auto-reconfigure so
        // software Virtio-GPU renders 4x fewer pixels and feels dramatically smoother.
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
            widgets::semibold_label(mtm, rect(28.0, 14.0, 74.0, 18.0), "Starting…", 12.0);
        toolbar.addSubview(&status_label);

        let pause_btn =
            widgets::push_button(mtm, rect(104.0, 9.0, 70.0, 28.0), "Pause", &mut targets, || {});
        let shutdown_btn = widgets::push_button(
            mtm,
            rect(178.0, 9.0, 84.0, 28.0),
            "Shutdown",
            &mut targets,
            || {},
        );
        let stop_btn =
            widgets::push_button(mtm, rect(266.0, 9.0, 84.0, 28.0), "Force Stop", &mut targets, || {});
        let iso_btn = widgets::push_button(
            mtm,
            rect(356.0, 9.0, 94.0, 28.0),
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
            rect(456.0, 9.0, 84.0, 28.0),
            "Free RAM",
            &mut targets,
            || {},
        );
        let log_btn = widgets::push_button(
            mtm,
            rect(546.0, 9.0, 80.0, 28.0),
            "Serial Log",
            &mut targets,
            || {},
        );

        let hidpi_cb = widgets::checkbox(
            mtm,
            rect(636.0, 13.0, 92.0, 20.0),
            "HiDPI (2×)",
            cfg.hidpi_display,
            &mut targets,
            |_| {},
        );

        let vm_view_for_keys = vm_view.clone();
        let keys_cb = widgets::checkbox(
            mtm,
            rect(734.0, 13.0, 128.0, 20.0),
            "Capture Keys",
            true,
            &mut targets,
            move |checked| unsafe {
                vm_view_for_keys.setCapturesSystemKeys(checked);
            },
        );

        toolbar.addSubview(&pause_btn);
        toolbar.addSubview(&shutdown_btn);
        toolbar.addSubview(&stop_btn);
        toolbar.addSubview(&iso_btn);
        toolbar.addSubview(&reclaim_btn);
        toolbar.addSubview(&log_btn);
        toolbar.addSubview(&hidpi_cb);
        toolbar.addSubview(&keys_cb);

        window.setContentView(Some(&root));

        let controls = Rc::new(DisplayControls {
            window: window.clone(),
            status_dot,
            status_label,
            pause_btn: pause_btn.clone(),
            shutdown_btn: shutdown_btn.clone(),
            stop_btn: stop_btn.clone(),
            iso_btn: iso_btn.clone(),
            reclaim_btn: reclaim_btn.clone(),
            footer_label,
            vm_name: cfg.name.clone(),
            vm_cpus: cfg.cpus,
            vm_memory_mb: cfg.memory_mb,
            fast_disk_io: cfg.fast_disk_io,
            shared_folder: cfg.shared_folder.clone(),
            hidpi_mode: Cell::new(cfg.hidpi_display),
            current_iso: RefCell::new(cfg.iso_path.clone()),
            current_state: Cell::new(VmState::Starting),
            log_window: RefCell::new(None),
        });
        controls.refresh_iso_and_footer();

        let serial_log_path = store.serial_log_path(&cfg.id);
        let serial = SerialBridge::new_log_only(&serial_log_path)?;

        let controls_ev = Rc::clone(&controls);
        let vm_id_for_iso = cfg.id.clone();
        let on_state_rc = Rc::new(on_state_change);
        let on_state_ev = Rc::clone(&on_state_rc);

        let runner = VmRunner::new(store, cfg, serial, false, move |event| match event {
            VmEvent::StateChanged(st) => {
                controls_ev.update_state(st);
                on_state_ev(st);
            }
            VmEvent::GuestStopped => {
                controls_ev.update_state(VmState::Stopped);
                on_state_ev(VmState::Stopped);
            }
            VmEvent::StoppedWithError(msg) => {
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

        unsafe {
            vm_view.setVirtualMachine(Some(runner.vz_vm()));
        }

        let runner = Rc::new(RefCell::new(runner));

        // Helper closure to sync Smooth 1x resolution to the current logical view size
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

        // Bind real toolbar button actions
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

        // Window delegate: prompt before killing running VM, sync 1x display on resize, clean up on close
        let r_close = Rc::clone(&runner);
        let c_close = Rc::clone(&controls);
        let vm_name_close = cfg.name.clone();
        let on_closed_rc = Rc::new(on_closed);
        let on_closed_cb = Rc::clone(&on_closed_rc);
        let r_will_close = Rc::clone(&runner);
        let on_state_close = Rc::clone(&on_state_rc);
        let on_resize = Rc::clone(&sync_1x_resolution);

        let delegate = DisplayWindowDelegate::new(
            mtm,
            move || {
                let st = c_close.current_state.get();
                if !st.is_active() {
                    return true;
                }
                let alert = NSAlert::new(mtm);
                alert.setAlertStyle(NSAlertStyle::Warning);
                alert.setMessageText(&NSString::from_str(&format!(
                    "Close '{}'?",
                    vm_name_close
                )));
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
            },
            move || {
                if r_will_close.borrow().state().is_active() {
                    r_will_close.borrow().force_stop();
                }
                r_will_close.borrow_mut().shutdown_serial();
                on_state_close(VmState::Stopped);
                on_closed_cb();
            },
            move || {
                on_resize();
            },
        );
        window.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));

        window.makeKeyAndOrderFront(None);
        runner.borrow().start();

        Ok(Self {
            window,
            runner,
            controls,
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
