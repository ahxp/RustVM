use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSRunLoop};
use std::cell::{Cell, RefCell};
use std::io::{self, Write};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::catalog::{OS_CATALOG, OsPreset, find_preset, is_preset_downloaded, preset_iso_path};
use crate::config::{
    CreateVmRequest, DiskIoMode, HostSpecs, VmConfig, VmStore, format_bytes, format_memory_mb,
};
use crate::disk;
use crate::download;
use crate::gui;
use crate::vm::console::SerialBridge;
use crate::vm::runner::VmRunner;
use crate::vm::{
    RosettaStatus, VmEvent, VmState, is_nested_virtualization_supported,
    is_virtualization_supported,
};

#[derive(Parser, Debug)]
#[command(
    name = "rustvm",
    version,
    about = "RustVM — Simple native macOS VirtualBox-like Linux VM manager powered by Apple Virtualization.framework",
    long_about = "RustVM lets you easily install and run ARM64 Linux operating systems (Ubuntu, Debian, Fedora, Alpine, or any ARM64 ISO) with native Apple Silicon virtualization.\n\nRun `rustvm` with no arguments to open the VirtualBox-style graphical manager, or use `rustvm quickstart alpine` for a 1-command Linux boot."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Open the VirtualBox-style graphical VM Manager (default when no command is given)
    Gui,

    /// List built-in downloadable ARM64 Linux OS images
    Catalog,

    /// Download an official ARM64 Linux ISO (alpine, debian, ubuntu-desktop, ubuntu-server, fedora)
    Download {
        /// OS preset ID from `rustvm catalog`
        preset: String,
    },

    /// One-command setup: download an OS ISO (if needed), create a VM, and launch it immediately
    Quickstart {
        /// OS preset ID (e.g. alpine, debian, ubuntu-desktop, ubuntu-server, fedora)
        preset: String,
        /// Optional custom name for the VM
        #[arg(long)]
        name: Option<String>,
        /// Run in Terminal serial console mode instead of opening a GUI display window
        #[arg(long)]
        headless: bool,
        /// Number of vCPUs (defaults to preset recommendation)
        #[arg(long)]
        cpus: Option<usize>,
        /// RAM in GB (defaults to preset recommendation)
        #[arg(long)]
        ram: Option<u64>,
        /// Virtual disk size in GB (defaults to preset recommendation)
        #[arg(long)]
        disk: Option<u64>,
        /// Optional host directory to share with Linux at VirtioFS tag 'shared'
        #[arg(long)]
        share: Option<PathBuf>,
    },

    /// Create a new Linux virtual machine
    Create {
        /// Display name for the virtual machine
        name: String,
        /// Optional OS preset (alpine, debian, ubuntu-desktop, ubuntu-server, fedora)
        #[arg(long)]
        preset: Option<String>,
        /// Path to a local ARM64 Linux .iso installer image
        #[arg(long)]
        iso: Option<PathBuf>,
        /// Download the preset ISO automatically if not cached locally
        #[arg(long)]
        download: bool,
        /// Number of virtual CPU cores
        #[arg(long)]
        cpus: Option<usize>,
        /// Memory in GB (e.g. 4 for 4 GB)
        #[arg(long)]
        ram: Option<u64>,
        /// Virtual disk size in GB (sparse, uses ~0 bytes until written)
        #[arg(long)]
        disk: Option<u64>,
        /// Host folder to share with the Linux VM (mount via `mount -t virtiofs shared /mnt/shared`)
        #[arg(long)]
        share: Option<PathBuf>,
        /// Enable Apple Rosetta VirtioFS share (tag 'rosetta') for x86_64 binaries inside Linux
        #[arg(long)]
        rosetta: bool,
        /// Disable virtual audio device
        #[arg(long)]
        no_audio: bool,
    },

    /// List all configured virtual machines
    List,

    /// Show detailed configuration and paths for a virtual machine
    Info {
        /// VM name or ID
        name: String,
    },

    /// Start a virtual machine (opens GUI display window unless --headless is set)
    Start {
        /// VM name or ID
        name: String,
        /// Run in Terminal serial console mode (no display window)
        #[arg(long)]
        headless: bool,
    },

    /// Attach or eject a bootable installer ISO image
    Iso {
        /// VM name or ID
        name: String,
        #[command(subcommand)]
        action: IsoAction,
    },

    /// Modify settings for an existing virtual machine
    Config {
        /// VM name or ID
        name: String,
        /// Set number of vCPUs
        #[arg(long)]
        cpus: Option<usize>,
        /// Set memory in GB
        #[arg(long)]
        ram: Option<u64>,
        /// Grow the virtual disk to a larger size in GB
        #[arg(long)]
        disk_grow: Option<u64>,
        /// Set shared host directory
        #[arg(long)]
        share: Option<PathBuf>,
        /// Remove shared host directory
        #[arg(long)]
        unshare: bool,
        /// Enable or disable Rosetta x86_64 translation share
        #[arg(long)]
        rosetta: Option<bool>,
        /// Enable or disable virtual speaker audio
        #[arg(long)]
        audio: Option<bool>,
        /// Enable or disable virtual microphone input (off by default to reduce wakeups)
        #[arg(long)]
        mic: Option<bool>,
        /// Enable or disable SPICE clipboard sharing
        #[arg(long)]
        clipboard: Option<bool>,
        /// Enable or disable HiDPI 2x Retina display (false = Smooth 1x, 4x faster GUI)
        #[arg(long)]
        hidpi: Option<bool>,
        /// Disk I/O mode: fastest (Cached+Fsync), low-ram (Uncached+Fsync), or strict (Full Sync)
        #[arg(long)]
        disk_mode: Option<String>,
        /// Enable or disable automatic idle RAM reclaim when minimized/inactive
        #[arg(long)]
        auto_reclaim: Option<bool>,
        /// Enable or disable nested virtualization (KVM/Docker on M3/M4 Macs)
        #[arg(long)]
        nested_virt: Option<bool>,
        /// Discard any saved VM state snapshot so next boot starts fresh
        #[arg(long)]
        discard_state: bool,
        /// Set display resolution as WIDTHxHEIGHT (e.g. 1280x800)
        #[arg(long)]
        resolution: Option<String>,
    },

    /// Delete a virtual machine and its disk image
    Delete {
        /// VM name or ID
        name: String,
        /// Skip confirmation prompt
        #[arg(short = 'y', long)]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum IsoAction {
    /// Attach an ISO installer image to the VM
    Attach {
        /// Path to the .iso file
        path: PathBuf,
    },
    /// Eject the currently attached ISO image so the VM boots from its virtual disk
    Eject,
}

