//! Windows resume and connectivity notifications, without a polling thread.
//!
//! Adapted from upstream `src/recovery_events.rs` (PR #8 by Xiaowei Rong /
//! feizaodemon, runtime resolution by Ray; MIT). Changes for this fork: the
//! message id avoids our `WM_APP_REARM_POLL`, and `RecoveryGate` delays the
//! post-resume refresh briefly (Wi-Fi usually reconnects within seconds) and
//! skips it when a network-restored refresh already ran since the resume, so
//! one wake never produces two back-to-back refreshes.
use std::ffi::c_void;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows::core::{s, w};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

use crate::diagnose;

// Declared directly instead of enabling three more `windows` crate features
// (IpHelper, WinSock, Power), which keeps that crate's build no larger.
#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)] // Layout mirrors NL_NETWORK_CONNECTIVITY_HINT; only the level is read.
struct NetworkConnectivityHint {
    connectivity_level: i32,
    connectivity_cost: i32,
    approaching_data_limit: u8,
    over_data_limit: u8,
    roaming: u8,
}

/// `NL_NETWORK_CONNECTIVITY_LEVEL_HINT::NetworkConnectivityLevelHintInternetAccess`.
const CONNECTIVITY_LEVEL_INTERNET_ACCESS: i32 = 3;
/// `DEVICE_NOTIFY_WINDOW_HANDLE`.
const DEVICE_NOTIFY_WINDOW_HANDLE: u32 = 0;

type HintCallback = unsafe extern "system" fn(*const c_void, NetworkConnectivityHint);

#[link(name = "user32")]
extern "system" {
    fn RegisterSuspendResumeNotification(recipient: isize, flags: u32) -> isize;
    fn UnregisterSuspendResumeNotification(handle: isize) -> i32;
}

#[link(name = "iphlpapi")]
extern "system" {
    fn CancelMibChangeNotify2(handle: isize) -> u32;
}

/// Posted to the widget when internet access returns after being lost.
pub const NETWORK_RESTORED: u32 = WM_APP + 5;
/// Delay between resume and its refresh, unless the network returns first.
pub const RESUME_REFRESH_DELAY_MS: u32 = 10_000;
const DEBOUNCE: Duration = Duration::from_secs(5);

static CONNECTIVITY: Mutex<Connectivity> = Mutex::new(Connectivity { online: None });

#[derive(Default)]
struct Connectivity {
    online: Option<bool>,
}

impl Connectivity {
    /// Only a transition from offline to internet access counts; the first
    /// report and cost-only changes do not.
    fn observe(&mut self, online: bool) -> bool {
        let restored = self.online == Some(false) && online;
        self.online = Some(online);
        restored
    }
}

/// Decides which recovery refreshes actually run.
#[derive(Debug, Default)]
pub struct RecoveryGate {
    resumed_at: Option<Instant>,
    last_resume: Option<Instant>,
    last_network: Option<Instant>,
}

impl RecoveryGate {
    /// A resume notification: `true` arms the delayed resume refresh.
    /// Windows sends both PBT_APMRESUMEAUTOMATIC and PBT_APMRESUMESUSPEND.
    pub fn on_resume(&mut self, now: Instant) -> bool {
        if self
            .last_resume
            .is_some_and(|last| now.saturating_duration_since(last) < DEBOUNCE)
        {
            return false;
        }
        self.last_resume = Some(now);
        self.resumed_at = Some(now);
        true
    }

    /// Internet access returned: `true` refreshes now.
    pub fn on_network_restored(&mut self, now: Instant) -> bool {
        if self
            .last_network
            .is_some_and(|last| now.saturating_duration_since(last) < DEBOUNCE)
        {
            return false;
        }
        self.last_network = Some(now);
        true
    }

    /// The delayed resume refresh: skipped when a network-restored refresh
    /// already ran after this resume.
    pub fn on_resume_timer(&mut self) -> bool {
        let Some(resumed) = self.resumed_at.take() else {
            return false;
        };
        !self.last_network.is_some_and(|network| network >= resumed)
    }
}

type NotifyHintChange =
    unsafe extern "system" fn(Option<HintCallback>, *const c_void, u8, *mut isize) -> u32;

