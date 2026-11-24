use std::path::PathBuf;

use crate::config::VmStore;

#[derive(Debug, Clone)]
pub struct OsPreset {
    pub id: &'static str,
    pub name: &'static str,
    pub short_name: &'static str,
    pub tagline: &'static str,
    pub filename: &'static str,
    pub url: &'static str,
    pub approx_size_mb: u64,
    pub default_cpus: usize,
    pub default_ram_mb: u64,
    pub default_disk_gb: u64,
    pub is_gui: bool,
    pub install_hint: &'static str,
}

pub const OS_CATALOG: &[OsPreset] = &[
    OsPreset {
        id: "alpine",
        name: "Alpine Linux 3.24 (Virt ARM64)",
        short_name: "Alpine Linux",
        tagline: "Ultra-fast minimal Linux (~89 MB) — boots in seconds",
        filename: "alpine-virt-3.24.4-aarch64.iso",
        url: "https://dl-cdn.alpinelinux.org/alpine/latest-stable/releases/aarch64/alpine-virt-3.24.4-aarch64.iso",
        approx_size_mb: 89,
        default_cpus: 2,
        default_ram_mb: 1024,
        default_disk_gb: 8,
        is_gui: false,
        install_hint: "Log in as 'root' (no password). Run 'setup-alpine' to install to the virtual disk, then eject ISO & reboot.",
    },
    OsPreset {
        id: "debian",
        name: "Debian 13.8 Netinst (ARM64)",
        short_name: "Debian 13",
        tagline: "Rock-solid Linux (~703 MB) — choose Server or GNOME/XFCE Desktop",
        filename: "debian-13.8.0-arm64-netinst.iso",
        url: "https://cdimage.debian.org/debian-cd/current/arm64/iso-cd/debian-13.8.0-arm64-netinst.iso",
        approx_size_mb: 703,
        default_cpus: 4,
        default_ram_mb: 4096,
        default_disk_gb: 24,
        is_gui: true,
        install_hint: "Follow the graphical or text installer, pick GNOME/XFCE for a desktop, then eject the ISO after install.",
    },
    OsPreset {
        id: "ubuntu-desktop",
        name: "Ubuntu 26.04 LTS Desktop (ARM64)",
        short_name: "Ubuntu Desktop",
        tagline: "Full Ubuntu GNOME graphical desktop (~3.9 GB)",
        filename: "ubuntu-26.04-desktop-arm64.iso",
        url: "https://cdimage.ubuntu.com/releases/26.04/release/ubuntu-26.04-desktop-arm64.iso",
        approx_size_mb: 3968,
        default_cpus: 4,
        default_ram_mb: 4096,
        default_disk_gb: 32,
        is_gui: true,
        install_hint: "Boots straight into the Ubuntu graphical installer. After installation finishes, click Eject ISO and restart.",
    },
    OsPreset {
        id: "ubuntu-server",
        name: "Ubuntu 26.04 LTS Server (ARM64)",
        short_name: "Ubuntu Server",
        tagline: "Ubuntu LTS Server with guided installer & OpenSSH (~2.9 GB)",
        filename: "ubuntu-26.04-live-server-arm64.iso",
        url: "https://cdimage.ubuntu.com/releases/26.04/release/ubuntu-26.04-live-server-arm64.iso",
        approx_size_mb: 2926,
        default_cpus: 4,
        default_ram_mb: 2048,
        default_disk_gb: 24,
        is_gui: false,
        install_hint: "Follow the guided Ubuntu Server installer, then click Eject ISO and reboot into your installed OS.",
    },
    OsPreset {
        id: "fedora",
        name: "Fedora 45 Workstation Live (ARM64)",
        short_name: "Fedora Workstation",
        tagline: "Modern GNOME desktop Live environment + installer (~2.8 GB)",
        filename: "Fedora-Workstation-Live-45-1.4.aarch64.iso",
        url: "https://download.fedoraproject.org/pub/fedora/linux/releases/45/Workstation/aarch64/iso/Fedora-Workstation-Live-45-1.4.aarch64.iso",
        approx_size_mb: 2849,
        default_cpus: 4,
        default_ram_mb: 4096,
        default_disk_gb: 32,
        is_gui: true,
        install_hint: "Boots into a live GNOME desktop! Try Fedora immediately or click 'Install to Hard Drive', then eject ISO.",
    },
];

pub fn find_preset(id_or_alias: &str) -> Option<&'static OsPreset> {
    let key = id_or_alias.trim().to_lowercase();
    OS_CATALOG.iter().find(|p| {
        p.id == key
            || p.short_name.to_lowercase() == key
            || (key == "ubuntu" && p.id == "ubuntu-desktop")
    })
}

pub fn preset_iso_path(store: &VmStore, preset: &OsPreset) -> PathBuf {
    store.isos_dir().join(preset.filename)
}

pub fn is_preset_downloaded(store: &VmStore, preset: &OsPreset) -> bool {
    let path = preset_iso_path(store, preset);
    path.is_file() && path.metadata().map(|m| m.len() > 1024 * 1024).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_lookup_and_aliases() {
        assert_eq!(find_preset("alpine").unwrap().id, "alpine");
        assert_eq!(find_preset("ubuntu").unwrap().id, "ubuntu-desktop");
        assert_eq!(find_preset("debian").unwrap().id, "debian");
        assert_eq!(find_preset("fedora").unwrap().id, "fedora");
        assert!(find_preset("nonexistent").is_none());
    }
}