pub fn run(cli: Cli) -> Result<()> {
    let store = VmStore::open_default()?;

    match cli.command {
        None | Some(Commands::Gui) => {
            gui::run_manager_app(None)
        }
        Some(Commands::Catalog) => cmd_catalog(&store),
        Some(Commands::Download { preset }) => {
            let p = resolve_preset(&preset)?;
            let path = download_with_progress(&store, p)?;
            println!("\nSaved to: {}", path.display());
            println!("Create & boot a VM with: rustvm quickstart {}", p.id);
            Ok(())
        }
        Some(Commands::Quickstart {
            preset,
            name,
            headless,
            cpus,
            ram,
            disk: disk_gb,
            share,
        }) => {
            let p = resolve_preset(&preset)?;
            let iso_path = if is_preset_downloaded(&store, p) {
                println!("Using cached ISO: {}", preset_iso_path(&store, p).display());
                preset_iso_path(&store, p)
            } else {
                download_with_progress(&store, p)?
            };

            let vm_name = name.unwrap_or_else(|| p.short_name.to_string());
            let cfg = match store.load_vm(&vm_name) {
                Ok(existing) => {
                    println!("Found existing VM '{}' — starting it...", existing.name);
                    existing
                }
                Err(_) => {
                    let host = HostSpecs::detect();
                    let req = CreateVmRequest {
                        name: vm_name,
                        os_preset: Some(p.id.to_string()),
                        cpus: cpus.unwrap_or(p.default_cpus).min(host.max_vm_cpus()),
                        memory_mb: ram
                            .map(|g| g * 1024)
                            .unwrap_or(p.default_ram_mb)
                            .min(host.max_vm_memory_mb()),
                        disk_gb: disk_gb.unwrap_or(p.default_disk_gb),
                        iso_path: Some(iso_path),
                        shared_folder: share,
                        enable_audio: true,
                        enable_clipboard: true,
                        enable_rosetta: false,
                    };
                    let created = store.create_vm(req)?;
                    println!(
                        "Created VM '{}' ({} vCPUs, {}, {} GB sparse disk)",
                        created.name,
                        created.cpus,
                        format_memory_mb(created.memory_mb),
                        created.disk_gb
                    );
                    println!("Tip: {}", p.install_hint);
                    created
                }
            };

            if headless {
                run_headless(&store, &cfg)
            } else {
                gui::run_manager_app(Some(cfg.id))
            }
        }
        Some(Commands::Create {
            name,
            preset,
            iso,
            download: should_download,
            cpus,
            ram,
            disk: disk_gb,
            share,
            rosetta,
            no_audio,
        }) => {
            let host = HostSpecs::detect();
            let preset_obj = match preset.as_deref() {
                Some(p) => Some(resolve_preset(p)?),
                None => None,
            };

            let iso_path = if let Some(path) = iso {
                let abs = path.canonicalize().unwrap_or(path);
                if !abs.is_file() {
                    bail!("ISO file not found: {}", abs.display());
                }
                Some(abs)
            } else if let Some(p) = preset_obj {
                if is_preset_downloaded(&store, p) {
                    Some(preset_iso_path(&store, p))
                } else if should_download {
                    Some(download_with_progress(&store, p)?)
                } else {
                    println!(
                        "Note: Preset ISO '{}' is not downloaded yet. Download it anytime with `rustvm download {}` or pass `--download`.",
                        p.filename, p.id
                    );
                    None
                }
            } else {
                None
            };

            let req = CreateVmRequest {
                name,
                os_preset: preset_obj.map(|p| p.id.to_string()),
                cpus: cpus
                    .or_else(|| preset_obj.map(|p| p.default_cpus))
                    .unwrap_or_else(|| host.recommended_cpus()),
                memory_mb: ram
                    .map(|g| g * 1024)
                    .or_else(|| preset_obj.map(|p| p.default_ram_mb))
                    .unwrap_or_else(|| host.recommended_memory_mb()),
                disk_gb: disk_gb
                    .or_else(|| preset_obj.map(|p| p.default_disk_gb))
                    .unwrap_or(20),
                iso_path,
                shared_folder: share,
                enable_audio: !no_audio,
                enable_clipboard: true,
                enable_rosetta: rosetta,
            };

            let cfg = store.create_vm(req)?;
            println!("Created virtual machine '{}' (id: {})", cfg.name, cfg.id);
            println!(
                "  Specs : {} vCPUs | {} RAM | {} GB sparse disk",
                cfg.cpus,
                format_memory_mb(cfg.memory_mb),
                cfg.disk_gb
            );
            if let Some(ref iso) = cfg.iso_path {
                println!("  ISO   : {}", iso.display());
            }
            println!("\nStart it with:");
            println!("  rustvm start \"{}\"            (GUI window)", cfg.name);
            println!("  rustvm start \"{}\" --headless (Terminal console)", cfg.name);
            Ok(())
        }
        Some(Commands::List) => cmd_list(&store),
        Some(Commands::Info { name }) => {
            let cfg = store.load_vm(&name)?;
            cmd_info(&store, &cfg)
        }
        Some(Commands::Start { name, headless }) => {
            let cfg = store.load_vm(&name)?;
            if headless {
                run_headless(&store, &cfg)
            } else {
                gui::run_manager_app(Some(cfg.id))
            }
        }
        Some(Commands::Iso { name, action }) => {
            let mut cfg = store.load_vm(&name)?;
            match action {
                IsoAction::Attach { path } => {
                    let abs = path.canonicalize().unwrap_or(path);
                    if !abs.is_file() {
                        bail!("ISO file not found: {}", abs.display());
                    }
                    cfg.iso_path = Some(abs.clone());
                    store.save_vm(&cfg)?;
                    println!("Attached ISO to '{}': {}", cfg.name, abs.display());
                }
                IsoAction::Eject => {
                    cfg.iso_path = None;
                    store.save_vm(&cfg)?;
                    println!(
                        "Ejected ISO from '{}'. Next boot will boot from the virtual disk.",
                        cfg.name
                    );
                }
            }
            Ok(())
        }
        Some(Commands::Config {
            name,
            cpus,
            ram,
            disk_grow,
            share,
            unshare,
            rosetta,
            audio,
            mic,
            clipboard,
            hidpi,
            disk_mode,
            auto_reclaim,
            nested_virt,
            discard_state,
            resolution,
        }) => {
            let mut cfg = store.load_vm(&name)?;
            let host = HostSpecs::detect();
            if let Some(c) = cpus {
                cfg.cpus = c.clamp(1, host.max_vm_cpus());
            }
            if let Some(r) = ram {
                cfg.memory_mb = (r * 1024).clamp(512, host.max_vm_memory_mb());
            }
            if let Some(new_gb) = disk_grow {
                disk::grow_sparse_disk(&store.disk_path(&cfg.id), new_gb)?;
                cfg.disk_gb = new_gb;
            }
            if unshare {
                cfg.shared_folder = None;
            } else if let Some(dir) = share {
                let abs = dir.canonicalize().unwrap_or(dir);
                if !abs.is_dir() {
                    bail!("Shared folder is not a directory: {}", abs.display());
                }
                cfg.shared_folder = Some(abs);
            }
            if let Some(r) = rosetta {
                cfg.enable_rosetta = r;
            }
            if let Some(a) = audio {
                cfg.enable_audio = a;
            }
            if let Some(m) = mic {
                cfg.enable_mic = m;
            }
            if let Some(c) = clipboard {
                cfg.enable_clipboard = c;
            }
            if let Some(h) = hidpi {
                cfg.hidpi_display = h;
            }
            if let Some(dm) = disk_mode {
                cfg.disk_io_mode = match dm.trim().to_lowercase().as_str() {
                    "fastest" | "cached" => DiskIoMode::Fastest,
                    "low-ram" | "lowram" | "uncached" => DiskIoMode::LowRam,
                    "strict" | "full" => DiskIoMode::Strict,
                    other => bail!(
                        "Invalid --disk-mode '{other}'. Expected: fastest, low-ram, or strict"
                    ),
                };
            }
            if let Some(ar) = auto_reclaim {
                cfg.auto_reclaim_ram = ar;
            }
            if let Some(nv) = nested_virt {
                cfg.enable_nested_virt = nv;
            }
            if let Some(res) = resolution {
                let parts: Vec<&str> = res.split(['x', 'X']).collect();
                if parts.len() != 2 {
                    bail!("Resolution must be WIDTHxHEIGHT (e.g. 1920x1080)");
                }
                let w: usize = parts[0].parse().context("Invalid display width")?;
                let h: usize = parts[1].parse().context("Invalid display height")?;
                if w < 640 || h < 480 || w > 7680 || h > 4320 {
                    bail!("Resolution out of supported range (640x480 .. 7680x4320)");
                }
                cfg.display_width = w;
                cfg.display_height = h;
            }
            if discard_state || cpus.is_some() || ram.is_some() || disk_grow.is_some() {
                store.clear_saved_state(&cfg.id);
            }
            store.save_vm(&cfg)?;
            println!("Updated configuration for '{}'.", cfg.name);
            cmd_info(&store, &cfg)
        }
        Some(Commands::Delete { name, yes }) => {
            let cfg = store.load_vm(&name)?;
            if !yes {
                print!(
                    "Delete virtual machine '{}' and its virtual disk? [y/N]: ",
                    cfg.name
                );
                io::stdout().flush()?;
                let mut answer = String::new();
                io::stdin().read_line(&mut answer)?;
                if !matches!(answer.trim().to_lowercase().as_str(), "y" | "yes") {
                    println!("Cancelled.");
                    return Ok(());
                }
            }
            store.delete_vm(&cfg.id)?;
            println!("Deleted virtual machine '{}'.", cfg.name);
            Ok(())
        }
    }
}

