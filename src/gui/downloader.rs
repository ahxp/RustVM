use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSBackingStoreType, NSButton, NSColor, NSProgressIndicator,
    NSScrollView, NSTextField, NSWindow, NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::{NSNotification, NSObject, NSObjectProtocol, NSString, NSTimer};
use std::cell::RefCell;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::catalog::{OS_CATALOG, OsPreset, is_preset_downloaded, preset_iso_path};
use crate::config::{VmStore, format_bytes, format_memory_mb};
use crate::download;
use crate::gui::widgets::{
    self, FlippedView, TargetStore, rect, show_error_alert, size,
};

#[derive(Default)]
struct SharedDownloadState {
    active: bool,
    preset_id: String,
    preset_name: String,
    downloaded_bytes: u64,
    total_bytes: Option<u64>,
    bytes_per_sec: f64,
    finished_iso: Option<(String, PathBuf)>,
    error: Option<String>,
}

struct CloseDelegateIvars {
    on_close: RefCell<Box<dyn Fn()>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = CloseDelegateIvars]
    struct SimpleCloseDelegate;

    unsafe impl NSObjectProtocol for SimpleCloseDelegate {}

    unsafe impl NSWindowDelegate for SimpleCloseDelegate {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _notification: &NSNotification) {
            (self.ivars().on_close.borrow())();
        }
    }
);

impl SimpleCloseDelegate {
    fn new<F>(mtm: MainThreadMarker, on_close: F) -> Retained<Self>
    where
        F: Fn() + 'static,
    {
        let this = mtm.alloc::<Self>().set_ivars(CloseDelegateIvars {
            on_close: RefCell::new(Box::new(on_close)),
        });
        unsafe { msg_send![super(this), init] }
    }
}

pub struct DownloaderWindow {
    window: Retained<NSWindow>,
    state: Rc<RefCell<DownloaderInner>>,
    _delegate: Retained<SimpleCloseDelegate>,
}

