use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSBackingStoreType, NSButton, NSColor, NSControlStateValueOff, NSControlStateValueOn,
    NSPopUpButton, NSSlider, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::NSString;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use crate::catalog::{OS_CATALOG, OsPreset, is_preset_downloaded, preset_iso_path};
use crate::config::{
    CreateVmRequest, DiskIoMode, DisplayScale, HostSpecs, VmConfig, VmStore, format_memory_mb,
};
use crate::disk;
use crate::gui::widgets::{
    self, FlippedView, TargetStore, pick_directory, pick_iso_file, rect, show_error_alert,
};
use crate::vm::{RosettaStatus, is_nested_virtualization_supported};

pub enum WizardMode {
    Create {
        prefill_preset: Option<&'static OsPreset>,
        prefill_iso: Option<PathBuf>,
    },
    Edit(VmConfig),
}

pub struct WizardResult {
    pub vm: VmConfig,
    pub start_immediately: bool,
}

pub struct VmWizardWindow {
    window: Retained<NSWindow>,
    _targets: TargetStore,
}

impl VmWizardWindow {
    pub fn open<F, G>(
        mtm: MainThreadMarker,
        mode: WizardMode,
        on_complete: F,
        on_open_downloader: G,
    ) -> Self
    where
        F: Fn(WizardResult) + 'static,
        G: Fn(Option<String>) + 'static,
    {
        let host = HostSpecs::detect();
        let store = VmStore::open_default().ok();

        let is_edit = matches!(mode, WizardMode::Edit(_));
        let w = 576.0;
        let h = if is_edit { 664.0 } else { 710.0 };

        let style = NSWindowStyleMask::Titled | NSWindowStyleMask::Closable;
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                mtm.alloc(),
                rect(220.0, 110.0, w, h),
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe {
            window.setReleasedWhenClosed(false);
        }
        window.setTitle(&NSString::from_str(if is_edit {
            "Virtual Machine Settings"
        } else {
            "New Linux Virtual Machine"
        }));
        window.center();

        let root = FlippedView::new(mtm, rect(0.0, 0.0, w, h));
        let mut targets = TargetStore::new();

        let (
            init_name,
            init_preset_idx,
            init_cpus,
            init_ram_mb,
            init_disk_gb,
            init_iso,
            init_share,
            init_audio,
            init_mic,
            init_clip,
            init_rosetta,
            init_nested,
            init_disk_mode,
            init_auto_reclaim,
            init_prevent_sleep,
            init_scale,
            editing_cfg,
        ) = match mode {
            WizardMode::Create {
                prefill_preset,
                prefill_iso,
            } => {
                let preset = prefill_preset.unwrap_or(&OS_CATALOG[0]);
                let idx = OS_CATALOG
                    .iter()
                    .position(|p| p.id == preset.id)
                    .unwrap_or(0);
                let iso = prefill_iso.or_else(|| {
                    store.as_ref().and_then(|s| {
                        if is_preset_downloaded(s, preset) {
                            Some(preset_iso_path(s, preset))
                        } else {
                            None
                        }
                    })
                });
                (
                    preset.short_name.to_string(),
                    idx,
                    preset.default_cpus.min(host.max_vm_cpus()),
                    preset.default_ram_mb.clamp(512, host.max_vm_memory_mb()),
                    preset.default_disk_gb,
                    iso,
                    None,
                    true,
                    false,
                    true,
                    false,
                    false,
                    DiskIoMode::Fastest,
                    true,
                    true,
                    DisplayScale::Crisp15x,
                    None,
                )
            }
            WizardMode::Edit(cfg) => {
                let idx = cfg
                    .os_preset
                    .as_deref()
                    .and_then(|id| OS_CATALOG.iter().position(|p| p.id == id))
                    .unwrap_or(OS_CATALOG.len());
                let eff_scale = cfg.effective_display_scale();
                (
                    cfg.name.clone(),
                    idx,
                    cfg.cpus,
                    cfg.memory_mb.clamp(512, host.max_vm_memory_mb()),
                    cfg.disk_gb,
                    cfg.iso_path.clone(),
                    cfg.shared_folder.clone(),
                    cfg.enable_audio,
                    cfg.enable_mic,
                    cfg.enable_clipboard,
                    cfg.enable_rosetta,
                    cfg.enable_nested_virt,
                    cfg.disk_io_mode,
                    cfg.auto_reclaim_ram,
                    cfg.prevent_sleep,
                    eff_scale,
                    Some(cfg),
                )
            }
        };

        let cpus_val = Rc::new(Cell::new(init_cpus));
        let ram_mb_val = Rc::new(Cell::new(init_ram_mb));
        let disk_gb_val = Rc::new(Cell::new(init_disk_gb));
        let iso_val = Rc::new(RefCell::new(init_iso.clone()));
        let share_val = Rc::new(RefCell::new(init_share.clone()));
        let audio_val = Rc::new(Cell::new(init_audio));
        let mic_val = Rc::new(Cell::new(init_mic));
        let clip_val = Rc::new(Cell::new(init_clip));
        let rosetta_val = Rc::new(Cell::new(init_rosetta));
        let nested_val = Rc::new(Cell::new(init_nested));
        let disk_mode_val = Rc::new(Cell::new(init_disk_mode));
        let auto_reclaim_val = Rc::new(Cell::new(init_auto_reclaim));
        let prevent_sleep_val = Rc::new(Cell::new(init_prevent_sleep));
        let scale_val = Rc::new(Cell::new(init_scale));
        let start_now_val = Rc::new(Cell::new(true));
        let preset_idx_val = Rc::new(Cell::new(init_preset_idx));

        let mut y = 18.0;
        let lbl_x = 22.0;
        let ctrl_x = 152.0;
        let ctrl_w = w - ctrl_x - 22.0;

        let header = widgets::bold_label(
            mtm,
            rect(lbl_x, y, w - 44.0, 22.0),
            if is_edit {
                "Configure Virtual Machine"
            } else {
                "Create a New Linux Virtual Machine"
            },
            16.0,
        );
        root.addSubview(&header);
        y += 32.0;

        // 1. VM Name
        root.addSubview(&widgets::semibold_label(mtm, rect(lbl_x, y + 3.0, 120.0, 18.0), "VM Name:", 13.0));
        let name_field = widgets::text_input(mtm, rect(ctrl_x, y, ctrl_w, 24.0), &init_name, "e.g. Ubuntu Desktop");
        if is_edit {
            name_field.setEditable(false);
        }
        root.addSubview(&name_field);
        y += 34.0;

        // 2. OS Preset
        let mut preset_items: Vec<String> = OS_CATALOG
            .iter()
            .map(|p| {
                let cached = store
                    .as_ref()
                    .map(|s| is_preset_downloaded(s, p))
                    .unwrap_or(false);
                if cached {
                    format!("{} [ISO Cached]", p.name)
                } else {
                    format!("{} (~{} MB)", p.name, p.approx_size_mb)
                }
            })
            .collect();
        preset_items.push("Custom ARM64 Linux ISO".to_string());
        let preset_refs: Vec<&str> = preset_items.iter().map(|s| s.as_str()).collect();

        let iso_field = widgets::text_input(
            mtm,
            rect(ctrl_x, y + 36.0, ctrl_w - 150.0, 24.0),
            &init_iso
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            "Optional — select local .iso or download",
        );
        iso_field.setEditable(false);

        let max_cpus = host.max_vm_cpus().max(1) as f64;
        let max_ram_half_gb = (host.max_vm_memory_mb() / 512).max(2) as f64;
        let min_disk_gb = if is_edit { init_disk_gb as f64 } else { 4.0 };

        let cpu_readout = widgets::semibold_label(
            mtm,
            rect(ctrl_x + ctrl_w - 108.0, y + 78.0, 108.0, 18.0),
            &format!("{} vCPUs", init_cpus),
            12.5,
        );
        let ram_readout = widgets::semibold_label(
            mtm,
            rect(ctrl_x + ctrl_w - 108.0, y + 114.0, 108.0, 18.0),
            &format_memory_mb(init_ram_mb),
            12.5,
        );
        let disk_readout = widgets::semibold_label(
            mtm,
            rect(ctrl_x + ctrl_w - 108.0, y + 150.0, 108.0, 18.0),
            &format!("{} GB", init_disk_gb),
            12.5,
        );

        let cpu_slider: Retained<NSSlider>;
        let ram_slider: Retained<NSSlider>;
        let disk_slider: Retained<NSSlider>;

        {
            let cv = Rc::clone(&cpus_val);
            let cr = cpu_readout.clone();
            cpu_slider = widgets::slider(
                mtm,
                rect(ctrl_x, y + 76.0, ctrl_w - 118.0, 22.0),
                1.0,
                max_cpus,
                init_cpus as f64,
                &mut targets,
                move |v| {
                    let n = v.round().max(1.0) as usize;
                    cv.set(n);
                    cr.setStringValue(&NSString::from_str(&format!("{n} vCPUs")));
                },
            );
        }
        {
            let rv = Rc::clone(&ram_mb_val);
            let rr = ram_readout.clone();
            ram_slider = widgets::slider(
                mtm,
                rect(ctrl_x, y + 112.0, ctrl_w - 118.0, 22.0),
                1.0,
                max_ram_half_gb,
                (init_ram_mb as f64 / 512.0).round().max(1.0),
                &mut targets,
                move |v| {
                    let mb = (v.round().max(1.0) as u64) * 512;
                    rv.set(mb);
                    rr.setStringValue(&NSString::from_str(&format_memory_mb(mb)));
                },
            );
        }
        {
            let dv = Rc::clone(&disk_gb_val);
            let dr = disk_readout.clone();
            disk_slider = widgets::slider(
                mtm,
                rect(ctrl_x, y + 148.0, ctrl_w - 118.0, 22.0),
                min_disk_gb,
                256.0_f64.max(min_disk_gb),
                init_disk_gb as f64,
                &mut targets,
                move |v| {
                    let n = v.round().max(min_disk_gb) as u64;
                    dv.set(n);
                    dr.setStringValue(&NSString::from_str(&format!("{n} GB")));
                },
            );
        }

        root.addSubview(&widgets::semibold_label(mtm, rect(lbl_x, y + 4.0, 120.0, 18.0), "Linux OS:", 13.0));
        let preset_popup: Retained<NSPopUpButton> = {
            let pidx = Rc::clone(&preset_idx_val);
            let nf = name_field.clone();
            let isof = iso_field.clone();
            let iv = Rc::clone(&iso_val);
            let cv = Rc::clone(&cpus_val);
            let rv = Rc::clone(&ram_mb_val);
            let dv = Rc::clone(&disk_gb_val);
            let cs = cpu_slider.clone();
            let rs = ram_slider.clone();
            let ds = disk_slider.clone();
            let cr = cpu_readout.clone();
            let rr = ram_readout.clone();
            let dr = disk_readout.clone();
            widgets::popup(
                mtm,
                rect(ctrl_x, y, ctrl_w, 26.0),
                &preset_refs,
                init_preset_idx,
                &mut targets,
                move |idx| {
                    pidx.set(idx);
                    if !is_edit {
                        if let Some(p) = OS_CATALOG.get(idx) {
                            nf.setStringValue(&NSString::from_str(p.short_name));
                            let c = p.default_cpus.min(host.max_vm_cpus());
                            let r_mb = p.default_ram_mb.clamp(512, host.max_vm_memory_mb());
                            let d = p.default_disk_gb;
                            cv.set(c);
                            rv.set(r_mb);
                            dv.set(d);
                            cs.setDoubleValue(c as f64);
                            rs.setDoubleValue((r_mb as f64 / 512.0).round().max(1.0));
                            ds.setDoubleValue(d as f64);
                            cr.setStringValue(&NSString::from_str(&format!("{c} vCPUs")));
                            rr.setStringValue(&NSString::from_str(&format_memory_mb(r_mb)));
                            dr.setStringValue(&NSString::from_str(&format!("{d} GB")));

                            if let Ok(st) = VmStore::open_default() {
                                if is_preset_downloaded(&st, p) {
                                    let path = preset_iso_path(&st, p);
                                    isof.setStringValue(&NSString::from_str(&path.display().to_string()));
                                    *iv.borrow_mut() = Some(path);
                                } else {
                                    isof.setStringValue(&NSString::from_str(""));
                                    *iv.borrow_mut() = None;
                                }
                            }
                        }
                    }
                },
            )
        };
        if is_edit {
            preset_popup.setEnabled(false);
        }
        root.addSubview(&preset_popup);

        // 3. Installer ISO
        let iso_y = y + 36.0;
        root.addSubview(&widgets::semibold_label(mtm, rect(lbl_x, iso_y + 3.0, 120.0, 18.0), "Installer ISO:", 13.0));
        root.addSubview(&iso_field);
        {
            let iv = Rc::clone(&iso_val);
            let isof = iso_field.clone();
            let browse_btn = widgets::push_button(
                mtm,
                rect(ctrl_x + ctrl_w - 144.0, iso_y - 2.0, 76.0, 28.0),
                "Browse…",
                &mut targets,
                move || {
                    if let Some(path) = pick_iso_file(mtm) {
                        isof.setStringValue(&NSString::from_str(&path.display().to_string()));
                        *iv.borrow_mut() = Some(path);
                    }
                },
            );
            root.addSubview(&browse_btn);
        }
        {
            let pidx = Rc::clone(&preset_idx_val);
            let win_dl = window.clone();
            let on_dl = Rc::new(on_open_downloader);
            let dl_btn = widgets::push_button(
                mtm,
                rect(ctrl_x + ctrl_w - 64.0, iso_y - 2.0, 64.0, 28.0),
                "Get OS",
                &mut targets,
                move || {
                    let chosen = OS_CATALOG.get(pidx.get()).map(|p| p.id.to_string());
                    win_dl.orderOut(None);
                    (on_dl)(chosen);
                },
            );
            root.addSubview(&dl_btn);
        }

        // 4. CPU / RAM / Disk sliders
        root.addSubview(&widgets::semibold_label(mtm, rect(lbl_x, y + 78.0, 120.0, 18.0), "CPU Cores:", 13.0));
        root.addSubview(&cpu_slider);
        root.addSubview(&cpu_readout);

        root.addSubview(&widgets::semibold_label(mtm, rect(lbl_x, y + 114.0, 120.0, 18.0), "Memory (RAM):", 13.0));
        root.addSubview(&ram_slider);
        root.addSubview(&ram_readout);

        root.addSubview(&widgets::semibold_label(
            mtm,
            rect(lbl_x, y + 150.0, 120.0, 18.0),
            if is_edit { "Disk (Grow):" } else { "Virtual Disk:" },
            13.0,
        ));
        root.addSubview(&disk_slider);
        root.addSubview(&disk_readout);

        y += 186.0;

        // 5. Disk I/O Mode dropdown
        root.addSubview(&widgets::semibold_label(mtm, rect(lbl_x, y + 4.0, 120.0, 18.0), "Disk I/O Mode:", 13.0));
        {
            let dmv = Rc::clone(&disk_mode_val);
            let disk_modes = [
                DiskIoMode::Fastest.label(),
                DiskIoMode::LowRam.label(),
                DiskIoMode::Strict.label(),
            ];
            let dm_popup = widgets::popup(
                mtm,
                rect(ctrl_x, y, ctrl_w, 26.0),
                &disk_modes,
                init_disk_mode.to_index(),
                &mut targets,
                move |idx| dmv.set(DiskIoMode::from_index(idx)),
            );
            root.addSubview(&dm_popup);
        }
        y += 34.0;

        // 5b. HiDPI / Display Scale dropdown
        root.addSubview(&widgets::semibold_label(mtm, rect(lbl_x, y + 4.0, 120.0, 18.0), "HiDPI Scale:", 13.0));
        {
            let sv = Rc::clone(&scale_val);
            let scale_modes = [
                DisplayScale::Smooth1x.label(),
                DisplayScale::Crisp15x.label(),
                DisplayScale::Retina2x.label(),
            ];
            let sc_popup = widgets::popup(
                mtm,
                rect(ctrl_x, y, ctrl_w, 26.0),
                &scale_modes,
                init_scale.to_index(),
                &mut targets,
                move |idx| sv.set(DisplayScale::from_index(idx)),
            );
            root.addSubview(&sc_popup);
        }
        y += 34.0;

        // 6. Shared Host Folder
        root.addSubview(&widgets::semibold_label(mtm, rect(lbl_x, y + 3.0, 120.0, 18.0), "Shared Folder:", 13.0));
        let share_field = widgets::text_input(
            mtm,
            rect(ctrl_x, y, ctrl_w - 140.0, 24.0),
            &init_share
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            "Optional host folder (VirtioFS tag 'shared')",
        );
        share_field.setEditable(false);
        root.addSubview(&share_field);
        {
            let sv = Rc::clone(&share_val);
            let sf = share_field.clone();
            let choose_btn = widgets::push_button(
                mtm,
                rect(ctrl_x + ctrl_w - 134.0, y - 2.0, 74.0, 28.0),
                "Choose…",
                &mut targets,
                move || {
                    if let Some(dir) = pick_directory(mtm) {
                        sf.setStringValue(&NSString::from_str(&dir.display().to_string()));
                        *sv.borrow_mut() = Some(dir);
                    }
                },
            );
            root.addSubview(&choose_btn);
        }
        {
            let sv = Rc::clone(&share_val);
            let sf = share_field.clone();
            let clear_btn = widgets::push_button(
                mtm,
                rect(ctrl_x + ctrl_w - 56.0, y - 2.0, 56.0, 28.0),
                "Clear",
                &mut targets,
                move || {
                    sf.setStringValue(&NSString::from_str(""));
                    *sv.borrow_mut() = None;
                },
            );
            root.addSubview(&clear_btn);
        }

        y += 36.0;
        root.addSubview(&widgets::separator(mtm, rect(lbl_x, y, w - 44.0, 1.0)));
        y += 12.0;

        // 7. Performance & Hardware Feature Checkboxes
        {
            let psv = Rc::clone(&prevent_sleep_val);
            let cb = widgets::checkbox(
                mtm,
                rect(ctrl_x, y, ctrl_w, 20.0),
                "Keep Mac & screen awake while VM is running",
                init_prevent_sleep,
                &mut targets,
                move |b| psv.set(b),
            );
            root.addSubview(&cb);
        }
        y += 24.0;
        {
            let arv = Rc::clone(&auto_reclaim_val);
            let cb = widgets::checkbox(
                mtm,
                rect(ctrl_x, y, ctrl_w, 20.0),
                "Auto-Reclaim Idle RAM when minimized or inactive",
                init_auto_reclaim,
                &mut targets,
                move |b| arv.set(b),
            );
            root.addSubview(&cb);
        }
        y += 24.0;
        {
            let av = Rc::clone(&audio_val);
            let cb = widgets::checkbox(
                mtm,
                rect(ctrl_x, y, 195.0, 20.0),
                "Speaker Audio (Virtio Sound)",
                init_audio,
                &mut targets,
                move |b| av.set(b),
            );
            root.addSubview(&cb);

            let mv = Rc::clone(&mic_val);
            let mic_cb = widgets::checkbox(
                mtm,
                rect(ctrl_x + 204.0, y, ctrl_w - 204.0, 20.0),
                "Microphone Input (opt-in)",
                init_mic,
                &mut targets,
                move |b| mv.set(b),
            );
            root.addSubview(&mic_cb);
        }
        y += 24.0;
        {
            let cv = Rc::clone(&clip_val);
            let cb = widgets::checkbox(
                mtm,
                rect(ctrl_x, y, ctrl_w, 20.0),
                "Host ↔ Guest Clipboard Sharing (SPICE Agent)",
                init_clip,
                &mut targets,
                move |b| cv.set(b),
            );
            root.addSubview(&cb);
        }
        y += 24.0;
        {
            let nested_supported = is_nested_virtualization_supported();
            let nv = Rc::clone(&nested_val);
            let cb: Retained<NSButton> = widgets::checkbox(
                mtm,
                rect(ctrl_x, y, ctrl_w, 20.0),
                if nested_supported {
                    "Nested Virtualization (KVM / Docker on M3/M4)"
                } else {
                    "Nested Virtualization (requires M3+ Mac)"
                },
                init_nested && nested_supported,
                &mut targets,
                move |b| nv.set(b),
            );
            if !nested_supported {
                cb.setEnabled(false);
                cb.setState(NSControlStateValueOff);
                nested_val.set(false);
            }
            root.addSubview(&cb);
        }
        y += 24.0;
        {
            let rosetta_status = RosettaStatus::detect();
            let rv = Rc::clone(&rosetta_val);
            let cb: Retained<NSButton> = widgets::checkbox(
                mtm,
                rect(ctrl_x, y, ctrl_w, 20.0),
                &format!("Apple Rosetta x86_64 w/ AOT Cache ({})", rosetta_status.label()),
                init_rosetta && rosetta_status == RosettaStatus::Installed,
                &mut targets,
                move |b| rv.set(b),
            );
            if rosetta_status != RosettaStatus::Installed {
                cb.setEnabled(false);
                cb.setState(NSControlStateValueOff);
                rosetta_val.set(false);
            }
            root.addSubview(&cb);
        }

        if !is_edit {
            y += 28.0;
            let sn = Rc::clone(&start_now_val);
            let cb = widgets::checkbox(
                mtm,
                rect(ctrl_x, y, ctrl_w, 20.0),
                "Start virtual machine immediately after creating",
                true,
                &mut targets,
                move |b| sn.set(b),
            );
            cb.setState(NSControlStateValueOn);
            root.addSubview(&cb);
        }

        // Bottom buttons
        let btn_y = h - 46.0;
        root.addSubview(&widgets::separator(mtm, rect(0.0, btn_y - 12.0, w, 1.0)));

        let win_cancel = window.clone();
        let cancel_btn = widgets::push_button(
            mtm,
            rect(w - 224.0, btn_y, 92.0, 30.0),
            "Cancel",
            &mut targets,
            move || {
                win_cancel.close();
            },
        );
        root.addSubview(&cancel_btn);

        let win_ok = window.clone();
        let nf_ok = name_field.clone();
        let on_complete = Rc::new(on_complete);
        let ok_btn = widgets::primary_button(
            mtm,
            rect(w - 122.0, btn_y, 100.0, 30.0),
            if is_edit { "Save" } else { "Create VM" },
            &mut targets,
            move || {
                let store = match VmStore::open_default() {
                    Ok(s) => s,
                    Err(e) => {
                        show_error_alert(mtm, "Storage Error", &e.to_string());
                        return;
                    }
                };
                if let Some(mut cfg) = editing_cfg.clone() {
                    cfg.cpus = cpus_val.get();
                    cfg.memory_mb = ram_mb_val.get();
                    let new_disk = disk_gb_val.get();
                    if new_disk > cfg.disk_gb {
                        if let Err(e) = disk::grow_sparse_disk(&store.disk_path(&cfg.id), new_disk) {
                            show_error_alert(mtm, "Disk Resize Error", &e.to_string());
                            return;
                        }
                        cfg.disk_gb = new_disk;
                    }
                    cfg.iso_path = iso_val.borrow().clone();
                    cfg.shared_folder = share_val.borrow().clone();
                    cfg.enable_audio = audio_val.get();
                    cfg.enable_mic = mic_val.get();
                    cfg.enable_clipboard = clip_val.get();
                    cfg.enable_rosetta = rosetta_val.get();
                    cfg.enable_nested_virt = nested_val.get();
                    cfg.disk_io_mode = disk_mode_val.get();
                    cfg.auto_reclaim_ram = auto_reclaim_val.get();
                    cfg.prevent_sleep = prevent_sleep_val.get();
                    let sc = scale_val.get();
                    cfg.display_scale = Some(sc);
                    cfg.hidpi_display = sc == DisplayScale::Retina2x;
                    // Changing hardware invalidates any previously saved snapshot
                    store.clear_saved_state(&cfg.id);
                    if let Err(e) = store.save_vm(&cfg) {
                        show_error_alert(mtm, "Failed to Save Settings", &e.to_string());
                        return;
                    }
                    win_ok.close();
                    (on_complete)(WizardResult {
                        vm: cfg,
                        start_immediately: false,
                    });
                } else {
                    let name = nf_ok.stringValue().to_string();
                    let os_preset = OS_CATALOG
                        .get(preset_idx_val.get())
                        .map(|p| p.id.to_string());
                    let req = CreateVmRequest {
                        name,
                        os_preset,
                        cpus: cpus_val.get(),
                        memory_mb: ram_mb_val.get(),
                        disk_gb: disk_gb_val.get(),
                        iso_path: iso_val.borrow().clone(),
                        shared_folder: share_val.borrow().clone(),
                        enable_audio: audio_val.get(),
                        enable_clipboard: clip_val.get(),
                        enable_rosetta: rosetta_val.get(),
                    };
                    match store.create_vm(req) {
                        Ok(mut cfg) => {
                            cfg.enable_mic = mic_val.get();
                            cfg.enable_nested_virt = nested_val.get();
                            cfg.disk_io_mode = disk_mode_val.get();
                            cfg.auto_reclaim_ram = auto_reclaim_val.get();
                            cfg.prevent_sleep = prevent_sleep_val.get();
                            let sc = scale_val.get();
                            cfg.display_scale = Some(sc);
                            cfg.hidpi_display = sc == DisplayScale::Retina2x;
                            let _ = store.save_vm(&cfg);
                            let start_now = start_now_val.get();
                            win_ok.close();
                            (on_complete)(WizardResult {
                                vm: cfg,
                                start_immediately: start_now,
                            });
                        }
                        Err(e) => {
                            show_error_alert(mtm, "Could Not Create VM", &e.to_string());
                        }
                    }
                }
            },
        );
        root.addSubview(&ok_btn);

        let _ = NSColor::labelColor();
        window.setContentView(Some(&root));
        window.makeKeyAndOrderFront(None);

        Self {
            window,
            _targets: targets,
        }
    }

    pub fn focus(&self) {
        self.window.makeKeyAndOrderFront(None);
    }
}