fn resolve_preset(query: &str) -> Result<&'static OsPreset> {
    find_preset(query).with_context(|| {
        let ids: Vec<&str> = OS_CATALOG.iter().map(|p| p.id).collect();
        format!(
            "Unknown OS preset '{query}'. Available presets: {}",
            ids.join(", ")
        )
    })
}

fn download_with_progress(store: &VmStore, preset: &OsPreset) -> Result<PathBuf> {
    println!("Downloading {} (~{} MB)...", preset.name, preset.approx_size_mb);
    println!("URL: {}", preset.url);
    let pb = ProgressBar::new(preset.approx_size_mb * 1024 * 1024);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} [{elapsed_precise}] [{bar:38.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, ETA {eta})",
        )
        .unwrap()
        .progress_chars("█▓░"),
    );

    let cancel = Arc::new(AtomicBool::new(false));
    let path = download::download_preset(store, preset, cancel, |prog| {
        if let Some(total) = prog.total_bytes {
            pb.set_length(total);
        }
        pb.set_position(prog.downloaded_bytes);
    })?;
    pb.finish_with_message("Done");
    Ok(path)
}

fn cmd_catalog(store: &VmStore) -> Result<()> {
    println!("Built-in ARM64 Linux OS Catalog (UEFI-ready for Apple Silicon):\n");
    println!(
        "{:<16} {:<34} {:<10} {:<10} {}",
        "PRESET ID", "OPERATING SYSTEM", "SIZE", "STATUS", "DEFAULTS"
    );
    println!("{}", "-".repeat(92));
    for p in OS_CATALOG {
        let status = if is_preset_downloaded(store, p) {
            "Cached"
        } else {
            "Online"
        };
        let size = if p.approx_size_mb >= 1024 {
            format!("{:.1} GB", p.approx_size_mb as f64 / 1024.0)
        } else {
            format!("{} MB", p.approx_size_mb)
        };
        println!(
            "{:<16} {:<34} {:<10} {:<10} {} CPU, {}, {} GB disk",
            p.id,
            p.name,
            size,
            status,
            p.default_cpus,
            format_memory_mb(p.default_ram_mb),
            p.default_disk_gb
        );
    }
    println!("\nQuickest way to boot Linux:");
    println!("  rustvm quickstart alpine            # ~89 MB, boots in seconds");
    println!("  rustvm quickstart ubuntu-desktop    # Full Ubuntu GNOME desktop");
    println!("  rustvm                              # Open graphical VM Manager");
    Ok(())
}

