# RustVM

A simple, fast, VirtualBox-style Linux virtual machine manager written in **100% Rust** for **Apple Silicon Macs**, built directly on Apple's native `Virtualization.framework` and `AppKit`.

**Zero external dependencies** — no QEMU, no VirtualBox, no Homebrew required.

## Highlights

- **VirtualBox-style Native macOS GUI**: Manage multiple Linux VMs, inspect hardware/storage, monitor live status, and run graphical Linux desktops in resizable native display windows (`VZVirtualMachineView` with Virtio-GPU auto-resize).
- **One-Click Official Linux OS Downloader**: Built-in catalog of verified ARM64 UEFI Linux installers:
  - **Alpine Linux 3.24** (~89 MB — downloads & boots in seconds)
  - **Debian 13.8 Netinst** (~703 MB — server or GNOME/XFCE desktop)
  - **Fedora 45 Workstation Live** (~2.8 GB — live GNOME desktop + installer)
  - **Ubuntu 26.04 LTS Server** (~2.9 GB)
  - **Ubuntu 26.04 LTS Desktop** (~3.9 GB — full Ubuntu GNOME desktop)
  - Plus bring-your-own ARM64 `.iso` support via the native file picker.
- **Live USB ISO Attach & Eject**: Attach an installer ISO to install Linux onto a sparse APFS virtual disk, then click **Eject ISO** (even while the VM is running!) and reboot straight into your installed OS.
- **Full Virtual Hardware & Performance Tuning**:
  - UEFI boot (`VZEFIBootLoader`) with persistent NVRAM (`nvram.bin`)
  - **Smooth 1× Display Mode** (default) with live **HiDPI (2×)** toggle on the toolbar — avoids 2× Retina software-rendering lag while dynamically resizing Linux to fit your window
  - **Fast & Low-RAM Disk I/O** (`Uncached + Fsync`) to avoid APFS `F_FULLFSYNC` stutters and host/guest memory double-caching
  - **Live `Free RAM` Button** (Virtio memory balloon pulse) to reclaim unused Linux page cache back to macOS on demand
  - USB keyboard & seamless absolute mouse, automatic NAT networking, Virtio-Sound audio, and SPICE clipboard sharing
  - **Shared Host Folders (VirtioFS)** — share any macOS folder with Linux
  - Optional **Apple Rosetta x86_64** share for running x86_64 binaries inside ARM64 Linux
  - Interactive **Headless Serial Console** (`--headless`) right inside Terminal

## Requirements

- Apple Silicon Mac (`arm64`) running macOS 15+
- Rust toolchain (`cargo` / `rustc`) and Xcode Command Line Tools (`codesign`)

## Simple 1-Step Setup

```bash
chmod +x setup.sh run.sh
./setup.sh
```

This builds `RustVM`, signs it with the `com.apple.security.virtualization` entitlement, creates `dist/RustVM.app`, and links `rustvm` into `~/.local/bin/rustvm`.

To also install `RustVM.app` into `~/Applications`:

```bash
./setup.sh --install-app
```

To package a distributable macOS `.dmg` installer (`dist/RustVM-1.0.0-arm64.dmg`):

```bash
./build-dmg.sh
# or:
make dmg
```

## Running Linux

### Option 1: Graphical VirtualBox-Style Manager (Recommended)

Open the app bundle or run with no arguments:

```bash
open dist/RustVM.app
# or:
./run.sh
```

1. Click **↓ Get Linux OS** to download an official ARM64 Linux ISO (or click **+ New VM** to pick a local `.iso`).
2. Pick CPU cores, RAM, and disk size (sparse disk uses ~0 bytes until written).
3. Click **▶ Start** to open the Linux display window and install to the virtual hard disk.
4. Once installation finishes, click **Eject ISO** and restart to boot directly from disk.

### Option 2: 1-Command Terminal Quickstart

```bash
# Download & boot Alpine Linux (~89 MB) in a GUI display window:
./run.sh quickstart alpine

# Download & boot Alpine Linux directly inside your Terminal (press Ctrl+] to stop):
./run.sh quickstart alpine --headless

# Download & boot full Ubuntu 26.04 Desktop GUI installer:
./run.sh quickstart ubuntu-desktop
```

### Useful CLI Commands

```bash
rustvm catalog                                          # List downloadable Linux OS presets
rustvm download debian                                  # Download an official ARM64 ISO
rustvm create "My Ubuntu" --iso ~/Downloads/ubuntu.iso --cpus 4 --ram 4 --disk 32
rustvm list                                             # List all VMs & disk usage
rustvm info "My Ubuntu"                                 # Show VM details & mount commands
rustvm start "My Ubuntu"                                # Start with native GUI display
rustvm start "My Ubuntu" --headless                     # Start in Terminal serial console
rustvm iso "My Ubuntu" eject                            # Eject ISO after OS install
rustvm iso "My Ubuntu" attach /path/to/installer.iso    # Attach an ISO
rustvm config "My Ubuntu" --cpus 6 --ram 8 --share ~/Shared
rustvm delete "My Ubuntu"                               # Delete VM and its disk image
```

## Sharing a Folder with Linux (VirtioFS)

1. Select a host folder in **Settings** / **Shared Folder → Choose…** (or `--share ~/Shared`).
2. Inside the Linux VM, mount it with:

```bash
sudo mkdir -p /mnt/shared
sudo mount -t virtiofs shared /mnt/shared
```

## Where VM Data Is Stored

All data lives cleanly under `~/RustVM/` (override with `RUSTVM_HOME`):

```
~/RustVM/
├── isos/                   # Cached downloaded Linux ISOs
└── vms/
    └── <vm-id>/
        ├── vm.json         # VM hardware & device configuration
        ├── disk.img        # Sparse raw virtual hard disk
        ├── nvram.bin       # Persistent UEFI NVRAM variable store
        ├── machine_id.bin  # Persistent hardware machine identifier
        └── serial.log      # Captured serial console output
```
