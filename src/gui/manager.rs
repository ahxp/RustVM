use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSBackingStoreType, NSColor, NSScrollView, NSTextField, NSWindow,
    NSWindowStyleMask,
};
use objc2_foundation::NSString;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use crate::catalog::{OS_CATALOG, OsPreset, find_preset, is_preset_downloaded, preset_iso_path};
use crate::config::{HostSpecs, VmConfig, VmStore, format_bytes, format_memory_mb};
use crate::disk;
use crate::gui::display::VmDisplaySession;
use crate::gui::downloader::DownloaderWindow;
use crate::gui::widgets::{
    self, FlippedView, TargetStore, pick_directory, pick_iso_file, rect, show_confirm_alert,
    show_error_alert, size,
};
use crate::gui::wizard::{VmWizardWindow, WizardMode};
use crate::vm::{VmState, is_virtualization_supported};

pub struct ManagerApp {
    mtm: MainThreadMarker,
    window: Retained<NSWindow>,
    header_targets: RefCell<TargetStore>,
    sidebar_container: Retained<FlippedView>,
    sidebar_count_label: Retained<NSTextField>,
    sidebar_targets: RefCell<TargetStore>,
    detail_container: Retained<FlippedView>,
    detail_targets: RefCell<TargetStore>,
    vms: RefCell<Vec<VmConfig>>,
    selected_vm_id: RefCell<Option<String>>,
    sessions: RefCell<HashMap<String, Rc<VmDisplaySession>>>,
    downloader: RefCell<Option<Rc<DownloaderWindow>>>,
    wizard: RefCell<Option<VmWizardWindow>>,
}

impl ManagerApp {
    pub fn launch(mtm: MainThreadMarker, auto_start_vm_id: Option<String>) -> Rc<Self> {
        let w = 960.0;
        let h = 640.0;
        let sidebar_w = 250.0;
        let header_h = 58.0;

        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable;
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                mtm.alloc(),
                rect(100.0, 90.0, w, h),
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe {
            window.setReleasedWhenClosed(false);
        }
        window.setMinSize(size(880.0, 560.0));
        window.setTitle(&NSString::from_str("RustVM — Virtual Machine Manager"));
        window.center();

        let root = FlippedView::new(mtm, rect(0.0, 0.0, w, h));
        root.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
        );

