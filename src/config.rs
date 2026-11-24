use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::disk;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DisplayScale {
    /// Exact 1x logical window size (fastest, clean 2:1 integer upscale on Retina)
    Smooth1x,
    /// 1.5x crisp resolution (2.25x sharper than 1x while staying smooth in fullscreen)
    Crisp15x,
    /// Native 2x Retina backing resolution (pixel-for-pixel sharpness, zero upscaling blur)
    Retina2x,
}

impl Default for DisplayScale {
    fn default() -> Self {
        Self::Crisp15x
    }
}

impl DisplayScale {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Smooth1x => "1× Smooth (Fastest)",
            Self::Crisp15x => "1.5× Crisp (Balanced)",
            Self::Retina2x => "2× Retina (Sharpest)",
        }
    }

    pub fn short_label(&self) -> &'static str {
        match self {
            Self::Smooth1x => "1× Smooth",
            Self::Crisp15x => "1.5× Crisp",
            Self::Retina2x => "2× Retina",
        }
    }

    pub fn from_index(idx: usize) -> Self {
        match idx {
            0 => Self::Smooth1x,
            1 => Self::Crisp15x,
            _ => Self::Retina2x,
        }
    }

    pub fn to_index(self) -> usize {
        match self {
            Self::Smooth1x => 0,
            Self::Crisp15x => 1,
            Self::Retina2x => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiskIoMode {
    /// Cached in macOS unified page cache + Fsync (2-5x faster random I/O & app launches)
    Fastest,
    /// Uncached (F_NOCACHE) + Fsync (avoids host/guest RAM double-caching)
    LowRam,
    /// Automatic caching + Full APFS barrier sync (strictest power-loss durability)
    Strict,
}

impl Default for DiskIoMode {
    fn default() -> Self {
        Self::Fastest
    }
}

impl DiskIoMode {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Fastest => "Fastest (Cached + Fsync)",
            Self::LowRam => "Low-RAM (Uncached + Fsync)",
            Self::Strict => "Strict (Full Sync)",
        }
    }

    pub fn short_label(&self) -> &'static str {
        match self {
            Self::Fastest => "Fastest Disk",
            Self::LowRam => "Low-RAM Disk",
            Self::Strict => "Strict Disk",
        }
    }

    pub fn from_index(idx: usize) -> Self {
        match idx {
            0 => Self::Fastest,
            1 => Self::LowRam,
            _ => Self::Strict,
        }
    }

    pub fn to_index(self) -> usize {
        match self {
            Self::Fastest => 0,
            Self::LowRam => 1,
            Self::Strict => 2,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmConfig {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub os_preset: Option<String>,
    pub cpus: usize,
    pub memory_mb: u64,
    pub disk_gb: u64,
    #[serde(default = "default_display_width")]
    pub display_width: usize,
    #[serde(default = "default_display_height")]
    pub display_height: usize,
    #[serde(default = "default_display_ppi")]
    pub display_ppi: usize,
    #[serde(default)]
    pub hidpi_display: bool,
    #[serde(default)]
    pub display_scale: Option<DisplayScale>,
    #[serde(default)]
    pub disk_io_mode: DiskIoMode,
    #[serde(default = "default_true")]
    pub auto_reclaim_ram: bool,
    #[serde(default)]
    pub iso_path: Option<PathBuf>,
    #[serde(default)]
    pub shared_folder: Option<PathBuf>,
    #[serde(default)]
    pub shared_folder_readonly: bool,
    #[serde(default = "default_true")]
    pub enable_audio: bool,
    #[serde(default)]
    pub enable_mic: bool,
    #[serde(default = "default_true")]
    pub enable_clipboard: bool,
    #[serde(default)]
    pub enable_rosetta: bool,
    #[serde(default)]
    pub enable_nested_virt: bool,
    #[serde(default = "generate_mac_address")]
    pub mac_address: String,
    #[serde(default = "now_iso_string")]
    pub created_at: String,
}

impl VmConfig {
    /// Returns the effective display scale mode (migrating legacy `hidpi_display` cleanly).
    pub fn effective_display_scale(&self) -> DisplayScale {
        if let Some(scale) = self.display_scale {
            scale
        } else if self.hidpi_display {
            DisplayScale::Retina2x
        } else {
            DisplayScale::Crisp15x
        }
    }
}

fn default_display_width() -> usize {
    1280
}

fn default_display_height() -> usize {
    800
}

fn default_display_ppi() -> usize {
    72
}

fn default_true() -> bool {
    true
}

pub fn generate_mac_address() -> String {
    let bytes = *uuid::Uuid::new_v4().as_bytes();
    // Locally administered, unicast MAC address (02:xx:xx:xx:xx:xx)
    format!(
        "02:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4]
    )
}

fn now_iso_string() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format_unix_timestamp(secs)
}

fn format_unix_timestamp(secs: u64) -> String {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let t = secs as libc::time_t;
    unsafe {
        libc::gmtime_r(&t, &mut tm);
    }
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

#[derive(Debug, Clone, Copy)]
pub struct HostSpecs {
    pub cpu_count: usize,
    pub total_memory_mb: u64,
}

impl HostSpecs {
    pub fn detect() -> Self {
        let cpu_count = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        let total_memory_mb = host_memory_bytes() / (1024 * 1024);
        Self {
            cpu_count,
            total_memory_mb: total_memory_mb.max(4096),
        }
    }

    pub fn max_vm_cpus(&self) -> usize {
        self.cpu_count.max(1)
    }

    pub fn recommended_cpus(&self) -> usize {
        (self.cpu_count / 2).clamp(2, 4).min(self.cpu_count)
    }

    pub fn max_vm_memory_mb(&self) -> u64 {
        // Leave at least 2 GB for macOS host
        self.total_memory_mb.saturating_sub(2048).max(1024)
    }

    pub fn recommended_memory_mb(&self) -> u64 {
        4096.min(self.max_vm_memory_mb())
    }
}

fn host_memory_bytes() -> u64 {
    let mut mem: u64 = 0;
    let mut size = std::mem::size_of::<u64>();
    let name = c"hw.memsize";
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            &mut mem as *mut u64 as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc == 0 && mem > 0 {
        mem
    } else {
        8 * 1024 * 1024 * 1024
    }
}

#[derive(Debug, Clone)]
pub struct CreateVmRequest {
    pub name: String,
    pub os_preset: Option<String>,
    pub cpus: usize,
    pub memory_mb: u64,
    pub disk_gb: u64,
    pub iso_path: Option<PathBuf>,
    pub shared_folder: Option<PathBuf>,
    pub enable_audio: bool,
    pub enable_clipboard: bool,
    pub enable_rosetta: bool,
}

pub struct VmStore {
    base_dir: PathBuf,
}

impl VmStore {
    pub fn open_default() -> Result<Self> {
        let base_dir = default_base_dir()?;
        let store = Self { base_dir };
        store.ensure_dirs()?;
        Ok(store)
    }

    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    pub fn vms_dir(&self) -> PathBuf {
        self.base_dir.join("vms")
    }

    pub fn isos_dir(&self) -> PathBuf {
        self.base_dir.join("isos")
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        fs::create_dir_all(self.vms_dir())
            .with_context(|| format!("Failed to create {}", self.vms_dir().display()))?;
        fs::create_dir_all(self.isos_dir())
            .with_context(|| format!("Failed to create {}", self.isos_dir().display()))?;
        Ok(())
    }

    pub fn vm_dir(&self, id: &str) -> PathBuf {
        self.vms_dir().join(id)
    }

    pub fn config_path(&self, id: &str) -> PathBuf {
        self.vm_dir(id).join("vm.json")
    }

    pub fn disk_path(&self, id: &str) -> PathBuf {
        self.vm_dir(id).join("disk.img")
    }

    pub fn nvram_path(&self, id: &str) -> PathBuf {
        self.vm_dir(id).join("nvram.bin")
    }

    pub fn machine_id_path(&self, id: &str) -> PathBuf {
        self.vm_dir(id).join("machine_id.bin")
    }

    pub fn serial_log_path(&self, id: &str) -> PathBuf {
        self.vm_dir(id).join("serial.log")
    }

    pub fn saved_state_path(&self, id: &str) -> PathBuf {
        self.vm_dir(id).join("state.vzsave")
    }

    pub fn has_saved_state(&self, id: &str) -> bool {
        self.saved_state_path(id).is_file()
    }

    pub fn clear_saved_state(&self, id: &str) {
        let _ = fs::remove_file(self.saved_state_path(id));
    }

    pub fn list_vms(&self) -> Result<Vec<VmConfig>> {
        self.ensure_dirs()?;
        let mut vms = Vec::new();
        for entry in fs::read_dir(self.vms_dir())? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let cfg_path = entry.path().join("vm.json");
            if cfg_path.exists() {
                match self.load_config_from_path(&cfg_path) {
                    Ok(cfg) => vms.push(cfg),
                    Err(e) => eprintln!("Warning: skipping invalid VM config {}: {e}", cfg_path.display()),
                }
            }
        }
        vms.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        Ok(vms)
    }

    pub fn load_vm(&self, name_or_id: &str) -> Result<VmConfig> {
        // Direct ID match first
        let direct = self.config_path(name_or_id);
        if direct.exists() {
            return self.load_config_from_path(&direct);
        }
        // Slug or case-insensitive name match
        let needle_slug = slugify(name_or_id);
        let needle_lower = name_or_id.trim().to_lowercase();
        for vm in self.list_vms()? {
            if vm.id == needle_slug || vm.name.to_lowercase() == needle_lower {
                return Ok(vm);
            }
        }
        bail!("Virtual machine '{name_or_id}' not found. Run `rustvm list` to see available VMs.");
    }

    pub fn create_vm(&self, req: CreateVmRequest) -> Result<VmConfig> {
        self.ensure_dirs()?;
        let name = req.name.trim().to_string();
        if name.is_empty() {
            bail!("VM name cannot be empty");
        }
        let id = slugify(&name);
        if id.is_empty() {
            bail!("VM name must contain at least one letter or digit");
        }

        let vm_dir = self.vm_dir(&id);
        if vm_dir.exists() {
            bail!("A virtual machine named '{name}' (id: '{id}') already exists");
        }

        let host = HostSpecs::detect();
        let cpus = req.cpus.clamp(1, host.max_vm_cpus());
        let memory_mb = req.memory_mb.clamp(512, host.max_vm_memory_mb());
        let disk_gb = req.disk_gb.clamp(2, 1024);

        if let Some(ref iso) = req.iso_path {
            if !iso.exists() {
                bail!("ISO file does not exist: {}", iso.display());
            }
        }
        if let Some(ref shared) = req.shared_folder {
            if !shared.is_dir() {
                bail!("Shared folder is not a directory: {}", shared.display());
            }
        }

        fs::create_dir_all(&vm_dir)
            .with_context(|| format!("Failed to create VM directory {}", vm_dir.display()))?;

        let disk_path = self.disk_path(&id);
        if let Err(e) = disk::create_sparse_disk(&disk_path, disk_gb) {
            let _ = fs::remove_dir_all(&vm_dir);
            return Err(e);
        }

        let cfg = VmConfig {
            id: id.clone(),
            name,
            os_preset: req.os_preset,
            cpus,
            memory_mb,
            disk_gb,
            display_width: default_display_width(),
            display_height: default_display_height(),
            display_ppi: default_display_ppi(),
            hidpi_display: false,
            display_scale: Some(DisplayScale::Crisp15x),
            disk_io_mode: DiskIoMode::Fastest,
            auto_reclaim_ram: true,
            iso_path: req.iso_path,
            shared_folder: req.shared_folder,
            shared_folder_readonly: false,
            enable_audio: req.enable_audio,
            enable_mic: false,
            enable_clipboard: req.enable_clipboard,
            enable_rosetta: req.enable_rosetta,
            enable_nested_virt: false,
            mac_address: generate_mac_address(),
            created_at: now_iso_string(),
        };

        if let Err(e) = self.save_vm(&cfg) {
            let _ = fs::remove_dir_all(&vm_dir);
            return Err(e);
        }

        Ok(cfg)
    }

    pub fn save_vm(&self, cfg: &VmConfig) -> Result<()> {
        let path = self.config_path(&cfg.id);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(cfg)?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, json)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn delete_vm(&self, id: &str) -> Result<()> {
        let dir = self.vm_dir(id);
        if !dir.exists() {
            bail!("VM directory does not exist: {}", dir.display());
        }
        fs::remove_dir_all(&dir)
            .with_context(|| format!("Failed to delete VM directory {}", dir.display()))?;
        Ok(())
    }

    fn load_config_from_path(&self, path: &Path) -> Result<VmConfig> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        let cfg: VmConfig = serde_json::from_str(&raw)
            .with_context(|| format!("Failed to parse {}", path.display()))?;
        Ok(cfg)
    }
}