struct DownloaderInner {
    mtm: MainThreadMarker,
    list_container: Retained<FlippedView>,
    progress_title: Retained<NSTextField>,
    progress_detail: Retained<NSTextField>,
    progress_bar: Retained<NSProgressIndicator>,
    cancel_btn: Retained<NSButton>,
    targets: TargetStore,
    shared: Arc<Mutex<SharedDownloadState>>,
    cancel_flag: Arc<AtomicBool>,
    poll_timer: Option<Retained<NSTimer>>,
    on_create_with_preset: Rc<dyn Fn(&'static OsPreset, PathBuf)>,
}

impl DownloaderWindow {
    pub fn open<F>(
        mtm: MainThreadMarker,
        auto_download_preset_id: Option<&str>,
        on_create_with_preset: F,
    ) -> Rc<Self>
    where
        F: Fn(&'static OsPreset, PathBuf) + 'static,
    {
        let w = 680.0;
        let h = 580.0;
        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable;
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                mtm.alloc(),
                rect(180.0, 140.0, w, h),
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe {
            window.setReleasedWhenClosed(false);
        }
        window.setTitle(&NSString::from_str("Get Linux OS — Official ARM64 Images"));
        window.center();

        let root = FlippedView::new(mtm, rect(0.0, 0.0, w, h));
        root.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
        );

        let title = widgets::bold_label(mtm, rect(20.0, 16.0, w - 40.0, 24.0), "Download an Official ARM64 Linux OS", 17.0);
        let subtitle = widgets::secondary_label(
            mtm,
            rect(20.0, 42.0, w - 40.0, 20.0),
            "All images below are verified UEFI ARM64 installers optimized for Apple Silicon. Cached in ~/RustVM/isos/.",
            12.0,
        );
        root.addSubview(&title);
        root.addSubview(&subtitle);

        let sep1 = widgets::separator(mtm, rect(0.0, 70.0, w, 1.0));
        root.addSubview(&sep1);

        let scroll_h = h - 72.0 - 94.0;
        let scroll = NSScrollView::initWithFrame(mtm.alloc(), rect(0.0, 72.0, w, scroll_h));
        scroll.setHasVerticalScroller(true);
        scroll.setDrawsBackground(false);

        let list_container = FlippedView::new(mtm, rect(0.0, 0.0, w - 16.0, 620.0));
        scroll.setDocumentView(Some(&list_container));
        root.addSubview(&scroll);

        // Bottom download progress bar area
        let sep2 = widgets::separator(mtm, rect(0.0, h - 94.0, w, 1.0));
        root.addSubview(&sep2);

        let progress_title = widgets::semibold_label(
            mtm,
            rect(20.0, h - 82.0, w - 150.0, 20.0),
            "Ready — pick a Linux distribution above to download or create a VM.",
            12.5,
        );
        let progress_bar = widgets::progress_bar(mtm, rect(20.0, h - 56.0, w - 140.0, 16.0));
        let progress_detail = widgets::secondary_label(
            mtm,
            rect(20.0, h - 32.0, w - 150.0, 18.0),
            "Tip: Alpine Linux (~89 MB) downloads in seconds for instant testing.",
            11.5,
        );

        let mut base_targets = TargetStore::new();
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let cancel_flag_btn = Arc::clone(&cancel_flag);
        let cancel_btn = widgets::push_button(
            mtm,
            rect(w - 108.0, h - 64.0, 88.0, 28.0),
            "Cancel",
            &mut base_targets,
            move || {
                cancel_flag_btn.store(true, Ordering::SeqCst);
            },
        );
        cancel_btn.setEnabled(false);

        root.addSubview(&progress_title);
        root.addSubview(&progress_bar);
        root.addSubview(&progress_detail);
        root.addSubview(&cancel_btn);

        window.setContentView(Some(&root));

        let shared = Arc::new(Mutex::new(SharedDownloadState::default()));
        let state = Rc::new(RefCell::new(DownloaderInner {
            mtm,
            list_container,
            progress_title,
            progress_detail,
            progress_bar,
            cancel_btn,
            targets: base_targets,
            shared: Arc::clone(&shared),
            cancel_flag: Arc::clone(&cancel_flag),
            poll_timer: None,
            on_create_with_preset: Rc::new(on_create_with_preset),
        }));

        let cancel_on_close = Arc::clone(&cancel_flag);
        let state_close = Rc::clone(&state);
        let delegate = SimpleCloseDelegate::new(mtm, move || {
            cancel_on_close.store(true, Ordering::SeqCst);
            if let Some(timer) = state_close.borrow_mut().poll_timer.take() {
                timer.invalidate();
            }
        });
        window.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));

        let this = Rc::new(Self {
            window,
            state,
            _delegate: delegate,
        });

        Self::rebuild_cards(&this);
        this.window.makeKeyAndOrderFront(None);

        if let Some(preset_id) = auto_download_preset_id {
            if let Some(preset) = OS_CATALOG.iter().find(|p| p.id == preset_id) {
                Self::start_download(&this, preset);
            }
        }

