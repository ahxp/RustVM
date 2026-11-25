use anyhow::Result;
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertSecondButtonReturn, NSAlertStyle,
    NSAlertThirdButtonReturn, NSApplicationPresentationOptions, NSAutoresizingMaskOptions,
    NSBackingStoreType, NSBox, NSButton, NSColor, NSScreen, NSScrollView, NSTextField, NSTextView,
    NSWindow, NSWindowCollectionBehavior, NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::{NSNotification, NSObject, NSObjectProtocol, NSString, NSTimer};
use objc2_virtualization::VZVirtualMachineView;
use std::cell::{Cell, RefCell};
use std::fs;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::rc::Rc;

use crate::config::{DiskIoMode, DisplayScale, VmConfig, VmStore, format_memory_mb};
use crate::gui::widgets::{
    self, FlippedView, TargetStore, defer_error_alert, pick_iso_file, rect, show_error_alert, size,
};
use crate::vm::console::SerialBridge;
use crate::vm::runner::{VmRunner, fit_guest_pixels};
use crate::vm::{VmEvent, VmState};

const TOOLBAR_H: f64 = 46.0;
const FOOTER_H: f64 = 26.0;

struct DisplayWindowDelegateIvars {
    on_should_close: RefCell<Box<dyn Fn() -> bool>>,
    on_will_close: RefCell<Box<dyn Fn()>>,
    on_layout: RefCell<Box<dyn Fn()>>,
    on_end_resize: RefCell<Box<dyn Fn()>>,
    on_change_screen: RefCell<Box<dyn Fn()>>,
    on_will_enter_fs: RefCell<Box<dyn Fn()>>,
    on_did_enter_fs: RefCell<Box<dyn Fn()>>,
    on_will_exit_fs: RefCell<Box<dyn Fn()>>,
    on_did_exit_fs: RefCell<Box<dyn Fn()>>,
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

        #[unsafe(method(windowDidResize:))]
        fn window_did_resize(&self, _notification: &NSNotification) {
            (self.ivars().on_layout.borrow())();
        }

        #[unsafe(method(windowDidEndLiveResize:))]
        fn window_did_end_live_resize(&self, _notification: &NSNotification) {
            (self.ivars().on_end_resize.borrow())();
        }

        #[unsafe(method(windowDidChangeScreen:))]
        fn window_did_change_screen(&self, _notification: &NSNotification) {
            (self.ivars().on_change_screen.borrow())();
        }

        #[unsafe(method(window:willUseFullScreenPresentationOptions:))]
        fn will_use_fullscreen_options(
            &self,
            _window: &NSWindow,
            _proposed: NSApplicationPresentationOptions,
        ) -> NSApplicationPresentationOptions {
            NSApplicationPresentationOptions::FullScreen
                | NSApplicationPresentationOptions::AutoHideMenuBar
                | NSApplicationPresentationOptions::AutoHideDock
        }

        #[unsafe(method(windowWillEnterFullScreen:))]
        fn window_will_enter_full_screen(&self, _notification: &NSNotification) {
            (self.ivars().on_will_enter_fs.borrow())();
        }

        #[unsafe(method(windowDidEnterFullScreen:))]
        fn window_did_enter_full_screen(&self, _notification: &NSNotification) {
            (self.ivars().on_did_enter_fs.borrow())();
        }

        #[unsafe(method(windowWillExitFullScreen:))]
        fn window_will_exit_full_screen(&self, _notification: &NSNotification) {
            (self.ivars().on_will_exit_fs.borrow())();
        }

