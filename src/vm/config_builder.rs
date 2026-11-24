use anyhow::{Context, Result, bail};
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::{NSArray, NSData, NSString, NSURL};
use objc2_virtualization::{
    VZAudioDeviceConfiguration, VZAudioInputStreamSource, VZAudioOutputStreamSink, VZBootLoader,
    VZConsoleDeviceConfiguration, VZConsolePortConfiguration, VZDirectoryShare,
    VZDirectorySharingDeviceConfiguration, VZDiskImageCachingMode,
    VZDiskImageStorageDeviceAttachment, VZDiskImageSynchronizationMode, VZEFIBootLoader,
    VZEFIVariableStore, VZEFIVariableStoreInitializationOptions, VZEntropyDeviceConfiguration,
    VZFileHandleSerialPortAttachment, VZGenericMachineIdentifier, VZGenericPlatformConfiguration,
    VZGraphicsDeviceConfiguration, VZHostAudioInputStreamSource, VZHostAudioOutputStreamSink,
    VZKeyboardConfiguration, VZLinuxBootLoader, VZLinuxRosettaAbstractSocketCachingOptions,
    VZLinuxRosettaAvailability, VZLinuxRosettaCachingOptions, VZLinuxRosettaDirectoryShare,
    VZMACAddress, VZMemoryBalloonDeviceConfiguration, VZNATNetworkDeviceAttachment,
    VZNetworkDeviceAttachment, VZNetworkDeviceConfiguration, VZPlatformConfiguration,
    VZPointingDeviceConfiguration, VZSerialPortAttachment, VZSerialPortConfiguration,
    VZSharedDirectory, VZSingleDirectoryShare, VZSpiceAgentPortAttachment,
    VZStorageDeviceAttachment, VZStorageDeviceConfiguration, VZUSBControllerConfiguration,
    VZUSBDeviceConfiguration, VZUSBKeyboardConfiguration, VZUSBMassStorageDeviceConfiguration,
    VZUSBScreenCoordinatePointingDeviceConfiguration, VZVirtioBlockDeviceConfiguration,
    VZVirtioConsoleDeviceConfiguration, VZVirtioConsoleDeviceSerialPortConfiguration,
    VZVirtioConsolePortConfiguration, VZVirtioEntropyDeviceConfiguration,
    VZVirtioFileSystemDeviceConfiguration, VZVirtioGraphicsDeviceConfiguration,
    VZVirtioGraphicsScanoutConfiguration, VZVirtioNetworkDeviceConfiguration,
    VZVirtioSoundDeviceConfiguration, VZVirtioSoundDeviceInputStreamConfiguration,
    VZVirtioSoundDeviceOutputStreamConfiguration, VZVirtioSoundDeviceStreamConfiguration,
    VZVirtioTraditionalMemoryBalloonDeviceConfiguration, VZVirtualMachineConfiguration,
    VZXHCIControllerConfiguration,
};
use std::fs;
use std::path::Path;

use crate::config::{DiskIoMode, VmConfig, VmStore};
use crate::vm::console::SerialBridge;