        this
    }

    pub fn focus(&self) {
        self.window.makeKeyAndOrderFront(None);
    }

    fn rebuild_cards(this: &Rc<Self>) {
        let store = match VmStore::open_default() {
            Ok(s) => s,
            Err(_) => return,
        };

        let mut inner = this.state.borrow_mut();
        let mtm = inner.mtm;

        // Clear existing subviews cleanly
        let subviews = inner.list_container.subviews();
        for i in (0..subviews.count()).rev() {
            subviews.objectAtIndex(i).removeFromSuperview();
        }

        // Preserve cancel button target by recreating targets? Cancel button target stays
        // in `base_targets` — let's keep a separate targets Vec just for cards by rebinding
        // or clearing and re-adding cancel target:
        inner.targets.clear();
        let cf = Arc::clone(&inner.cancel_flag);
        let cbtn = inner.cancel_btn.clone();
        inner.targets.bind_click(mtm, &cbtn, move || {
            cf.store(true, Ordering::SeqCst);
        });

        let is_downloading = inner.shared.lock().map(|s| s.active).unwrap_or(false);
        let active_id = inner
            .shared
            .lock()
            .map(|s| s.preset_id.clone())
            .unwrap_or_default();

        let card_w = 628.0;
        let card_h = 98.0;
        let gap = 12.0;
        let mut y = 14.0;

        for preset in OS_CATALOG {
            let (card_box, card_view) = widgets::card(mtm, rect(20.0, y, card_w, card_h));

            let downloaded = is_preset_downloaded(&store, preset);
            let size_label = if preset.approx_size_mb >= 1024 {
                format!("{:.1} GB", preset.approx_size_mb as f64 / 1024.0)
            } else {
                format!("{} MB", preset.approx_size_mb)
            };

            let name_lbl = widgets::bold_label(mtm, rect(14.0, 12.0, 410.0, 20.0), preset.name, 14.0);
            let tag_lbl = widgets::secondary_label(mtm, rect(14.0, 34.0, 440.0, 18.0), preset.tagline, 12.0);
            let meta = format!(
                "Recommended: {} vCPUs · {} RAM · {} GB Disk · {}",
                preset.default_cpus,
                format_memory_mb(preset.default_ram_mb),
                preset.default_disk_gb,
                if preset.is_gui { "Desktop GUI" } else { "Console / Server" }
            );
            let meta_lbl = widgets::secondary_label(mtm, rect(14.0, 56.0, 440.0, 16.0), &meta, 11.0);
            let hint_lbl = widgets::secondary_label(mtm, rect(14.0, 74.0, 455.0, 16.0), preset.install_hint, 10.5);

            card_view.addSubview(&name_lbl);
            card_view.addSubview(&tag_lbl);
            card_view.addSubview(&meta_lbl);
            card_view.addSubview(&hint_lbl);

            if downloaded {
                let badge = widgets::semibold_label(mtm, rect(card_w - 146.0, 14.0, 132.0, 18.0), "✓ Downloaded", 12.0);
                badge.setTextColor(Some(&NSColor::systemGreenColor()));
                card_view.addSubview(&badge);

                let iso_path = preset_iso_path(&store, preset);
                let cb = Rc::clone(&inner.on_create_with_preset);
                let win = this.window.clone();
                let btn = widgets::push_button(
                    mtm,
                    rect(card_w - 146.0, 42.0, 130.0, 30.0),
                    "Create VM →",
                    &mut inner.targets,
                    move || {
                        win.orderOut(None);
                        (cb)(preset, iso_path.clone());
                    },
                );
                card_view.addSubview(&btn);
            } else {
                let btn_title = if is_downloading && active_id == preset.id {
                    "Downloading…".to_string()
                } else {
                    format!("Download ({size_label})")
                };
                let weak_this = Rc::downgrade(this);
                let btn = widgets::push_button(
                    mtm,
                    rect(card_w - 156.0, 34.0, 140.0, 30.0),
                    &btn_title,
                    &mut inner.targets,
                    move || {
                        if let Some(strong) = weak_this.upgrade() {
                            Self::start_download(&strong, preset);
                        }
                    },
                );
                btn.setEnabled(!is_downloading);
                card_view.addSubview(&btn);
            }

            inner.list_container.addSubview(&card_box);
            y += card_h + gap;
        }

        inner
            .list_container
            .setFrameSize(size(card_w + 36.0, (y + 8.0).max(400.0)));
    }

    fn start_download(this: &Rc<Self>, preset: &'static OsPreset) {
        {
            let inner = this.state.borrow();
            let mut st = inner.shared.lock().unwrap();
            if st.active {
                return;
            }
            st.active = true;
            st.preset_id = preset.id.to_string();
            st.preset_name = preset.name.to_string();
            st.downloaded_bytes = 0;
            st.total_bytes = Some(preset.approx_size_mb * 1024 * 1024);
            st.bytes_per_sec = 0.0;
            st.finished_iso = None;
            st.error = None;
            inner.cancel_flag.store(false, Ordering::SeqCst);
            inner.cancel_btn.setEnabled(true);
            inner.progress_bar.setDoubleValue(0.0);
            inner.progress_title.setStringValue(&NSString::from_str(&format!(
                "Downloading {}…",
                preset.name
            )));
            inner
                .progress_detail
                .setStringValue(&NSString::from_str("Connecting…"));
        }

        Self::rebuild_cards(this);

        // Spawn background download thread
        let shared = Arc::clone(&this.state.borrow().shared);
        let cancel = Arc::clone(&this.state.borrow().cancel_flag);
        thread::spawn(move || {
            let store = match VmStore::open_default() {
                Ok(s) => s,
                Err(e) => {
                    let mut st = shared.lock().unwrap();
                    st.active = false;
                    st.error = Some(e.to_string());
                    return;
                }
            };
            let shared_prog = Arc::clone(&shared);
            let res = download::download_preset(&store, preset, cancel, move |prog| {
                if let Ok(mut st) = shared_prog.lock() {
                    st.downloaded_bytes = prog.downloaded_bytes;
                    if prog.total_bytes.is_some() {
                        st.total_bytes = prog.total_bytes;
                    }
                    st.bytes_per_sec = prog.bytes_per_sec;
                }
            });
            let mut st = shared.lock().unwrap();
            st.active = false;
            match res {
                Ok(path) => {
                    st.finished_iso = Some((preset.id.to_string(), path));
                }
                Err(e) => {
                    st.error = Some(e.to_string());
                }
            }
        });

        // Start 100ms main-thread poll timer
        let weak_this = Rc::downgrade(this);
        let block = RcBlock::new(move |timer: NonNull<NSTimer>| {
            let Some(strong) = weak_this.upgrade() else {
                unsafe { timer.as_ref().invalidate() };
                return;
            };
            let (active, name, dl, total, bps, finished, err) = {
                let inner = strong.state.borrow();
                let mut st = inner.shared.lock().unwrap();
                (
                    st.active,
                    st.preset_name.clone(),
                    st.downloaded_bytes,
                    st.total_bytes,
                    st.bytes_per_sec,
                    st.finished_iso.take(),
                    st.error.take(),
                )
            };

            if active {
                let inner = strong.state.borrow();
                if let Some(tot) = total.filter(|&t| t > 0) {
                    let pct = (dl as f64 / tot as f64 * 100.0).clamp(0.0, 100.0);
                    inner.progress_bar.setDoubleValue(pct);
                    inner.progress_detail.setStringValue(&NSString::from_str(&format!(
                        "{} / {} ({:.1}%) — {}/s",
                        format_bytes(dl),
                        format_bytes(tot),
                        pct,
                        format_bytes(bps as u64)
                    )));
                } else {
                    inner.progress_detail.setStringValue(&NSString::from_str(&format!(
                        "{} downloaded — {}/s",
                        format_bytes(dl),
                        format_bytes(bps as u64)
                    )));
                }
            } else {
                unsafe { timer.as_ref().invalidate() };
                {
                    let mut inner = strong.state.borrow_mut();
                    inner.poll_timer = None;
                    inner.cancel_btn.setEnabled(false);
                    if let Some((_id, path)) = finished {
                        inner.progress_bar.setDoubleValue(100.0);
                        inner.progress_title.setStringValue(&NSString::from_str(&format!(
                            "Downloaded {}!",
                            name
                        )));
                        inner.progress_detail.setStringValue(&NSString::from_str(&format!(
                            "Saved to {} — click 'Create VM →' above to install & boot.",
                            path.display()
                        )));
                    } else if let Some(msg) = err {
                        inner.progress_bar.setDoubleValue(0.0);
                        inner
                            .progress_title
                            .setStringValue(&NSString::from_str("Download stopped"));
                        inner
                            .progress_detail
                            .setStringValue(&NSString::from_str(&msg));
                        if !msg.contains("cancelled") {
                            show_error_alert(inner.mtm, "Download Failed", &msg);
                        }
                    }
                }
                Self::rebuild_cards(&strong);
            }
        });

        let timer = unsafe {
            NSTimer::scheduledTimerWithTimeInterval_repeats_block(0.1, true, &block)
        };
        this.state.borrow_mut().poll_timer = Some(timer);
    }
}