/// Windows 10 2004+ only. Resolved at runtime so older builds still start,
/// without network recovery.
unsafe fn notify_network_connectivity_hint_change() -> Option<NotifyHintChange> {
    let module = LoadLibraryW(w!("iphlpapi.dll")).ok()?;
    let function = GetProcAddress(module, s!("NotifyNetworkConnectivityHintChange"))?;
    Some(std::mem::transmute::<
        unsafe extern "system" fn() -> isize,
        NotifyHintChange,
    >(function))
}

unsafe extern "system" fn network_changed(context: *const c_void, hint: NetworkConnectivityHint) {
    let restored = CONNECTIVITY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .observe(hint.connectivity_level == CONNECTIVITY_LEVEL_INTERNET_ACCESS);
    if restored {
        let _ = PostMessageW(
            HWND(context as *mut _),
            NETWORK_RESTORED,
            WPARAM(0),
            LPARAM(0),
        );
    }
}

/// Registration handles; dropped (unregistered) with the widget.
pub struct Watch {
    power: isize,
    network: isize,
}

// The handles are only registered and released, never dereferenced.
unsafe impl Send for Watch {}

impl Watch {
    pub fn register(hwnd: HWND) -> Self {
        unsafe {
            let power =
                RegisterSuspendResumeNotification(hwnd.0 as isize, DEVICE_NOTIFY_WINDOW_HANDLE);
            let Some(notify) = notify_network_connectivity_hint_change() else {
                diagnose::log(format!(
                    "recovery notifications registered resume={} network=unavailable_on_this_windows",
                    power != 0
                ));
                return Self { power, network: 0 };
            };
            let mut network = 0isize;
            // initialNotification = TRUE seeds the current level (not a "restore").
            let status = notify(
                Some(network_changed),
                hwnd.0 as *const c_void,
                1,
                &mut network,
            );
            diagnose::log(format!(
                "recovery notifications registered resume={} network={}",
                power != 0,
                status == 0
            ));
            Self {
                power,
                network: if status == 0 { network } else { 0 },
            }
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        unsafe {
            if self.network != 0 {
                let _ = CancelMibChangeNotify2(self.network);
            }
            if self.power != 0 {
                let _ = UnregisterSuspendResumeNotification(self.power);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_return_of_internet_access_counts_as_restored() {
        let mut state = Connectivity::default();
        assert!(!state.observe(true), "first report");
        assert!(!state.observe(true));
        assert!(!state.observe(false));
        assert!(!state.observe(false));
        assert!(state.observe(true));
        assert!(!state.observe(true));
    }

    #[test]
    fn duplicate_resume_notifications_arm_one_delayed_refresh() {
        let now = Instant::now();
        let mut gate = RecoveryGate::default();
        assert!(gate.on_resume(now));
        assert!(
            !gate.on_resume(now + Duration::from_millis(300)),
            "second PBT_ message"
        );
        assert!(gate.on_resume_timer());
        assert!(!gate.on_resume_timer(), "one refresh per resume");
    }

    #[test]
    fn a_network_refresh_after_resume_replaces_the_delayed_resume_refresh() {
        let now = Instant::now();
        let mut gate = RecoveryGate::default();
        assert!(gate.on_resume(now));
        assert!(gate.on_network_restored(now + Duration::from_secs(3)));
        assert!(!gate.on_resume_timer(), "no second refresh 7 s later");
        // A network refresh from before the resume does not suppress it.
        let mut gate = RecoveryGate::default();
        assert!(gate.on_network_restored(now));
        assert!(gate.on_resume(now + Duration::from_secs(60)));
        assert!(gate.on_resume_timer());
    }

    #[test]
    fn network_flapping_is_debounced_but_later_recoveries_run() {
        let now = Instant::now();
        let mut gate = RecoveryGate::default();
        assert!(gate.on_network_restored(now));
        assert!(!gate.on_network_restored(now + Duration::from_secs(1)));
        assert!(gate.on_network_restored(now + Duration::from_secs(6)));
        assert!(
            gate.on_resume(now + Duration::from_secs(6)),
            "kinds are independent"
        );
    }
}