        #[unsafe(method(windowDidExitFullScreen:))]
        fn window_did_exit_full_screen(&self, _notification: &NSNotification) {
            (self.ivars().on_did_exit_fs.borrow())();
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
    #[allow(clippy::too_many_arguments)]
    fn new<F, G, L, H, S, A, B, C, D, M, R, K>(
        mtm: MainThreadMarker,
        on_should_close: F,
        on_will_close: G,
        on_layout: L,
        on_end_resize: H,
        on_change_screen: S,
        on_will_enter_fs: A,
        on_did_enter_fs: B,
        on_will_exit_fs: C,
        on_did_exit_fs: D,
        on_minimize: M,
        on_resign_key: R,
        on_become_key: K,
    ) -> Retained<Self>
    where
        F: Fn() -> bool + 'static,
        G: Fn() + 'static,
        L: Fn() + 'static,
        H: Fn() + 'static,
        S: Fn() + 'static,
        A: Fn() + 'static,
        B: Fn() + 'static,
        C: Fn() + 'static,
        D: Fn() + 'static,
        M: Fn() + 'static,
        R: Fn() + 'static,
        K: Fn() + 'static,
    {
        let this = mtm.alloc::<Self>().set_ivars(DisplayWindowDelegateIvars {
            on_should_close: RefCell::new(Box::new(on_should_close)),
            on_will_close: RefCell::new(Box::new(on_will_close)),
            on_layout: RefCell::new(Box::new(on_layout)),
            on_end_resize: RefCell::new(Box::new(on_end_resize)),
            on_change_screen: RefCell::new(Box::new(on_change_screen)),
            on_will_enter_fs: RefCell::new(Box::new(on_will_enter_fs)),
            on_did_enter_fs: RefCell::new(Box::new(on_did_enter_fs)),
            on_will_exit_fs: RefCell::new(Box::new(on_will_exit_fs)),
            on_did_exit_fs: RefCell::new(Box::new(on_did_exit_fs)),
            on_minimize: RefCell::new(Box::new(on_minimize)),
            on_resign_key: RefCell::new(Box::new(on_resign_key)),
            on_become_key: RefCell::new(Box::new(on_become_key)),
        });
        unsafe { msg_send![super(this), init] }
    }
}

/// Computes the exact native fullscreen pixel dimensions of the target `NSWindow`'s screen
/// (falling back to `mainScreen`), preserving exact aspect ratio within 4096×4096 even on
/// 5K/6K 16:9 monitors (e.g. 6016×3384 -> 4096×2304) and notched MacBook displays.
fn screen_fullscreen_pixels(
    window: Option<&NSWindow>,
    mtm: MainThreadMarker,
    scale: DisplayScale,
) -> (usize, usize, f64, f64, f64) {
    let screen = window
        .and_then(|w| w.screen())
        .or_else(|| NSScreen::mainScreen(mtm));
    let (fs_pts_w, fs_pts_h, backing_scale) = screen
        .map(|s| {
            let f = s.frame();
            let top_inset = s.safeAreaInsets().top;
            let w = f.size.width.max(960.0);
            let h = (f.size.height - top_inset).max(600.0);
            let b = s.backingScaleFactor().max(1.0);
            (w, h, b)
        })
        .unwrap_or((1440.0, 900.0, 2.0));

    let factor = match scale {
        DisplayScale::Smooth1x => 1.0,
        DisplayScale::Crisp15x => 1.5_f64.min(backing_scale),
        DisplayScale::Retina2x => backing_scale,
    };
    let raw_w = (fs_pts_w * factor).round().max(640.0) as usize;
    let raw_h = (fs_pts_h * factor).round().max(480.0) as usize;
    let (px_w, px_h) = fit_guest_pixels(raw_w, raw_h);
    (px_w, px_h, fs_pts_w, fs_pts_h, factor)
}

struct DisplayControls {
    window: Retained<NSWindow>,
    root: Retained<FlippedView>,
    toolbar: Retained<FlippedView>,
    top_sep: Retained<NSBox>,
    bottom_sep: Retained<NSBox>,
    footer_bar: Retained<FlippedView>,
    vm_view: Retained<VZVirtualMachineView>,
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
    display_scale: Cell<DisplayScale>,
    prevent_sleep: Cell<bool>,
    active_pixels: Cell<(usize, usize)>,
    in_fullscreen: Cell<bool>,
    current_iso: RefCell<Option<PathBuf>>,
    current_state: Cell<VmState>,
    log_window: RefCell<Option<Retained<NSWindow>>>,
    idle_reclaim_timer: RefCell<Option<Retained<NSTimer>>>,
    resize_debounce_timer: RefCell<Option<Retained<NSTimer>>>,
    sync_timer: RefCell<Option<Retained<NSTimer>>>,
}

impl DisplayControls {
    fn cancel_timers(&self) {
        if let Some(t) = self.idle_reclaim_timer.borrow_mut().take() {
            t.invalidate();
        }
        if let Some(t) = self.resize_debounce_timer.borrow_mut().take() {
            t.invalidate();
        }
        if let Some(t) = self.sync_timer.borrow_mut().take() {
            t.invalidate();
        }
    }

