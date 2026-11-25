use objc2_foundation::NSString;
use std::ffi::c_void;

const K_IOPM_ASSERTION_LEVEL_ON: u32 = 255;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMAssertionCreateWithName(
        assertion_type: *const c_void,
        assertion_level: u32,
        assertion_name: *const c_void,
        assertion_id: *mut u32,
    ) -> i32;

    fn IOPMAssertionRelease(assertion_id: u32) -> i32;
}

/// Holds native macOS IOKit power management assertions (`PreventUserIdleDisplaySleep`
/// and `PreventUserIdleSystemSleep`) so the Mac will not sleep or turn off its screen
/// while a Linux virtual machine is actively running.
///
/// Assertions are automatically released on `release()` or when dropped.
pub struct SleepInhibitor {
    display_assertion_id: Option<u32>,
    system_assertion_id: Option<u32>,
}

impl SleepInhibitor {
    pub fn new() -> Self {
        Self {
            display_assertion_id: None,
            system_assertion_id: None,
        }
    }

    pub fn is_active(&self) -> bool {
        self.display_assertion_id.is_some() || self.system_assertion_id.is_some()
    }

    pub fn acquire(&mut self, reason: &str) {
        if self.is_active() {
            return;
        }
        // NSString is toll-free bridged with CFStringRef on macOS
        let reason_ns = NSString::from_str(reason);
        let reason_ptr = (&*reason_ns as *const NSString).cast::<c_void>();

        let display_type = NSString::from_str("PreventUserIdleDisplaySleep");
        let mut display_id: u32 = 0;
        let rc_display = unsafe {
            IOPMAssertionCreateWithName(
                (&*display_type as *const NSString).cast::<c_void>(),
                K_IOPM_ASSERTION_LEVEL_ON,
                reason_ptr,
                &mut display_id,
            )
        };
        if rc_display == 0 {
            self.display_assertion_id = Some(display_id);
        }

        let system_type = NSString::from_str("PreventUserIdleSystemSleep");
        let mut system_id: u32 = 0;
        let rc_system = unsafe {
            IOPMAssertionCreateWithName(
                (&*system_type as *const NSString).cast::<c_void>(),
                K_IOPM_ASSERTION_LEVEL_ON,
                reason_ptr,
                &mut system_id,
            )
        };
        if rc_system == 0 {
            self.system_assertion_id = Some(system_id);
        }
    }

    pub fn release(&mut self) {
        if let Some(id) = self.display_assertion_id.take() {
            unsafe {
                IOPMAssertionRelease(id);
            }
        }
        if let Some(id) = self.system_assertion_id.take() {
            unsafe {
                IOPMAssertionRelease(id);
            }
        }
    }
}

impl Default for SleepInhibitor {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for SleepInhibitor {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iokit_sleep_inhibitor_acquires_and_releases() {
        let mut inh = SleepInhibitor::new();
        assert!(!inh.is_active());
        inh.acquire("RustVM unit test");
        assert!(inh.is_active());
        inh.release();
        assert!(!inh.is_active());
    }
}