        // Header bar
        let header = FlippedView::new(mtm, rect(0.0, 0.0, w, header_h));
        header.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMaxYMargin,
        );
        let brand = widgets::bold_label(mtm, rect(20.0, 10.0, 260.0, 22.0), "RustVM Manager", 18.0);
        let host = HostSpecs::detect();
        let sub = widgets::secondary_label(
            mtm,
            rect(20.0, 34.0, 440.0, 16.0),
            &format!(
                "Native Apple Silicon Linux Virtualization · Host: {} CPUs, {}",
                host.cpu_count,
                format_memory_mb(host.total_memory_mb)
            ),
            11.5,
        );
        header.addSubview(&brand);
        header.addSubview(&sub);
        root.addSubview(&header);

        let header_sep = widgets::separator(mtm, rect(0.0, header_h - 1.0, w, 1.0));
        header_sep.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMaxYMargin,
        );
        root.addSubview(&header_sep);

        // Sidebar pane
        let sidebar_pane = FlippedView::new(mtm, rect(0.0, header_h, sidebar_w, h - header_h));
        sidebar_pane.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewHeightSizable | NSAutoresizingMaskOptions::ViewMaxXMargin,
        );
        let sidebar_count_label = widgets::semibold_label(
            mtm,
            rect(16.0, 12.0, sidebar_w - 24.0, 18.0),
            "VIRTUAL MACHINES",
            11.0,
        );
        sidebar_count_label.setTextColor(Some(&NSColor::secondaryLabelColor()));
        sidebar_pane.addSubview(&sidebar_count_label);

        let sidebar_scroll = NSScrollView::initWithFrame(
            mtm.alloc(),
            rect(0.0, 34.0, sidebar_w, h - header_h - 34.0),
        );
        sidebar_scroll.setHasVerticalScroller(true);
        sidebar_scroll.setDrawsBackground(false);
        sidebar_scroll.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        let sidebar_container = FlippedView::new(mtm, rect(0.0, 0.0, sidebar_w - 8.0, 400.0));
        sidebar_scroll.setDocumentView(Some(&sidebar_container));
        sidebar_pane.addSubview(&sidebar_scroll);
        root.addSubview(&sidebar_pane);

        // Vertical divider
        let v_sep = widgets::separator(mtm, rect(sidebar_w, header_h, 1.0, h - header_h));
        v_sep.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewHeightSizable | NSAutoresizingMaskOptions::ViewMaxXMargin,
        );
        root.addSubview(&v_sep);

        // Detail pane (scrollable right panel)
        let detail_w = w - sidebar_w - 1.0;
        let detail_scroll = NSScrollView::initWithFrame(
            mtm.alloc(),
            rect(sidebar_w + 1.0, header_h, detail_w, h - header_h),
        );
        detail_scroll.setHasVerticalScroller(true);
        detail_scroll.setDrawsBackground(false);
        detail_scroll.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        let detail_container = FlippedView::new(mtm, rect(0.0, 0.0, detail_w - 16.0, 620.0));
        detail_container.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        detail_scroll.setDocumentView(Some(&detail_container));
        root.addSubview(&detail_scroll);

        window.setContentView(Some(&root));

        let app = Rc::new(Self {
            mtm,
            window: window.clone(),
            header_targets: RefCell::new(TargetStore::new()),
            sidebar_container,
            sidebar_count_label,
            sidebar_targets: RefCell::new(TargetStore::new()),
            detail_container,
            detail_targets: RefCell::new(TargetStore::new()),
            vms: RefCell::new(Vec::new()),
            selected_vm_id: RefCell::new(None),
            sessions: RefCell::new(HashMap::new()),
            downloader: RefCell::new(None),
            wizard: RefCell::new(None),
        });

        // Header action buttons (right-aligned)
        {
            let mut ht = app.header_targets.borrow_mut();

            let app_dl = Rc::downgrade(&app);
            let get_os_btn = widgets::push_button(
                mtm,
                rect(w - 256.0, 14.0, 128.0, 30.0),
                "↓ Get Linux OS",
                &mut ht,
                move || {
                    if let Some(a) = app_dl.upgrade() {
                        a.open_downloader(None);
                    }
                },
            );
            get_os_btn.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin);
            header.addSubview(&get_os_btn);

            let app_new = Rc::downgrade(&app);
            let new_btn = widgets::primary_button(
                mtm,
                rect(w - 118.0, 14.0, 98.0, 30.0),
                "+ New VM",
                &mut ht,
                move || {
                    if let Some(a) = app_new.upgrade() {
                        a.open_create_wizard(None, None);
                    }
                },
            );
            new_btn.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin);
            header.addSubview(&new_btn);
        }

        app.reload_vms(auto_start_vm_id.clone());
        window.makeKeyAndOrderFront(None);

        if !is_virtualization_supported() {
            show_error_alert(
                mtm,
                "Virtualization Not Supported",
                "Apple Virtualization.framework is not supported on this Mac.",
            );
        } else if let Some(vm_id) = auto_start_vm_id {
            app.start_or_focus_vm(&vm_id);
        }

        app
    }

    pub fn focus(&self) {
        self.window.makeKeyAndOrderFront(None);
    }

    pub fn open_create_wizard(
        self: &Rc<Self>,
        prefill_preset: Option<&'static OsPreset>,
        prefill_iso: Option<PathBuf>,
    ) {
        let weak = Rc::downgrade(self);
        let weak_dl = Rc::downgrade(self);
        let wiz = VmWizardWindow::open(
            self.mtm,
            WizardMode::Create {
                prefill_preset,
                prefill_iso,
            },
            move |result| {
                if let Some(app) = weak.upgrade() {
                    let vm_id = result.vm.id.clone();
                    app.reload_vms(Some(vm_id.clone()));
                    if result.start_immediately {
                        app.start_or_focus_vm(&vm_id);
                    }
                }
            },
            move |preset_id| {
                if let Some(app) = weak_dl.upgrade() {
                    app.open_downloader(preset_id.as_deref());
                }
            },
        );
        wiz.focus();
        *self.wizard.borrow_mut() = Some(wiz);
    }

    pub fn open_edit_wizard(self: &Rc<Self>, cfg: VmConfig) {
        let weak = Rc::downgrade(self);
        let weak_dl = Rc::downgrade(self);
        let wiz = VmWizardWindow::open(
            self.mtm,
            WizardMode::Edit(cfg),
            move |result| {
                if let Some(app) = weak.upgrade() {
                    app.reload_vms(Some(result.vm.id));
                }
            },
            move |preset_id| {
                if let Some(app) = weak_dl.upgrade() {
                    app.open_downloader(preset_id.as_deref());
                }
            },
        );
        wiz.focus();
        *self.wizard.borrow_mut() = Some(wiz);
    }

    pub fn open_downloader(self: &Rc<Self>, auto_preset_id: Option<&str>) {
        let weak = Rc::downgrade(self);
        let dl = DownloaderWindow::open(self.mtm, auto_preset_id, move |preset, iso_path| {
            if let Some(app) = weak.upgrade() {
                app.open_create_wizard(Some(preset), Some(iso_path));
            }
        });
        dl.focus();
        *self.downloader.borrow_mut() = Some(dl);
    }

    pub fn reload_vms(self: &Rc<Self>, select_id: Option<String>) {
        let store = match VmStore::open_default() {
            Ok(s) => s,
            Err(e) => {
                show_error_alert(self.mtm, "Storage Error", &e.to_string());
                return;
            }
        };
        let list = store.list_vms().unwrap_or_default();
        let next_sel = if let Some(req) = select_id {
            if list.iter().any(|v| v.id == req) {
                Some(req)
            } else {
                list.first().map(|v| v.id.clone())
            }
        } else {
            let cur = self.selected_vm_id.borrow().clone();
            match cur {
                Some(id) if list.iter().any(|v| v.id == id) => Some(id),
                _ => list.first().map(|v| v.id.clone()),
            }
        };

        *self.vms.borrow_mut() = list;
        *self.selected_vm_id.borrow_mut() = next_sel;
        self.render_sidebar();
        self.render_detail();
    }

    pub fn stop_all_sessions(&self) {
        let active: Vec<Rc<VmDisplaySession>> = self.sessions.borrow().values().cloned().collect();
        for sess in active {
            sess.force_stop();
        }
        self.sessions.borrow_mut().clear();
    }

    fn vm_state(&self, vm_id: &str) -> VmState {
        self.sessions
            .borrow()
            .get(vm_id)
            .map(|s| s.state())
            .unwrap_or(VmState::Stopped)
    }

    pub fn start_or_focus_vm(self: &Rc<Self>, vm_id: &str) {
        if let Some(existing) = self.sessions.borrow().get(vm_id).cloned() {
            if existing.state().is_active() {
                existing.focus();
                return;
            }
        }
        let store = match VmStore::open_default() {
            Ok(s) => s,
            Err(e) => {
                show_error_alert(self.mtm, "Storage Error", &e.to_string());
                return;
            }
        };
        let cfg = match store.load_vm(vm_id) {
            Ok(c) => c,
            Err(e) => {
                show_error_alert(self.mtm, "VM Not Found", &e.to_string());
                return;
            }
        };

        let weak_state = Rc::downgrade(self);
        let weak_close = Rc::downgrade(self);
        let id_close = cfg.id.clone();

        match VmDisplaySession::launch(
            self.mtm,
            &store,
            &cfg,
            move |_state| {
                if let Some(app) = weak_state.upgrade() {
                    app.render_sidebar();
                    app.render_detail();
                }
            },
            move || {
                if let Some(app) = weak_close.upgrade() {
                    app.sessions.borrow_mut().remove(&id_close);
                    app.reload_vms(None);
                }
            },
        ) {
            Ok(session) => {
                self.sessions
                    .borrow_mut()
                    .insert(cfg.id.clone(), Rc::new(session));
                self.render_sidebar();
                self.render_detail();
            }
            Err(e) => {
                show_error_alert(self.mtm, "Failed to Start Virtual Machine", &e.to_string());
            }
        }
    }

    fn render_sidebar(self: &Rc<Self>) {
        let mtm = self.mtm;
        clear_subviews(&self.sidebar_container);
        self.sidebar_targets.borrow_mut().clear();

        let vms = self.vms.borrow().clone();
        let selected = self.selected_vm_id.borrow().clone();

        self.sidebar_count_label
            .setStringValue(&NSString::from_str(&format!(
                "VIRTUAL MACHINES ({})",
                vms.len()
            )));

        if vms.is_empty() {
            let empty = widgets::secondary_label(
                mtm,
                rect(16.0, 12.0, 214.0, 50.0),
                "No VMs yet.\nClick '+ New VM' or '↓ Get Linux OS' to start.",
                12.0,
            );
            self.sidebar_container.addSubview(&empty);
            self.sidebar_container.setFrameSize(size(240.0, 120.0));
            return;
        }

        let mut targets = self.sidebar_targets.borrow_mut();
        let mut y = 6.0;
        let row_h = 56.0;
        let row_w = 224.0;

        for vm in vms {
            let is_sel = selected.as_deref() == Some(vm.id.as_str());
            let state = self.vm_state(&vm.id);

            let (fill, border) = if is_sel {
                (
                    NSColor::systemBlueColor().colorWithAlphaComponent(0.16),
                    NSColor::systemBlueColor().colorWithAlphaComponent(0.75),
                )
            } else {
                (NSColor::controlBackgroundColor(), NSColor::separatorColor())
            };
            let (card_box, card_view) = widgets::tint_card(mtm, rect(12.0, y, row_w, row_h), &fill, &border, 8.0);

            let dot_color = state_color(state);
            let dot = widgets::status_dot(mtm, 12.0, 15.0, &dot_color);
            card_view.addSubview(&dot);

            let title = widgets::semibold_label(mtm, rect(28.0, 10.0, row_w - 38.0, 18.0), &vm.name, 13.0);
            let sub_text = format!(
                "{} vCPU · {} · {}",
                vm.cpus,
                format_memory_mb(vm.memory_mb),
                state.label()
            );
            let sub = widgets::secondary_label(mtm, rect(28.0, 30.0, row_w - 38.0, 16.0), &sub_text, 11.0);
            card_view.addSubview(&title);
            card_view.addSubview(&sub);

            // Transparent full-card button to handle clicks cleanly
            let btn = objc2_app_kit::NSButton::initWithFrame(mtm.alloc(), rect(0.0, 0.0, row_w, row_h));
            btn.setTitle(&NSString::from_str(""));
            btn.setBordered(false);
            btn.setTransparent(true);
            let weak = Rc::downgrade(self);
            let id_click = vm.id.clone();
            targets.bind_click(mtm, &btn, move || {
                if let Some(app) = weak.upgrade() {
                    *app.selected_vm_id.borrow_mut() = Some(id_click.clone());
                    app.render_sidebar();
                    app.render_detail();
                }
            });
            card_view.addSubview(&btn);

            self.sidebar_container.addSubview(&card_box);
            y += row_h + 8.0;
        }

        self.sidebar_container
            .setFrameSize(size(240.0, (y + 12.0).max(300.0)));
    }

    fn render_detail(self: &Rc<Self>) {
        let mtm = self.mtm;
        clear_subviews(&self.detail_container);
        self.detail_targets.borrow_mut().clear();

        let selected_id = self.selected_vm_id.borrow().clone();
        let selected_vm = selected_id.as_deref().and_then(|id| {
            self.vms
                .borrow()
                .iter()
                .find(|v| v.id == id)
                .cloned()
        });

        let Some(vm) = selected_vm else {
            self.render_welcome_dashboard();
            return;
        };

        let store = match VmStore::open_default() {
            Ok(s) => s,
            Err(_) => return,
        };
        let state = self.vm_state(&vm.id);
        let is_active = state.is_active();
        let disk_path = store.disk_path(&vm.id);
        let usage = disk::disk_usage(&disk_path);

        let content_w = 654.0;
        let pad_x = 24.0;
        let mut y = 20.0;

        // 1. VM Title & Status Header
        let name_lbl = widgets::bold_label(mtm, rect(pad_x, y, 440.0, 28.0), &vm.name, 22.0);
        self.detail_container.addSubview(&name_lbl);

        let dot = widgets::status_dot(mtm, pad_x + content_w - 110.0, y + 9.0, &state_color(state));
        let st_lbl = widgets::semibold_label(
            mtm,
            rect(pad_x + content_w - 94.0, y + 5.0, 96.0, 18.0),
            state.label(),
            13.0,
        );
        self.detail_container.addSubview(&dot);
        self.detail_container.addSubview(&st_lbl);

        let preset = vm.os_preset.as_deref().and_then(find_preset);
        let os_subtitle = preset
            .map(|p| format!("{} · ARM64 UEFI", p.name))
            .unwrap_or_else(|| "Custom ARM64 Linux · UEFI".to_string());
        let sub_lbl = widgets::secondary_label(mtm, rect(pad_x, y + 30.0, 500.0, 18.0), &os_subtitle, 12.5);
        self.detail_container.addSubview(&sub_lbl);
        y += 58.0;

        // 2. Action Toolbar Buttons
        let mut targets = self.detail_targets.borrow_mut();

        {
            let weak = Rc::downgrade(self);
            let id = vm.id.clone();
            let title = if is_active { "Show Display" } else { "▶ Start" };
            let btn = widgets::primary_button(mtm, rect(pad_x, y, 114.0, 32.0), title, &mut targets, move || {
                if let Some(app) = weak.upgrade() {
                    app.start_or_focus_vm(&id);
                }
            });
            self.detail_container.addSubview(&btn);
        }
        {
            let weak = Rc::downgrade(self);
            let id = vm.id.clone();
            let title = if state == VmState::Paused { "▶ Resume" } else { "⏸ Pause" };
            let btn = widgets::push_button(mtm, rect(pad_x + 122.0, y, 94.0, 32.0), title, &mut targets, move || {
                if let Some(app) = weak.upgrade() {
                    if let Some(sess) = app.sessions.borrow().get(&id).cloned() {
                        sess.pause_or_resume();
                    }
                }
            });
            btn.setEnabled(matches!(state, VmState::Running | VmState::Paused));
            self.detail_container.addSubview(&btn);
        }
        {
            let weak = Rc::downgrade(self);
            let id = vm.id.clone();
            let btn = widgets::push_button(mtm, rect(pad_x + 224.0, y, 102.0, 32.0), "⏻ Shutdown", &mut targets, move || {
                if let Some(app) = weak.upgrade() {
                    if let Some(sess) = app.sessions.borrow().get(&id).cloned() {
                        sess.request_stop();
                    }
                }
            });
            btn.setEnabled(state == VmState::Running);
            self.detail_container.addSubview(&btn);
        }
        {
            let weak = Rc::downgrade(self);
            let id = vm.id.clone();
            let btn = widgets::push_button(mtm, rect(pad_x + 334.0, y, 96.0, 32.0), "■ Stop", &mut targets, move || {
                if let Some(app) = weak.upgrade() {
                    if let Some(sess) = app.sessions.borrow().get(&id).cloned() {
                        sess.force_stop();
                    }
                }
            });
            btn.setEnabled(is_active);
            self.detail_container.addSubview(&btn);
        }
        {
            let weak = Rc::downgrade(self);
            let cfg_edit = vm.clone();
            let btn = widgets::push_button(mtm, rect(pad_x + 448.0, y, 96.0, 32.0), "⚙ Settings", &mut targets, move || {
                if let Some(app) = weak.upgrade() {
                    app.open_edit_wizard(cfg_edit.clone());
                }
            });
            btn.setEnabled(!is_active);
            self.detail_container.addSubview(&btn);
        }
        {
            let weak = Rc::downgrade(self);
            let id = vm.id.clone();
            let name = vm.name.clone();
            let btn = widgets::push_button(mtm, rect(pad_x + 552.0, y, 94.0, 32.0), "🗑 Delete", &mut targets, move || {
                if let Some(app) = weak.upgrade() {
                    let ok = show_confirm_alert(
                        mtm,
                        &format!("Delete '{name}'?"),
                        "This permanently removes the virtual machine, its EFI NVRAM, and its virtual hard disk image. Cached ISOs in ~/RustVM/isos/ are kept.",
                        "Delete VM",
                        true,
                    );
                    if ok {
                        if let Ok(st) = VmStore::open_default() {
                            if let Err(e) = st.delete_vm(&id) {
                                show_error_alert(mtm, "Delete Failed", &e.to_string());
                            } else {
                                app.reload_vms(None);
                            }
                        }
                    }
                }
            });
            btn.setEnabled(!is_active);
            self.detail_container.addSubview(&btn);
        }

        y += 48.0;

        // 3. Hardware & Devices Card
        let (hw_box, hw_view) = widgets::card(mtm, rect(pad_x, y, content_w, 126.0));
        hw_view.addSubview(&widgets::semibold_label(mtm, rect(16.0, 12.0, 260.0, 18.0), "HARDWARE & PERFORMANCE", 11.5));
        hw_view.addSubview(&widgets::separator(mtm, rect(16.0, 34.0, content_w - 32.0, 1.0)));

        add_kv_row(mtm, &hw_view, 16.0, 44.0, "Processors", &format!("{} vCPUs (Apple Silicon Native)", vm.cpus));
        add_kv_row(mtm, &hw_view, 336.0, 44.0, "Memory", &format_memory_mb(vm.memory_mb));
        add_kv_row(
            mtm,
            &hw_view,
            16.0,
            70.0,
            "Display",
            &format!(
                "{}×{} ({})",
                vm.display_width,
                vm.display_height,
                if vm.hidpi_display { "HiDPI 2×" } else { "Smooth 1×" }
            ),
        );
        add_kv_row(
            mtm,
            &hw_view,
            336.0,
            70.0,
            "Disk I/O",
            if vm.fast_disk_io {
                "Fast (Uncached + Fsync)"
            } else {
                "Strict (Full Sync)"
            },
        );
        add_kv_row(
            mtm,
            &hw_view,
            16.0,
            96.0,
            "Audio / Clip",
            &format!(
                "Audio {} · Clipboard {}",
                if vm.enable_audio { "On" } else { "Off" },
                if vm.enable_clipboard { "On" } else { "Off" }
            ),
        );
        add_kv_row(
            mtm,
            &hw_view,
            336.0,
            96.0,
            "Network",
            &format!("NAT ({})", vm.mac_address),
        );
        self.detail_container.addSubview(&hw_box);
        y += 140.0;

        // 4. Storage & Installer ISO Card
        let (st_box, st_view) = widgets::card(mtm, rect(pad_x, y, content_w, 134.0));
        st_view.addSubview(&widgets::semibold_label(mtm, rect(16.0, 12.0, 320.0, 18.0), "STORAGE & INSTALLER ISO", 11.5));
        st_view.addSubview(&widgets::separator(mtm, rect(16.0, 34.0, content_w - 32.0, 1.0)));

        add_kv_row(
            mtm,
            &st_view,
            16.0,
            44.0,
            "Virtual Disk",
            &format!(
                "{} GB sparse ({} used on host)",
                vm.disk_gb,
                format_bytes(usage.actual_bytes)
            ),
        );
        let disk_path_lbl = widgets::selectable_mono_label(
            mtm,
            rect(112.0, 64.0, content_w - 130.0, 16.0),
            &disk_path.display().to_string(),
            10.5,
        );
        st_view.addSubview(&disk_path_lbl);

        let iso_text = vm
            .iso_path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "Empty (Boots from Virtual Disk)".to_string());
        add_kv_row(mtm, &st_view, 16.0, 92.0, "Installer ISO", &iso_text);

        {
            let weak = Rc::downgrade(self);
            let id = vm.id.clone();
            let attach_btn = widgets::push_button(
                mtm,
                rect(content_w - 208.0, 88.0, 102.0, 28.0),
                "Attach ISO…",
                &mut targets,
                move || {
                    if let Some(app) = weak.upgrade() {
                        if let Some(path) = pick_iso_file(mtm) {
                            if let Some(sess) = app.sessions.borrow().get(&id).cloned() {
                                if sess.state().is_active() {
                                    if let Err(e) = sess.hot_attach_iso(&path) {
                                        show_error_alert(mtm, "Attach ISO Failed", &e.to_string());
                                    }
                                    return;
                                }
                            }
                            if let Ok(st) = VmStore::open_default() {
                                if let Ok(mut saved) = st.load_vm(&id) {
                                    saved.iso_path = Some(path);
                                    let _ = st.save_vm(&saved);
                                    app.reload_vms(Some(id.clone()));
                                }
                            }
                        }
                    }
                },
            );
            st_view.addSubview(&attach_btn);
        }
        {
            let weak = Rc::downgrade(self);
            let id = vm.id.clone();
            let has_iso = vm.iso_path.is_some();
            let eject_btn = widgets::push_button(
                mtm,
                rect(content_w - 98.0, 88.0, 82.0, 28.0),
                "Eject ISO",
                &mut targets,
                move || {
                    if let Some(app) = weak.upgrade() {
                        if let Some(sess) = app.sessions.borrow().get(&id).cloned() {
                            if sess.state().is_active() {
                                sess.hot_eject_iso();
                                return;
                            }
                        }
                        if let Ok(st) = VmStore::open_default() {
                            if let Ok(mut saved) = st.load_vm(&id) {
                                saved.iso_path = None;
                                let _ = st.save_vm(&saved);
                                app.reload_vms(Some(id.clone()));
                            }
                        }
                    }
                },
            );
            eject_btn.setEnabled(has_iso);
            st_view.addSubview(&eject_btn);
        }

        self.detail_container.addSubview(&st_box);
        y += 148.0;

        // 5. Shared Folder (VirtioFS) Card
        let (sh_box, sh_view) = widgets::card(mtm, rect(pad_x, y, content_w, 102.0));
        sh_view.addSubview(&widgets::semibold_label(mtm, rect(16.0, 12.0, 320.0, 18.0), "SHARED HOST FOLDER (VIRTIOFS)", 11.5));
        sh_view.addSubview(&widgets::separator(mtm, rect(16.0, 34.0, content_w - 32.0, 1.0)));

        let share_text = vm
            .shared_folder
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "Not configured".to_string());
        add_kv_row(mtm, &sh_view, 16.0, 44.0, "Host Folder", &share_text);

        {
            let weak = Rc::downgrade(self);
            let id = vm.id.clone();
            let choose_btn = widgets::push_button(
                mtm,
                rect(content_w - 208.0, 40.0, 108.0, 28.0),
                "Choose…",
                &mut targets,
                move || {
                    if let Some(app) = weak.upgrade() {
                        if let Some(dir) = pick_directory(mtm) {
                            if let Ok(st) = VmStore::open_default() {
                                if let Ok(mut saved) = st.load_vm(&id) {
                                    saved.shared_folder = Some(dir);
                                    let _ = st.save_vm(&saved);
                                    app.reload_vms(Some(id.clone()));
                                }
                            }
                        }
                    }
                },
            );
            choose_btn.setEnabled(!is_active);
            sh_view.addSubview(&choose_btn);
        }
        {
            let weak = Rc::downgrade(self);
            let id = vm.id.clone();
            let remove_btn = widgets::push_button(
                mtm,
                rect(content_w - 92.0, 40.0, 76.0, 28.0),
                "Remove",
                &mut targets,
                move || {
                    if let Some(app) = weak.upgrade() {
                        if let Ok(st) = VmStore::open_default() {
                            if let Ok(mut saved) = st.load_vm(&id) {
                                saved.shared_folder = None;
                                let _ = st.save_vm(&saved);
                                app.reload_vms(Some(id.clone()));
                            }
                        }
                    }
                },
            );
            remove_btn.setEnabled(!is_active && vm.shared_folder.is_some());
            sh_view.addSubview(&remove_btn);
        }

        let mount_cmd = widgets::selectable_mono_label(
            mtm,
            rect(16.0, 74.0, content_w - 32.0, 18.0),
            "Mount in Linux: sudo mkdir -p /mnt/shared && sudo mount -t virtiofs shared /mnt/shared",
            11.0,
        );
        sh_view.addSubview(&mount_cmd);
        self.detail_container.addSubview(&sh_box);
        y += 116.0;

        // 6. Smoothness & Guest Optimization Tip Banner
        let tip_fill = NSColor::systemBlueColor().colorWithAlphaComponent(0.08);
        let tip_border = NSColor::systemBlueColor().colorWithAlphaComponent(0.30);
        let (tip_box, tip_view) = widgets::tint_card(mtm, rect(pad_x, y, content_w, 86.0), &tip_fill, &tip_border, 8.0);
        let tip_title = widgets::semibold_label(mtm, rect(14.0, 10.0, content_w - 220.0, 16.0), "Smooth Desktop & Install Tip", 12.0);
        let tip_body_text = preset
            .map(|p| p.install_hint)
            .unwrap_or("Attach an ARM64 UEFI Linux .iso, click Start to install onto the virtual disk, then click Eject ISO and reboot.");
        let tip_body = widgets::secondary_label(mtm, rect(14.0, 28.0, content_w - 28.0, 28.0), tip_body_text, 11.0);
        let speed_cmd_preview = widgets::selectable_mono_label(
            mtm,
            rect(14.0, 58.0, content_w - 210.0, 18.0),
            "Ubuntu/GNOME speed-up: gsettings set org.gnome.desktop.interface enable-animations false",
            10.5,
        );
        let copy_btn = widgets::push_button(
            mtm,
            rect(content_w - 190.0, 52.0, 176.0, 26.0),
            "⚡ Copy Speed-Up Cmd",
            &mut targets,
            || {},
        );
        {
            let btn_ref = copy_btn.clone();
            targets.bind_click(mtm, &copy_btn, move || {
                widgets::copy_to_clipboard(
                    "gsettings set org.gnome.desktop.interface enable-animations false && sudo apt update && sudo apt install -y spice-vdagent",
                );
                btn_ref.setTitle(&NSString::from_str("✓ Copied! Paste in Linux"));
            });
        }
        tip_view.addSubview(&tip_title);
        tip_view.addSubview(&tip_body);
        tip_view.addSubview(&speed_cmd_preview);
        tip_view.addSubview(&copy_btn);
        self.detail_container.addSubview(&tip_box);
        y += 102.0;

        self.detail_container
            .setFrameSize(size(content_w + 48.0, y.max(540.0)));
    }

    fn render_welcome_dashboard(self: &Rc<Self>) {
        let mtm = self.mtm;
        let pad_x = 34.0;
        let w = 630.0;

        let title = widgets::bold_label(mtm, rect(pad_x, 34.0, w, 30.0), "Welcome to RustVM", 24.0);
        let subtitle = widgets::secondary_label(
            mtm,
            rect(pad_x, 68.0, w, 38.0),
            "Run Ubuntu, Debian, Fedora, Alpine, or any ARM64 Linux OS natively at full Apple Silicon speed — no QEMU or VirtualBox required.",
            13.5,
        );
        self.detail_container.addSubview(&title);
        self.detail_container.addSubview(&subtitle);

        // 3-step guide card
        let (guide_box, guide_view) = widgets::card(mtm, rect(pad_x, 120.0, w, 146.0));
        guide_view.addSubview(&widgets::semibold_label(mtm, rect(18.0, 14.0, 300.0, 18.0), "3 EASY STEPS TO RUN LINUX", 11.5));
        guide_view.addSubview(&widgets::separator(mtm, rect(18.0, 36.0, w - 36.0, 1.0)));
        guide_view.addSubview(&widgets::label(
            mtm,
            rect(18.0, 48.0, w - 36.0, 20.0),
            "1. Download an official ARM64 Linux ISO (or pick one you already have).",
            13.0,
        ));
        guide_view.addSubview(&widgets::label(
            mtm,
            rect(18.0, 76.0, w - 36.0, 20.0),
            "2. Click '+ New VM' — choose CPUs, RAM, and sparse disk size (takes 0 space until used).",
            13.0,
        ));
        guide_view.addSubview(&widgets::label(
            mtm,
            rect(18.0, 104.0, w - 36.0, 20.0),
            "3. Click '▶ Start' to open the Linux display window, install to disk, then click 'Eject ISO'!",
            13.0,
        ));
        self.detail_container.addSubview(&guide_box);

        // Quick Action Cards
        let mut targets = self.detail_targets.borrow_mut();

        let (cta_box, cta_view) = widgets::card(mtm, rect(pad_x, 286.0, w, 152.0));
        cta_view.addSubview(&widgets::bold_label(mtm, rect(18.0, 16.0, 400.0, 20.0), "Get Started Immediately", 15.0));
        cta_view.addSubview(&widgets::secondary_label(
            mtm,
            rect(18.0, 40.0, w - 36.0, 20.0),
            "Want to try Linux right now? Alpine Linux is only ~89 MB and boots in a few seconds.",
            12.5,
        ));

        let weak_alpine = Rc::downgrade(self);
        let alpine_btn = widgets::primary_button(
            mtm,
            rect(18.0, 74.0, 236.0, 34.0),
            "⚡ Quick Start Alpine Linux",
            &mut targets,
            move || {
                if let Some(app) = weak_alpine.upgrade() {
                    let alpine = &OS_CATALOG[0];
                    if let Ok(st) = VmStore::open_default() {
                        if is_preset_downloaded(&st, alpine) {
                            app.open_create_wizard(Some(alpine), Some(preset_iso_path(&st, alpine)));
                            return;
                        }
                    }
                    app.open_downloader(Some("alpine"));
                }
            },
        );
        cta_view.addSubview(&alpine_btn);

        let weak_dl = Rc::downgrade(self);
        let dl_btn = widgets::push_button(
            mtm,
            rect(266.0, 74.0, 174.0, 34.0),
            "↓ Browse Linux OSes",
            &mut targets,
            move || {
                if let Some(app) = weak_dl.upgrade() {
                    app.open_downloader(None);
                }
            },
        );
        cta_view.addSubview(&dl_btn);

        let weak_new = Rc::downgrade(self);
        let local_btn = widgets::push_button(
            mtm,
            rect(452.0, 74.0, 160.0, 34.0),
            "+ Use Local ISO…",
            &mut targets,
            move || {
                if let Some(app) = weak_new.upgrade() {
                    let iso = pick_iso_file(mtm);
                    app.open_create_wizard(None, iso);
                }
            },
        );
        cta_view.addSubview(&local_btn);

        let cli_tip = widgets::selectable_mono_label(
            mtm,
            rect(18.0, 118.0, w - 36.0, 18.0),
            "Terminal shortcut: rustvm quickstart alpine --headless   |   rustvm quickstart ubuntu-desktop",
            11.0,
        );
        cta_view.addSubview(&cli_tip);
        self.detail_container.addSubview(&cta_box);

        self.detail_container.setFrameSize(size(w + 68.0, 500.0));
    }
}

fn add_kv_row(
    mtm: MainThreadMarker,
    parent: &FlippedView,
    x: f64,
    y: f64,
    key: &str,
    val: &str,
) {
    let k = widgets::secondary_label(mtm, rect(x, y, 92.0, 18.0), key, 12.0);
    let v = widgets::label(mtm, rect(x + 96.0, y, 214.0, 18.0), val, 12.5);
    parent.addSubview(&k);
    parent.addSubview(&v);
}

fn state_color(state: VmState) -> Retained<NSColor> {
    match state {
        VmState::Running => NSColor::systemGreenColor(),
        VmState::Paused => NSColor::systemOrangeColor(),
        VmState::Starting | VmState::Resuming | VmState::Pausing | VmState::Stopping => {
            NSColor::systemBlueColor()
        }
        VmState::Error => NSColor::systemRedColor(),
        VmState::Stopped => NSColor::systemGrayColor(),
    }
}

fn clear_subviews(view: &FlippedView) {
    let subviews = view.subviews();
    for i in (0..subviews.count()).rev() {
        subviews.objectAtIndex(i).removeFromSuperview();
    }
}