/// Builds and validates an Apple `VZVirtualMachineConfiguration` for a Linux VM.
pub fn build_vz_configuration(
    store: &VmStore,
    cfg: &VmConfig,
    serial: &SerialBridge,
    headless: bool,
) -> Result<Retained<VZVirtualMachineConfiguration>> {
    unsafe {
        let vz = VZVirtualMachineConfiguration::new();

        // 1. CPU & Memory clamped to Virtualization.framework limits
        let min_cpus = VZVirtualMachineConfiguration::minimumAllowedCPUCount();
        let max_cpus = VZVirtualMachineConfiguration::maximumAllowedCPUCount();
        let cpus = cfg.cpus.clamp(min_cpus, max_cpus);
        vz.setCPUCount(cpus);

        let min_mem = VZVirtualMachineConfiguration::minimumAllowedMemorySize();
        let max_mem = VZVirtualMachineConfiguration::maximumAllowedMemorySize();
        let requested_mem = cfg.memory_mb.saturating_mul(1024 * 1024);
        // Align to 1 MiB boundary
        let aligned_mem = (requested_mem / (1024 * 1024)) * (1024 * 1024);
        let mem = aligned_mem.clamp(min_mem, max_mem);
        vz.setMemorySize(mem);

        // 2. Bootloader:
        //    - GUI mode (or disk boot): UEFI (`VZEFIBootLoader`) with persistent NVRAM.
        //    - Headless mode with an ARM64 Linux ISO: automatically extract kernel+initrd
        //      and boot via `VZLinuxBootLoader` with `console=hvc0` so interactive Terminal
        //      serial I/O works out of the box (Apple's UEFI doesn't expose PL011 UART).
        let headless_boot = if headless {
            cfg.iso_path
                .as_deref()
                .and_then(|iso| prepare_headless_iso_boot(store, &cfg.id, iso).ok().flatten())
        } else {
            None
        };

        if let Some(hb) = headless_boot {
            let kernel_url = file_url(&hb.kernel_path)?;
            let linux_boot = VZLinuxBootLoader::initWithKernelURL(
                VZLinuxBootLoader::alloc(),
                &kernel_url,
            );
            let initrd_url = file_url(&hb.initrd_path)?;
            linux_boot.setInitialRamdiskURL(Some(&initrd_url));
            linux_boot.setCommandLine(&NSString::from_str(&hb.cmdline));
            let boot_loader: Retained<VZBootLoader> = Retained::into_super(linux_boot);
            vz.setBootLoader(Some(&boot_loader));
        } else {
            let nvram_path = store.nvram_path(&cfg.id);
            let nvram_url = file_url(&nvram_path)?;
            let var_store = if nvram_path.exists() {
                VZEFIVariableStore::initWithURL(VZEFIVariableStore::alloc(), &nvram_url)
            } else {
                VZEFIVariableStore::initCreatingVariableStoreAtURL_options_error(
                    VZEFIVariableStore::alloc(),
                    &nvram_url,
                    VZEFIVariableStoreInitializationOptions::AllowOverwrite,
                )
                .map_err(|e| anyhow::anyhow!("Failed to create EFI variable store: {e}"))?
            };
            let efi = VZEFIBootLoader::new();
            efi.setVariableStore(Some(&var_store));
            let boot_loader: Retained<VZBootLoader> = Retained::into_super(efi);
            vz.setBootLoader(Some(&boot_loader));
        }

        // 3. Generic Platform + persistent Machine Identifier + optional Nested KVM (M3/M4+)
        let platform = VZGenericPlatformConfiguration::new();
        let machine_id = load_or_create_machine_id(&store.machine_id_path(&cfg.id))?;
        platform.setMachineIdentifier(&machine_id);
        if cfg.enable_nested_virt
            && VZGenericPlatformConfiguration::isNestedVirtualizationSupported()
        {
            platform.setNestedVirtualizationEnabled(true);
        }
        let platform_super: Retained<VZPlatformConfiguration> = Retained::into_super(platform);
        vz.setPlatform(&platform_super);

        // 4. Storage Devices (Main Virtio-Block virtual hard drive)
        let disk_path = store.disk_path(&cfg.id);
        if !disk_path.exists() {
            bail!(
                "Virtual disk image not found at {}",
                disk_path.display()
            );
        }
        let disk_url = file_url(&disk_path)?;
        let (caching_mode, sync_mode) = match cfg.disk_io_mode {
            DiskIoMode::Fastest => (
                VZDiskImageCachingMode::Cached,
                VZDiskImageSynchronizationMode::Fsync,
            ),
            DiskIoMode::LowRam => (
                VZDiskImageCachingMode::Uncached,
                VZDiskImageSynchronizationMode::Fsync,
            ),
            DiskIoMode::Strict => (
                VZDiskImageCachingMode::Automatic,
                VZDiskImageSynchronizationMode::Full,
            ),
        };
        let disk_attachment =
            VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_cachingMode_synchronizationMode_error(
                VZDiskImageStorageDeviceAttachment::alloc(),
                &disk_url,
                false,
                caching_mode,
                sync_mode,
            )
            .map_err(|e| {
                anyhow::anyhow!("Failed to attach virtual disk {}: {e}", disk_path.display())
            })?;
        let disk_att_super: Retained<VZStorageDeviceAttachment> =
            Retained::into_super(disk_attachment);
        let block_dev = VZVirtioBlockDeviceConfiguration::initWithAttachment(
            VZVirtioBlockDeviceConfiguration::alloc(),
            &disk_att_super,
        );
        let block_super: Retained<VZStorageDeviceConfiguration> = Retained::into_super(block_dev);
        let storage_devices: Vec<Retained<VZStorageDeviceConfiguration>> = vec![block_super];
        let storage_array = NSArray::from_retained_slice(&storage_devices);
        vz.setStorageDevices(&storage_array);

        // 5. USB XHCI Controller + optional Bootable / Hot-Pluggable ISO USB Mass Storage
        let xhci = VZXHCIControllerConfiguration::new();
        if let Some(ref iso_path) = cfg.iso_path {
            if !iso_path.exists() {
                bail!(
                    "Attached ISO file does not exist: {}\nEject the ISO or select a valid file before starting.",
                    iso_path.display()
                );
            }
            let usb_iso = build_usb_iso_config(iso_path)?;
            let usb_proto = ProtocolObject::from_retained(usb_iso);
            let usb_devs = NSArray::from_retained_slice(&[usb_proto]);
            xhci.setUsbDevices(&usb_devs);
        }
        let xhci_super: Retained<VZUSBControllerConfiguration> = Retained::into_super(xhci);
        let usb_controllers = NSArray::from_retained_slice(&[xhci_super]);
        vz.setUsbControllers(&usb_controllers);

        // 6. Graphics, Keyboard, Mouse, Audio & SPICE Clipboard (GUI mode)
        if !headless {
            // Virtio-GPU
            let gpu = VZVirtioGraphicsDeviceConfiguration::new();
            let scanout =
                VZVirtioGraphicsScanoutConfiguration::initWithWidthInPixels_heightInPixels(
                    VZVirtioGraphicsScanoutConfiguration::alloc(),
                    cfg.display_width as isize,
                    cfg.display_height as isize,
                );
            let scanouts = NSArray::from_retained_slice(&[scanout]);
            gpu.setScanouts(&scanouts);
            let gpu_super: Retained<VZGraphicsDeviceConfiguration> = Retained::into_super(gpu);
            vz.setGraphicsDevices(&NSArray::from_retained_slice(&[gpu_super]));

            // USB Keyboard
            let kbd = VZUSBKeyboardConfiguration::new();
            let kbd_super: Retained<VZKeyboardConfiguration> = Retained::into_super(kbd);
            vz.setKeyboards(&NSArray::from_retained_slice(&[kbd_super]));

            // USB Absolute-Coordinate Pointer (seamless mouse without grab trap!)
            let mouse = VZUSBScreenCoordinatePointingDeviceConfiguration::new();
            let mouse_super: Retained<VZPointingDeviceConfiguration> =
                Retained::into_super(mouse);
            vz.setPointingDevices(&NSArray::from_retained_slice(&[mouse_super]));

            // Audio (Virtio Sound: speaker output, plus opt-in mic input to avoid idle coreaudiod wakeups)
            if cfg.enable_audio {
                let sound = VZVirtioSoundDeviceConfiguration::new();
                let mut streams: Vec<Retained<VZVirtioSoundDeviceStreamConfiguration>> = Vec::new();

                let out_stream = VZVirtioSoundDeviceOutputStreamConfiguration::new();
                let sink = VZHostAudioOutputStreamSink::new();
                let sink_super: Retained<VZAudioOutputStreamSink> = Retained::into_super(sink);
                out_stream.setSink(Some(&sink_super));
                streams.push(Retained::into_super(out_stream));

                if cfg.enable_mic {
                    let in_stream = VZVirtioSoundDeviceInputStreamConfiguration::new();
                    let source = VZHostAudioInputStreamSource::new();
                    let source_super: Retained<VZAudioInputStreamSource> =
                        Retained::into_super(source);
                    in_stream.setSource(Some(&source_super));
                    streams.push(Retained::into_super(in_stream));
                }

                sound.setStreams(&NSArray::from_retained_slice(&streams));

                let sound_super: Retained<VZAudioDeviceConfiguration> =
                    Retained::into_super(sound);
                vz.setAudioDevices(&NSArray::from_retained_slice(&[sound_super]));
            }

            // SPICE Agent for Host <-> Guest Clipboard Sharing
            if cfg.enable_clipboard {
                let console_dev = VZVirtioConsoleDeviceConfiguration::new();
                let spice_port = VZVirtioConsolePortConfiguration::new();
                let port_name = VZSpiceAgentPortAttachment::spiceAgentPortName();
                spice_port.setName(Some(&port_name));
                let spice_att = VZSpiceAgentPortAttachment::new();
                spice_att.setSharesClipboard(true);
                let att_super: Retained<VZSerialPortAttachment> = Retained::into_super(spice_att);
                // VZVirtioConsolePortConfiguration inherits attachment from VZConsolePortConfiguration
                let port_as_base: &VZConsolePortConfiguration = &spice_port;
                port_as_base.setAttachment(Some(&att_super));
                console_dev
                    .ports()
                    .setObject_atIndexedSubscript(Some(&spice_port), 0);

                let console_super: Retained<VZConsoleDeviceConfiguration> =
                    Retained::into_super(console_dev);
                vz.setConsoleDevices(&NSArray::from_retained_slice(&[console_super]));
            }
        }

        // 7. Serial Console (Virtio Console Serial Port)
        let serial_att =
            VZFileHandleSerialPortAttachment::initWithFileHandleForReading_fileHandleForWriting(
                VZFileHandleSerialPortAttachment::alloc(),
                Some(serial.guest_read_handle()),
                Some(serial.guest_write_handle()),
            );
        let serial_att_super: Retained<VZSerialPortAttachment> = Retained::into_super(serial_att);
        let serial_port = VZVirtioConsoleDeviceSerialPortConfiguration::new();
        let serial_base: &VZSerialPortConfiguration = &serial_port;
        serial_base.setAttachment(Some(&serial_att_super));
        let serial_super: Retained<VZSerialPortConfiguration> = Retained::into_super(serial_port);
        vz.setSerialPorts(&NSArray::from_retained_slice(&[serial_super]));

        // 8. Networking (Virtio-Net with automatic macOS NAT)
        let net = VZVirtioNetworkDeviceConfiguration::new();
        let nat = VZNATNetworkDeviceAttachment::new();
        let nat_super: Retained<VZNetworkDeviceAttachment> = Retained::into_super(nat);
        let net_base: &VZNetworkDeviceConfiguration = &net;
        net_base.setAttachment(Some(&nat_super));
        let mac_ns = NSString::from_str(&cfg.mac_address);
        if let Some(mac) = VZMACAddress::initWithString(VZMACAddress::alloc(), &mac_ns) {
            net_base.setMACAddress(&mac);
        }
        let net_super: Retained<VZNetworkDeviceConfiguration> = Retained::into_super(net);
        vz.setNetworkDevices(&NSArray::from_retained_slice(&[net_super]));

        // 9. Directory Sharing (VirtioFS shared host folder + optional Rosetta x86_64)
        let mut fs_devices: Vec<Retained<VZDirectorySharingDeviceConfiguration>> = Vec::new();
        if let Some(ref shared_path) = cfg.shared_folder {
            if shared_path.is_dir() {
                let url = file_url(shared_path)?;
                let shared_dir = VZSharedDirectory::initWithURL_readOnly(
                    VZSharedDirectory::alloc(),
                    &url,
                    cfg.shared_folder_readonly,
                );
                let single_share = VZSingleDirectoryShare::initWithDirectory(
                    VZSingleDirectoryShare::alloc(),
                    &shared_dir,
                );
                let share_super: Retained<VZDirectoryShare> = Retained::into_super(single_share);
                let tag = NSString::from_str("shared");
                let fs_dev = VZVirtioFileSystemDeviceConfiguration::initWithTag(
                    VZVirtioFileSystemDeviceConfiguration::alloc(),
                    &tag,
                );
                fs_dev.setShare(Some(&share_super));
                fs_devices.push(Retained::into_super(fs_dev));
            }
        }
        if cfg.enable_rosetta
            && VZLinuxRosettaDirectoryShare::availability()
                == VZLinuxRosettaAvailability::Installed
        {
            if let Ok(rosetta) =
                VZLinuxRosettaDirectoryShare::initWithError(VZLinuxRosettaDirectoryShare::alloc())
            {
                // Enable Rosetta AOT translation caching for much faster repeat x86_64 execution
                let cache_name = NSString::from_str("rosetta");
                if let Ok(cache_opts) =
                    VZLinuxRosettaAbstractSocketCachingOptions::initWithName_error(
                        VZLinuxRosettaAbstractSocketCachingOptions::alloc(),
                        &cache_name,
                    )
                {
                    let opts_super: Retained<VZLinuxRosettaCachingOptions> =
                        Retained::into_super(cache_opts);
                    rosetta.setOptions(Some(&opts_super));
                }
                let share_super: Retained<VZDirectoryShare> = Retained::into_super(rosetta);
                let tag = NSString::from_str("rosetta");
                let fs_dev = VZVirtioFileSystemDeviceConfiguration::initWithTag(
                    VZVirtioFileSystemDeviceConfiguration::alloc(),
                    &tag,
                );
                fs_dev.setShare(Some(&share_super));
                fs_devices.push(Retained::into_super(fs_dev));
            }
        }
        if !fs_devices.is_empty() {
            vz.setDirectorySharingDevices(&NSArray::from_retained_slice(&fs_devices));
        }

        // 10. Entropy (Hardware RNG) & Memory Balloon
        let entropy = VZVirtioEntropyDeviceConfiguration::new();
        let entropy_super: Retained<VZEntropyDeviceConfiguration> = Retained::into_super(entropy);
        vz.setEntropyDevices(&NSArray::from_retained_slice(&[entropy_super]));

        let balloon = VZVirtioTraditionalMemoryBalloonDeviceConfiguration::new();
        let balloon_super: Retained<VZMemoryBalloonDeviceConfiguration> =
            Retained::into_super(balloon);
        vz.setMemoryBalloonDevices(&NSArray::from_retained_slice(&[balloon_super]));

        // 11. Validate configuration
        vz.validateWithError()
            .map_err(|e| anyhow::anyhow!("Invalid Virtual Machine configuration: {e}"))?;

        Ok(vz)
    }
}