fn cmd_list(store: &VmStore) -> Result<()> {
    let vms = store.list_vms()?;
    if vms.is_empty() {
        println!("No virtual machines yet (data dir: {}).", store.base_dir().display());
        println!("\nGet started in one command:");
        println!("  rustvm                              # Open graphical VirtualBox-style Manager");
        println!("  rustvm quickstart alpine            # Download & boot Alpine Linux (~89 MB)");
        println!("  rustvm catalog                      # See all built-in Linux OSes");
        return Ok(());
    }

    println!(
        "{:<22} {:<6} {:<9} {:<20} {}",
        "NAME", "CPUS", "RAM", "DISK (USED / MAX)", "INSTALLER ISO"
    );
    println!("{}", "-".repeat(86));
    for vm in vms {
        let usage = disk::disk_usage(&store.disk_path(&vm.id));
        let disk_col = format!(
            "{} / {} GB",
            format_bytes(usage.actual_bytes),
            vm.disk_gb
        );
        let iso_col = vm
            .iso_path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "— (boots disk)".to_string());
        println!(
            "{:<22} {:<6} {:<9} {:<20} {}",
            vm.name,
            vm.cpus,
            format_memory_mb(vm.memory_mb),
            disk_col,
            iso_col
        );
    }
    Ok(())
}