pub fn default_base_dir() -> Result<PathBuf> {
    if let Ok(custom) = std::env::var("RUSTVM_HOME") {
        if !custom.trim().is_empty() {
            return Ok(PathBuf::from(custom));
        }
    }
    let home = std::env::var("HOME").context("HOME environment variable is not set")?;
    Ok(PathBuf::from(home).join("RustVM"))
}

pub fn slugify(input: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in input.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if (ch == ' ' || ch == '-' || ch == '_' || ch == '.') && !out.is_empty() && !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_end_matches('-').to_string()
}

pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

pub fn format_memory_mb(mb: u64) -> String {
    if mb % 1024 == 0 {
        format!("{} GB", mb / 1024)
    } else if mb >= 1024 {
        format!("{:.1} GB", mb as f64 / 1024.0)
    } else {
        format!("{mb} MB")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_normalizes_vm_names() {
        assert_eq!(slugify("Ubuntu Desktop 26.04"), "ubuntu-desktop-26-04");
        assert_eq!(slugify("  Alpine_Linux!!  "), "alpine-linux");
        assert_eq!(slugify("my---vm"), "my-vm");
    }

    #[test]
    fn formatting_helpers_work() {
        assert_eq!(format_memory_mb(512), "512 MB");
        assert_eq!(format_memory_mb(4096), "4 GB");
        assert_eq!(format_memory_mb(1536), "1.5 GB");
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MB");
    }

    #[test]
    fn vm_store_crud_lifecycle() {
        let tmp = std::env::temp_dir().join(format!("rustvm_test_{}", uuid::Uuid::new_v4()));
        let store = VmStore {
            base_dir: tmp.clone(),
        };
        store.ensure_dirs().unwrap();

        let created = store
            .create_vm(CreateVmRequest {
                name: "Test Alpine".into(),
                os_preset: Some("alpine".into()),
                cpus: 2,
                memory_mb: 1024,
                disk_gb: 4,
                iso_path: None,
                shared_folder: None,
                enable_audio: true,
                enable_clipboard: true,
                enable_rosetta: false,
            })
            .unwrap();

        assert_eq!(created.id, "test-alpine");
        assert!(store.disk_path(&created.id).exists());
        assert_eq!(store.list_vms().unwrap().len(), 1);
        assert_eq!(store.load_vm("Test Alpine").unwrap().id, "test-alpine");

        store.delete_vm(&created.id).unwrap();
        assert!(store.list_vms().unwrap().is_empty());
        let _ = fs::remove_dir_all(&tmp);
    }
}