pub fn build_usb_iso_config(
    iso_path: &Path,
) -> Result<Retained<VZUSBMassStorageDeviceConfiguration>> {
    unsafe {
        let iso_url = file_url(iso_path)?;
        // Uncached + None on read-only ISOs prevents macOS from caching the multi-GB ISO in host RAM.
        let att =
            VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_cachingMode_synchronizationMode_error(
                VZDiskImageStorageDeviceAttachment::alloc(),
                &iso_url,
                true,
                VZDiskImageCachingMode::Uncached,
                VZDiskImageSynchronizationMode::None,
            )
            .map_err(|e| anyhow::anyhow!("Failed to attach ISO {}: {e}", iso_path.display()))?;
        let att_super: Retained<VZStorageDeviceAttachment> = Retained::into_super(att);
        let usb = VZUSBMassStorageDeviceConfiguration::initWithAttachment(
            VZUSBMassStorageDeviceConfiguration::alloc(),
            &att_super,
        );
        Ok(usb)
    }
}

fn load_or_create_machine_id(path: &Path) -> Result<Retained<VZGenericMachineIdentifier>> {
    unsafe {
        if path.exists() {
            if let Ok(bytes) = fs::read(path) {
                let data = NSData::with_bytes(&bytes);
                if let Some(id) = VZGenericMachineIdentifier::initWithDataRepresentation(
                    VZGenericMachineIdentifier::alloc(),
                    &data,
                ) {
                    return Ok(id);
                }
            }
        }
        let id = VZGenericMachineIdentifier::new();
        let data = id.dataRepresentation();
        let bytes = data.to_vec();
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        fs::write(path, bytes)
            .with_context(|| format!("Failed to save machine identifier {}", path.display()))?;
        Ok(id)
    }
}