fn cmd_info(store: &VmStore, cfg: &VmConfig) -> Result<()> {
    let usage = disk::disk_usage(&store.disk_path(&cfg.id));
    println!("Virtual Machine : {} (id: {})", cfg.name, cfg.id);
    println!("OS Preset       : {}", cfg.os_preset.as_deref().unwrap_or("Custom Linux"));
    println!(
        "Saved State     : {}",
        if store.has_saved_state(&cfg.id) {
            "Ready (instant resume)"
        } else {
            "None"
        }
    );
    println!(
        "CPUs            : {} vCPUs{}",
        cfg.cpus,
        if cfg.enable_nested_virt { " (Nested KVM enabled)" } else { "" }
    );
    println!(
        "Memory          : {} (Auto-Reclaim={})",
        format_memory_mb(cfg.memory_mb),
        if cfg.auto_reclaim_ram { "On" } else { "Off" }
    );
    println!(
        "Virtual Disk    : {} GB allocated ({} used on disk)",
        cfg.disk_gb,
        format_bytes(usage.actual_bytes)
    );
    println!("  Disk Path     : {}", store.disk_path(&cfg.id).display());
    println!("  EFI NVRAM     : {}", store.nvram_path(&cfg.id).display());
    println!("  Serial Log    : {}", store.serial_log_path(&cfg.id).display());
    println!(
        "Display         : {}x{} Virtio-GPU ({})",
        cfg.display_width,
        cfg.display_height,
        if cfg.hidpi_display { "HiDPI 2x" } else { "Smooth 1x" }
    );
    println!("Disk I/O Mode   : {}", cfg.disk_io_mode.label());
    println!("Network         : NAT (MAC {})", cfg.mac_address);
    println!(
        "Installer ISO   : {}",
        cfg.iso_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "None (boots virtual disk)".to_string())
    );
    println!(
        "Audio / Clipb.  : Speakers={} | Mic={} | SPICE Clipboard={}",
        if cfg.enable_audio { "On" } else { "Off" },
        if cfg.enable_audio && cfg.enable_mic { "On" } else { "Off" },
        if cfg.enable_clipboard { "On" } else { "Off" }
    );
    println!(
        "Nested KVM      : {} (Host: {})",
        if cfg.enable_nested_virt { "Enabled" } else { "Disabled" },
        if is_nested_virtualization_supported() { "Supported" } else { "Not supported" }
    );
    println!(
        "Rosetta x86_64  : {} (AOT cache enabled when on | Host: {})",
        if cfg.enable_rosetta { "Enabled" } else { "Disabled" },
        RosettaStatus::detect().label()
    );
    if let Some(ref shared) = cfg.shared_folder {
        println!(
            "Shared Folder   : {} ({})",
            shared.display(),
            if cfg.shared_folder_readonly { "read-only" } else { "read-write" }
        );
        println!("  Mount in Linux: sudo mkdir -p /mnt/shared && sudo mount -t virtiofs shared /mnt/shared");
    } else {
        println!("Shared Folder   : None");
    }
    println!("Created         : {}", cfg.created_at);
    Ok(())
}