    /// Forces exact edge-to-edge layout in fullscreen (0px chrome) or clean toolbar/footer in windowed mode.
    fn relayout(&self) {
        let fullscreen = self.in_fullscreen.get();
        self.toolbar.setHidden(fullscreen);
        self.top_sep.setHidden(fullscreen);
        self.bottom_sep.setHidden(fullscreen);
        self.footer_bar.setHidden(fullscreen);

        let b = self.root.bounds();
        let w = b.size.width.max(100.0);
        let h = b.size.height.max(100.0);
        if fullscreen {
            self.vm_view.setFrame(rect(0.0, 0.0, w, h));
        } else {
            self.toolbar.setFrame(rect(0.0, 0.0, w, TOOLBAR_H));
            self.top_sep.setFrame(rect(0.0, TOOLBAR_H - 1.0, w, 1.0));
            let view_h = (h - TOOLBAR_H - FOOTER_H).max(200.0);
            self.vm_view.setFrame(rect(0.0, TOOLBAR_H, w, view_h));
            self.bottom_sep.setFrame(rect(0.0, h - FOOTER_H, w, 1.0));
            self.footer_bar.setFrame(rect(0.0, h - FOOTER_H, w, FOOTER_H));
        }
    }

    fn set_fullscreen(&self, fullscreen: bool) {
        self.in_fullscreen.set(fullscreen);
        self.relayout();
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
                self.stop_btn
                    .setEnabled(!matches!(state, VmState::Saving | VmState::Restoring));
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
        let (pw, ph) = self.active_pixels.get();
        let display_desc = format!(
            "HiDPI {} ({}×{})",
            self.display_scale.get().short_label(),
            pw,
            ph
        );
        let awake_desc = if self.prevent_sleep.get() {
            " · Keep Awake"
        } else {
            ""
        };
        let share_desc = if self.shared_folder.is_some() {
            " · Shared: virtiofs"
        } else {
            ""
        };
        self.footer_label.setStringValue(&NSString::from_str(&format!(
            "{} vCPUs · {} · {} · {} · NAT{} · {}{}",
            self.vm_cpus,
            format_memory_mb(self.vm_memory_mb),
            display_desc,
            self.disk_io_mode.short_label(),
            awake_desc,
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
        let initial_scale = cfg.effective_display_scale();

        let (_fs_px_w, _fs_px_h, fs_pts_w, fs_pts_h, scale_factor) =
            screen_fullscreen_pixels(None, mtm, initial_scale);

        // Size the windowed view to preserve the EXACT screen aspect ratio inside visibleFrame
        let (max_view_w, max_view_h) = NSScreen::mainScreen(mtm)
            .map(|s| {
                let vf = s.visibleFrame();
                (
                    (vf.size.width - 48.0).max(980.0),
                    (vf.size.height - TOOLBAR_H - FOOTER_H - 44.0).max(600.0),
                )
            })
            .unwrap_or((1360.0, 820.0));

        let aspect = fs_pts_w / fs_pts_h.max(1.0);
        let mut view_h = max_view_h.min(760.0);
        let mut view_w = (view_h * aspect).round();
        if view_w > max_view_w {
            view_w = max_view_w;
            view_h = (view_w / aspect).round();
        }
        if view_w < 1148.0 {
            view_w = 1148.0;
            view_h = (view_w / aspect).round().clamp(560.0, max_view_h);
        }
        view_h = view_h.max(560.0);

        // Boot Virtio-GPU at the exact initial window view dimensions (aspect-matched!)
        let (boot_px_w, boot_px_h) = fit_guest_pixels(
            (view_w * scale_factor).round() as usize,
            (view_h * scale_factor).round() as usize,
        );
        let mut launch_cfg = cfg.clone();
        launch_cfg.display_width = boot_px_w;
        launch_cfg.display_height = boot_px_h;

        let width = view_w;
        let height = view_h + TOOLBAR_H + FOOTER_H;

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
        window.setCollectionBehavior(NSWindowCollectionBehavior::FullScreenPrimary);
        window.setBackgroundColor(Some(&NSColor::blackColor()));
        window.setMinSize(size(1148.0, 560.0));
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

        let toolbar_bg = widgets::tint_card(
            mtm,
            rect(0.0, 0.0, width, TOOLBAR_H),
            &NSColor::windowBackgroundColor(),
            &NSColor::clearColor(),
            0.0,
        )
        .0;
        toolbar_bg.setBorderWidth(0.0);
        toolbar_bg.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        let toolbar = FlippedView::new(mtm, rect(0.0, 0.0, width, TOOLBAR_H));
        toolbar.addSubview(&toolbar_bg);
        root.addSubview(&toolbar);

        let top_sep = widgets::separator(mtm, rect(0.0, TOOLBAR_H - 1.0, width, 1.0));
        root.addSubview(&top_sep);

        let vm_view_rect = rect(0.0, TOOLBAR_H, width, height - TOOLBAR_H - FOOTER_H);
        let vm_view = unsafe {
            let v = VZVirtualMachineView::initWithFrame(mtm.alloc(), vm_view_rect);
            v.setCapturesSystemKeys(true);
            // Keep manual debounced resolution sync as single source of truth across all scales
            // so VZVirtualMachineView never fights fullscreen/windowed transitions.
            v.setAutomaticallyReconfiguresDisplay(false);
            v
        };
        root.addSubview(&vm_view);

        let bottom_sep = widgets::separator(mtm, rect(0.0, height - FOOTER_H, width, 1.0));
        root.addSubview(&bottom_sep);

        let footer_bar = FlippedView::new(mtm, rect(0.0, height - FOOTER_H, width, FOOTER_H));
        let footer_bg = widgets::tint_card(
            mtm,
            rect(0.0, 0.0, width, FOOTER_H),
            &NSColor::windowBackgroundColor(),
            &NSColor::clearColor(),
            0.0,
        )
        .0;
        footer_bg.setBorderWidth(0.0);
        footer_bg.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        footer_bar.addSubview(&footer_bg);

        let footer_label = widgets::secondary_label(
            mtm,
            rect(12.0, 4.0, width - 24.0, 18.0),
            "",
            11.0,
        );
        footer_label.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        footer_bar.addSubview(&footer_label);
        root.addSubview(&footer_bar);

        let mut targets = TargetStore::new();

        // Toolbar controls (generously sized so no button or checkbox ever truncates)
        let status_dot = widgets::status_dot(mtm, 10.0, 18.0, &NSColor::systemBlueColor());
        toolbar.addSubview(&status_dot);

        let status_label =
            widgets::semibold_label(mtm, rect(24.0, 14.0, 64.0, 18.0), "Starting…", 11.5);
        toolbar.addSubview(&status_label);

        let pause_btn =
            widgets::push_button(mtm, rect(90.0, 9.0, 68.0, 28.0), "Pause", &mut targets, || {});
        let save_btn = widgets::push_button(
            mtm,
            rect(162.0, 9.0, 104.0, 28.0),
            "Save & Close",
            &mut targets,
            || {},
        );
        let shutdown_btn = widgets::push_button(
            mtm,
            rect(270.0, 9.0, 82.0, 28.0),
            "Shutdown",
            &mut targets,
            || {},
        );
        let stop_btn =
            widgets::push_button(mtm, rect(356.0, 9.0, 56.0, 28.0), "Stop", &mut targets, || {});
        let iso_btn = widgets::push_button(
            mtm,
            rect(416.0, 9.0, 98.0, 28.0),
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
            rect(518.0, 9.0, 82.0, 28.0),
            "Free RAM",
            &mut targets,
            || {},
        );
        let log_btn =
            widgets::push_button(mtm, rect(604.0, 9.0, 50.0, 28.0), "Log", &mut targets, || {});

        // Explicit HiDPI label + 3-mode dropdown so HiDPI is immediately visible
        let hidpi_lbl = widgets::semibold_label(mtm, rect(662.0, 14.0, 44.0, 18.0), "HiDPI:", 11.5);
        toolbar.addSubview(&hidpi_lbl);

        let scale_items = ["1× Off", "1.5× Crisp", "2× Retina"];
        let scale_popup = widgets::popup(
            mtm,
            rect(706.0, 10.0, 106.0, 26.0),
            &scale_items,
            initial_scale.to_index(),
            &mut targets,
            |_| {},
        );
        scale_popup.setToolTip(Some(&NSString::from_str(
            "HiDPI Display Mode: 1× Off (fastest), 1.5× Crisp (balanced sharpness), 2× Retina (native Liquid Retina clarity)",
        )));

        let win_for_fs = window.clone();
        let fs_btn = widgets::push_button(
            mtm,
            rect(816.0, 9.0, 98.0, 28.0),
            "⛶ Fullscreen",
            &mut targets,
            move || {
                win_for_fs.toggleFullScreen(None);
            },
        );
        fs_btn.setToolTip(Some(&NSString::from_str(
            "Enter true edge-to-edge native fullscreen (fills entire MacBook screen with zero bars)",
        )));

        let vm_view_for_keys = vm_view.clone();
        let keys_cb = widgets::checkbox(
            mtm,
            rect(920.0, 13.0, 112.0, 20.0),
            "Capture Keys",
            true,
            &mut targets,
            move |checked| unsafe {
                vm_view_for_keys.setCapturesSystemKeys(checked);
            },
        );

        let awake_cb = widgets::checkbox(
            mtm,
            rect(1036.0, 13.0, 102.0, 20.0),
            "Keep Awake",
            cfg.prevent_sleep,
            &mut targets,
            |_| {},
        );
        awake_cb.setToolTip(Some(&NSString::from_str(
            "Prevent macOS from sleeping or turning off the display while this VM is running",
        )));

        toolbar.addSubview(&pause_btn);
        toolbar.addSubview(&save_btn);
        toolbar.addSubview(&shutdown_btn);
        toolbar.addSubview(&stop_btn);
        toolbar.addSubview(&iso_btn);
        toolbar.addSubview(&reclaim_btn);
        toolbar.addSubview(&log_btn);
        toolbar.addSubview(&scale_popup);
        toolbar.addSubview(&fs_btn);
        toolbar.addSubview(&keys_cb);
        toolbar.addSubview(&awake_cb);

        window.setContentView(Some(&root));

        let serial_log_path = store.serial_log_path(&cfg.id);
        let saved_state_path = store.saved_state_path(&cfg.id);
        let serial = SerialBridge::new_log_only(&serial_log_path)?;

        let controls = Rc::new(DisplayControls {
            window: window.clone(),
            root: root.clone(),
            toolbar: toolbar.clone(),
            top_sep: top_sep.clone(),
            bottom_sep: bottom_sep.clone(),
            footer_bar: footer_bar.clone(),
            vm_view: vm_view.clone(),
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
            display_scale: Cell::new(initial_scale),
            prevent_sleep: Cell::new(cfg.prevent_sleep),
            active_pixels: Cell::new((boot_px_w, boot_px_h)),
            in_fullscreen: Cell::new(false),
            current_iso: RefCell::new(cfg.iso_path.clone()),
            current_state: Cell::new(VmState::Starting),
            log_window: RefCell::new(None),
            idle_reclaim_timer: RefCell::new(None),
            resize_debounce_timer: RefCell::new(None),
            sync_timer: RefCell::new(None),
        });

        let controls_ev = Rc::clone(&controls);
        let vm_id_for_iso = cfg.id.clone();
        let on_state_rc = Rc::new(on_state_change);
        let on_state_ev = Rc::clone(&on_state_rc);
        let running_sync_hook: Rc<RefCell<Option<Rc<dyn Fn(bool)>>>> = Rc::new(RefCell::new(None));
        let running_sync_ev = Rc::clone(&running_sync_hook);

        let runner = VmRunner::new(store, &launch_cfg, serial, false, move |event| match event {
            VmEvent::StateChanged(st) => {
                controls_ev.update_state(st);
                if st == VmState::Running {
                    if let Some(ref sync) = *running_sync_ev.borrow() {
                        sync(controls_ev.in_fullscreen.get());
                    }
                }
                on_state_ev(st);
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
                defer_error_alert(mtm, "Virtual Machine Stopped", &msg);
            }
            VmEvent::ActionError(msg) => {
                defer_error_alert(mtm, "Virtual Machine Error", &msg);
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

        // Syncs Virtio-GPU scanout resolution cleanly to either exact fullscreen pixels or
        // exact windowed view bounds, proportionally fitted within 4096×4096 so 5K/6K 16:9
        // displays never distort into 16:10.
        let sync_display_resolution: Rc<dyn Fn(bool, bool)> = {
            let r = Rc::clone(&runner);
            let c = Rc::clone(&controls);
            let view = vm_view.clone();
            Rc::new(move |use_native_fs_size: bool, force_hotplug: bool| {
                if c.current_state.get() != VmState::Running {
                    return;
                }
                let scale = c.display_scale.get();
                let (even_w, even_h) = if use_native_fs_size || c.in_fullscreen.get() {
                    let bounds = view.bounds();
                    if c.in_fullscreen.get()
                        && bounds.size.width >= 600.0
                        && bounds.size.height >= 400.0
                    {
                        let backing = view.convertRectToBacking(bounds);
                        let (rw, rh) = match scale {
                            DisplayScale::Smooth1x => (
                                bounds.size.width.round() as usize,
                                bounds.size.height.round() as usize,
                            ),
                            DisplayScale::Crisp15x => (
                                (bounds.size.width * 1.5).min(backing.size.width).round() as usize,
                                (bounds.size.height * 1.5).min(backing.size.height).round()
                                    as usize,
                            ),
                            DisplayScale::Retina2x => (
                                backing.size.width.round() as usize,
                                backing.size.height.round() as usize,
                            ),
                        };
                        fit_guest_pixels(rw, rh)
                    } else {
                        let (fw, fh, _, _, _) =
                            screen_fullscreen_pixels(Some(&c.window), mtm, scale);
                        (fw, fh)
                    }
                } else {
                    let bounds = view.bounds();
                    if bounds.size.width < 100.0 || bounds.size.height < 100.0 {
                        return;
                    }
                    let backing = view.convertRectToBacking(bounds);
                    let (rw, rh) = match scale {
                        DisplayScale::Smooth1x => (
                            bounds.size.width.round() as usize,
                            bounds.size.height.round() as usize,
                        ),
                        DisplayScale::Crisp15x => (
                            (bounds.size.width * 1.5).min(backing.size.width).round() as usize,
                            (bounds.size.height * 1.5).min(backing.size.height).round() as usize,
                        ),
                        DisplayScale::Retina2x => (
                            backing.size.width.round() as usize,
                            backing.size.height.round() as usize,
                        ),
                    };
                    fit_guest_pixels(rw, rh)
                };

                c.active_pixels.set((even_w, even_h));
                r.borrow()
                    .reconfigure_display_ext(even_w, even_h, force_hotplug);
                c.refresh_iso_and_footer();
            })
        };

        let schedule_sync_pulse: Rc<dyn Fn(bool)> = {
            let sync_fn = Rc::clone(&sync_display_resolution);
            let c = Rc::clone(&controls);
            Rc::new(move |use_native_fs: bool| {
                sync_fn(use_native_fs, true);
                if let Some(prev) = c.sync_timer.borrow_mut().take() {
                    prev.invalidate();
                }
                let s = Rc::clone(&sync_fn);
                let blk = RcBlock::new(move |_t: NonNull<NSTimer>| {
                    s(use_native_fs, false);
                });
                let t = unsafe {
                    NSTimer::scheduledTimerWithTimeInterval_repeats_block(0.35, false, &blk)
                };
                *c.sync_timer.borrow_mut() = Some(t);
            })
        };
        *running_sync_hook.borrow_mut() = Some(Rc::clone(&schedule_sync_pulse));

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
                        "This VM configuration does not support saving machine state.",
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
            let sync_pulse = Rc::clone(&schedule_sync_pulse);
            let vm_id_scale = cfg.id.clone();
            targets.bind(mtm, &scale_popup, move |sender| {
                let idx: isize = unsafe { msg_send![sender, indexOfSelectedItem] };
                if idx < 0 {
                    return;
                }
                let scale = DisplayScale::from_index(idx as usize);
                c.display_scale.set(scale);
                // Sync resolution to fullscreen pixels if in fullscreen, or window view bounds if windowed
                sync_pulse(c.in_fullscreen.get());
                if let Ok(st) = VmStore::open_default() {
                    if let Ok(mut saved) = st.load_vm(&vm_id_scale) {
                        saved.display_scale = Some(scale);
                        saved.hidpi_display = scale == DisplayScale::Retina2x;
                        let _ = st.save_vm(&saved);
                    }
                }
            });
        }
        {
            let r = Rc::clone(&runner);
            let c = Rc::clone(&controls);
            let vm_id_awake = cfg.id.clone();
            targets.bind(mtm, &awake_cb, move |sender| {
                let state: isize = unsafe { msg_send![sender, state] };
                let enabled = state == objc2_app_kit::NSControlStateValueOn;
                c.prevent_sleep.set(enabled);
                r.borrow().set_prevent_sleep(enabled);
                c.refresh_iso_and_footer();
                if let Ok(st) = VmStore::open_default() {
                    if let Ok(mut saved) = st.load_vm(&vm_id_awake) {
                        saved.prevent_sleep = enabled;
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

        let c_layout = Rc::clone(&controls);
        let sync_on_resize = Rc::clone(&sync_display_resolution);
        let on_end_resize = Rc::clone(&schedule_sync_pulse);
        let c_screen = Rc::clone(&controls);
        let on_screen_change = Rc::clone(&schedule_sync_pulse);
        let c_will_enter_fs = Rc::clone(&controls);
        let c_did_enter_fs = Rc::clone(&controls);
        let sync_enter_fs = Rc::clone(&schedule_sync_pulse);
        let c_will_exit_fs = Rc::clone(&controls);
        let c_did_exit_fs = Rc::clone(&controls);
        let sync_exit_fs = Rc::clone(&schedule_sync_pulse);

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
                c_layout.relayout();
                // Debounced 200ms display sync so maximizing, zooming, or tiling also updates Linux resolution
                if let Some(prev) = c_layout.resize_debounce_timer.borrow_mut().take() {
                    prev.invalidate();
                }
                let s = Rc::clone(&sync_on_resize);
                let in_fs = c_layout.in_fullscreen.get();
                let blk = RcBlock::new(move |_t: NonNull<NSTimer>| {
                    s(in_fs, false);
                });
                let timer = unsafe {
                    NSTimer::scheduledTimerWithTimeInterval_repeats_block(0.20, false, &blk)
                };
                *c_layout.resize_debounce_timer.borrow_mut() = Some(timer);
            },
            move || {
                on_end_resize(false);
            },
            move || {
                on_screen_change(c_screen.in_fullscreen.get());
            },
            move || {
                c_will_enter_fs.set_fullscreen(true);
            },
            move || {
                c_did_enter_fs.set_fullscreen(true);
                sync_enter_fs(true);
            },
            move || {
                c_will_exit_fs.set_fullscreen(false);
            },
            move || {
                c_did_exit_fs.set_fullscreen(false);
                sync_exit_fs(false);
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
        self.save_state_and_close_with(|_| {});
    }

    pub fn save_state_and_close_with<F>(&self, on_done: F)
    where
        F: Fn(Result<(), String>) + 'static,
    {
        let win = self.window.clone();
        self.runner
            .borrow()
            .save_state_and_stop(&self.saved_state_path, move |res| {
                if res.is_ok() {
                    win.close();
                }
                on_done(res);
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