pub fn file_url(path: &Path) -> Result<Retained<NSURL>> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let s = abs
        .to_str()
        .with_context(|| format!("Non-UTF8 path: {}", abs.display()))?;
    let ns = NSString::from_str(s);
    Ok(NSURL::fileURLWithPath(&ns))
}

#[allow(dead_code)]
fn _check_usb_device_protocol(_: &ProtocolObject<dyn VZUSBDeviceConfiguration>) {}

struct HeadlessIsoBoot {
    kernel_path: std::path::PathBuf,
    initrd_path: std::path::PathBuf,
    cmdline: String,
}

/// Inspects an ARM64 Linux ISO for a standard kernel + initrd pair and extracts them
/// so `--headless` mode can boot directly with `console=hvc0` on Virtio serial console.
fn prepare_headless_iso_boot(
    store: &VmStore,
    vm_id: &str,
    iso_path: &Path,
) -> Result<Option<HeadlessIsoBoot>> {
    use std::process::Command;

    const CANDIDATES: &[(&str, &str, &str)] = &[
        (
            "boot/vmlinuz-virt",
            "boot/initramfs-virt",
            "modules=loop,squashfs,sd-mod,usb-storage console=hvc0",
        ),
        (
            "boot/vmlinuz-lts",
            "boot/initramfs-lts",
            "modules=loop,squashfs,sd-mod,usb-storage console=hvc0",
        ),
        (
            "install.a64/vmlinuz",
            "install.a64/initrd.gz",
            "console=hvc0",
        ),
        ("casper/vmlinuz", "casper/initrd", "console=hvc0"),
        (
            "images/pxeboot/vmlinuz",
            "images/pxeboot/initrd.img",
            "console=hvc0",
        ),
    ];

    let listing = Command::new("bsdtar")
        .arg("-tf")
        .arg(iso_path)
        .output();
    let Ok(output) = listing else {
        return Ok(None);
    };
    if !output.status.success() {
        return Ok(None);
    }
    let entries = String::from_utf8_lossy(&output.stdout);

    let matched = CANDIDATES.iter().find(|(k, i, _)| {
        let has_k = entries.lines().any(|l| l.trim_start_matches("./") == *k);
        let has_i = entries.lines().any(|l| l.trim_start_matches("./") == *i);
        has_k && has_i
    });

    let Some(&(kernel_entry, initrd_entry, cmdline)) = matched else {
        return Ok(None);
    };

    let boot_dir = store.vm_dir(vm_id).join("headless_boot");
    fs::create_dir_all(&boot_dir)?;
    let kernel_path = boot_dir.join("vmlinuz");
    let initrd_path = boot_dir.join("initrd");

    let k_out = Command::new("bsdtar")
        .args(["-xOf"])
        .arg(iso_path)
        .arg(kernel_entry)
        .output()?;
    if !k_out.status.success() || k_out.stdout.is_empty() {
        return Ok(None);
    }

    // VZLinuxBootLoader on ARM64 expects an uncompressed Linux Image.
    // Modern ARM64 distros ship either raw gzip or Linux EFI zboot ("MZ..zimg" + gzip payload).
    let kernel_bytes = unpack_arm64_kernel(&k_out.stdout)?;
    fs::write(&kernel_path, kernel_bytes)?;

    let i_out = Command::new("bsdtar")
        .args(["-xOf"])
        .arg(iso_path)
        .arg(initrd_entry)
        .output()?;
    if !i_out.status.success() || i_out.stdout.is_empty() {
        return Ok(None);
    }
    fs::write(&initrd_path, i_out.stdout)?;

    Ok(Some(HeadlessIsoBoot {
        kernel_path,
        initrd_path,
        cmdline: cmdline.to_string(),
    }))
}

fn unpack_arm64_kernel(raw: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read;

    // Case 1: Direct gzip-compressed kernel Image
    if raw.len() >= 2 && raw[0] == 0x1f && raw[1] == 0x8b {
        let mut decoder = flate2::read::GzDecoder::new(raw);
        let mut out = Vec::new();
        decoder.read_to_end(&mut out)?;
        return Ok(out);
    }

    // Case 2: Linux EFI zboot container (`MZ\0\0zimg`, payload offset @ 8, payload size @ 12, alg @ 24)
    if raw.len() >= 32 && &raw[4..8] == b"zimg" {
        let offset = u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]) as usize;
        let size = u32::from_le_bytes([raw[12], raw[13], raw[14], raw[15]]) as usize;
        let end = offset.saturating_add(size);
        if end <= raw.len() && &raw[24..28] == b"gzip" {
            let mut decoder = flate2::read::GzDecoder::new(&raw[offset..end]);
            let mut out = Vec::new();
            decoder.read_to_end(&mut out)?;
            return Ok(out);
        }
    }

    // Case 3: Already an uncompressed ARM64 kernel Image
    Ok(raw.to_vec())
}