/// Boots a VM directly inside the current Terminal using a Virtio serial console.
pub fn run_headless(store: &VmStore, cfg: &VmConfig) -> Result<()> {
    if !is_virtualization_supported() {
        bail!("Apple Virtualization.framework is not supported on this Mac.");
    }

    println!("================================================================");
    println!(
        " RustVM Headless Console — {} ({} vCPUs, {})",
        cfg.name,
        cfg.cpus,
        format_memory_mb(cfg.memory_mb)
    );
    if let Some(ref iso) = cfg.iso_path {
        println!(" Boot ISO : {}", iso.display());
    }
    println!(" Controls : Press Ctrl+] once to shut down, twice to force stop");
    println!(" Log file : {}", store.serial_log_path(&cfg.id).display());
    println!("================================================================\n");

    let escape_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let escape_flag = Arc::new(AtomicBool::new(false));
    let esc_flag_clone = Arc::clone(&escape_flag);
    let esc_count_clone = Arc::clone(&escape_count);

    let serial = SerialBridge::new_interactive(&store.serial_log_path(&cfg.id), move || {
        esc_count_clone.fetch_add(1, Ordering::SeqCst);
        esc_flag_clone.store(true, Ordering::SeqCst);
    })?;

    let should_exit = Rc::new(Cell::new(false));
    let fatal_error: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

    let exit_cb = Rc::clone(&should_exit);
    let err_cb = Rc::clone(&fatal_error);

    let mut runner = VmRunner::new(store, cfg, serial, true, move |event| match event {
        VmEvent::StateChanged(VmState::Running) => {
            let _ = io::stderr().write_all(b"[RustVM] VM is running.\r\n");
        }
        VmEvent::GuestStopped => {
            let _ = io::stderr().write_all(b"\r\n[RustVM] Guest powered off.\r\n");
            exit_cb.set(true);
        }
        VmEvent::StoppedWithError(msg) => {
            let _ = write!(io::stderr(), "\r\n[RustVM] VM stopped with error: {msg}\r\n");
            *err_cb.borrow_mut() = Some(msg);
            exit_cb.set(true);
        }
        VmEvent::ActionError(msg) => {
            let _ = write!(io::stderr(), "\r\n[RustVM] Error: {msg}\r\n");
            *err_cb.borrow_mut() = Some(msg);
            exit_cb.set(true);
        }
        _ => {}
    })?;

    runner.start();

    let mut requested_stop = false;
    while !should_exit.get() {
        if escape_flag.swap(false, Ordering::SeqCst) {
            let presses = escape_count.load(Ordering::SeqCst);
            if !requested_stop && presses == 1 && runner.can_request_stop() {
                requested_stop = true;
                let _ = io::stderr().write_all(
                    b"\r\n[RustVM] Sending graceful ACPI shutdown... (press Ctrl+] again to force power off)\r\n",
                );
                runner.request_stop();
            } else {
                let _ = io::stderr().write_all(b"\r\n[RustVM] Forcing power off...\r\n");
                runner.force_stop();
            }
        }
        let until = NSDate::dateWithTimeIntervalSinceNow(0.05);
        unsafe {
            NSRunLoop::mainRunLoop().runMode_beforeDate(NSDefaultRunLoopMode, &until);
        }
    }

    runner.shutdown_serial();

    if let Some(err) = fatal_error.borrow().clone() {
        bail!("{err}");
    }
    Ok(())
}
