pub mod config_builder;
pub mod console;
pub mod lock;
pub mod power;
pub mod runner;

use std::path::PathBuf;

use objc2_virtualization::{
    VZGenericPlatformConfiguration, VZLinuxRosettaAvailability, VZLinuxRosettaDirectoryShare,
    VZVirtualMachine, VZVirtualMachineState,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmState {
    Stopped,
    Starting,
    Running,
    Pausing,
    Paused,
    Resuming,
    Saving,
    Restoring,
    Stopping,
    Error,
}

impl VmState {
    pub fn from_vz(state: VZVirtualMachineState) -> Self {
        match state {
            VZVirtualMachineState::Stopped => Self::Stopped,
            VZVirtualMachineState::Starting => Self::Starting,
            VZVirtualMachineState::Running => Self::Running,
            VZVirtualMachineState::Pausing => Self::Pausing,
            VZVirtualMachineState::Paused => Self::Paused,
            VZVirtualMachineState::Resuming => Self::Resuming,
            VZVirtualMachineState::Saving => Self::Saving,
            VZVirtualMachineState::Restoring => Self::Restoring,
            VZVirtualMachineState::Stopping => Self::Stopping,
            VZVirtualMachineState::Error => Self::Error,
            _ => Self::Stopped,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Stopped => "Stopped",
            Self::Starting => "Starting…",
            Self::Running => "Running",
            Self::Pausing => "Pausing…",
            Self::Paused => "Paused",
            Self::Resuming => "Resuming…",
            Self::Saving => "Saving…",
            Self::Restoring => "Restoring…",
            Self::Stopping => "Stopping…",
            Self::Error => "Error",
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(
            self,
            Self::Starting
                | Self::Running
                | Self::Pausing
                | Self::Paused
                | Self::Resuming
                | Self::Saving
                | Self::Restoring
                | Self::Stopping
        )
    }
}

#[derive(Debug, Clone)]
pub enum VmEvent {
    StateChanged(VmState),
    GuestStopped,
    StoppedWithError(String),
    ActionError(String),
    IsoChanged(Option<PathBuf>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RosettaStatus {
    NotSupported,
    NotInstalled,
    Installed,
}

impl RosettaStatus {
    pub fn detect() -> Self {
        let avail = unsafe { VZLinuxRosettaDirectoryShare::availability() };
        match avail {
            VZLinuxRosettaAvailability::Installed => Self::Installed,
            VZLinuxRosettaAvailability::NotInstalled => Self::NotInstalled,
            _ => Self::NotSupported,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Installed => "Available (Installed)",
            Self::NotInstalled => "Supported (Install Rosetta 2 to use)",
            Self::NotSupported => "Not supported on this host",
        }
    }
}

pub fn is_virtualization_supported() -> bool {
    unsafe { VZVirtualMachine::isSupported() }
}

pub fn is_nested_virtualization_supported() -> bool {
    unsafe { VZGenericPlatformConfiguration::isNestedVirtualizationSupported() }
}
