use std::collections::BTreeSet;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};
use windows::Win32::System::Registry::*;
use windows::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};
use windows::Win32::UI::Accessibility::HWINEVENTHOOK;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::Shell::{ExtractIconExW, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::build_info;
use crate::codex_mcp;
use crate::diagnose;
use crate::localization::{self, LanguageId, Strings};
use crate::models::{AppUsageData, CreditBalance};
use crate::native_interop::{
    self, Color, TIMER_COUNTDOWN, TIMER_CREDENTIAL_WATCH, TIMER_LAYOUT_RECONCILE, TIMER_POLL,
    TIMER_RESET_POLL, TIMER_UPDATE_CHECK, WM_APP_TRAY, WM_APP_USAGE_UPDATED,
};
use crate::poller;
use crate::provider_schedule::{self, CLAUDE, CODEX};
use crate::quota_cache;
use crate::taskbar_layout::{self, LayoutAction, LayoutEventGate};
use crate::theme;
use crate::tray_icon;
use crate::updater::{self, ReleaseDescriptor, UpdateCheckResult};

/// Wrapper to make HWND sendable across threads (safe for PostMessage usage)
#[derive(Clone, Copy)]
struct SendHwnd(isize);

unsafe impl Send for SendHwnd {}

impl SendHwnd {
    fn from_hwnd(hwnd: HWND) -> Self {
        Self(hwnd.0 as isize)
    }
    fn to_hwnd(self) -> HWND {
        HWND(self.0 as *mut _)
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct CodexAuthEpisode {
    active: bool,
    passive_failures: u8,
    model_free_refresh_attempted: bool,
    model_free_refresh_succeeded: bool,
    exec_attempted: bool,
    credential_snapshot: poller::CredentialWatchSnapshot,
}

/// Shared application state
struct AppState {
    hwnd: SendHwnd,
    taskbar_hwnd: Option<HWND>,
    tray_notify_hwnd: Option<HWND>,
    win_event_hook: Option<HWINEVENTHOOK>,
    is_dark: bool,
    embedded: bool,
    language_override: Option<LanguageId>,
    language: LanguageId,
    session_percent: f64,
    session_text: String,
    weekly_percent: f64,
    weekly_text: String,
    codex_session_percent: f64,
    codex_session_text: String,
    codex_weekly_percent: f64,
    codex_weekly_text: String,
    antigravity_session_percent: f64,
    antigravity_session_text: String,
    antigravity_weekly_percent: f64,
    antigravity_weekly_text: String,
    claude_code_available: bool,
    show_claude_code: bool,
    show_codex: bool,
    show_antigravity: bool,
    show_session_window: bool,
    show_weekly_window: bool,
    show_drag_handle: bool,
    enable_codex_mcp: bool,
    usage_display: UsageDisplayMode,
    credit_display: CreditDisplayMode,
    credit_position: CreditPosition,
    credit_value_mode: CreditValueMode,
    codex_credit_text: String,
    bar_color: Option<String>,
    alert_thresholds_percent: Vec<u8>,
    notified_quota_windows: BTreeSet<String>,

    data: Option<AppUsageData>,
    provider_health: [crate::provider_health::ProviderHealth; 3],
    provider_generation: u64,
    /// Where each provider's shown values came from.
    data_sources: [poller::DataSource; 3],
    /// Last snapshot written to (or restored from) disk, to limit rewrites.
    persisted: [Option<quota_cache::Snapshot>; 3],
    claude_desktop_sign_in: bool,

    poll_interval_ms: u32,
    adaptive_polling: bool,
    schedule: provider_schedule::ProviderSchedule,
    codex_auth: CodexAuthEpisode,
    last_codex_exec_refresh_unix: Option<u64>,
    force_notify_auth_error: bool,
    last_poll_ok: bool,
    update_status: UpdateStatus,
    last_update_check_unix: Option<u64>,

    taskbar_index: usize,
    tray_offset: i32,
    manual_position: bool,
    dragging: bool,
    drag_start_mouse_x: i32,
    drag_start_client_x: i32,
    drag_start_offset: i32,

    widget_visible: bool,
}

#[derive(Clone, Debug)]
enum UpdateStatus {
    Idle,
    Checking,
    Applying,
    UpToDate,
    Available(ReleaseDescriptor),
}

const CODEX_AUTH_REFRESH_AFTER_FAILURES: u8 = 3;
const CODEX_AUTH_EXEC_AFTER_FAILURES: u8 = 6;
const CODEX_EXEC_REFRESH_COOLDOWN_SECS: u64 = 24 * 60 * 60;
// No automatic model tasks. Legacy cooldown settings remain readable.
const ALLOW_AUTOMATIC_MODEL_RECOVERY: bool = false;
const CODEX_AUTH_WATCH_MS: u32 = 2_000;

const POLL_1_MIN: u32 = 60_000;
const POLL_30_SEC: u32 = 30_000;
const POLL_5_MIN: u32 = 300_000;
const POLL_15_MIN: u32 = 900_000;
const POLL_1_HOUR: u32 = 3_600_000;
const IDM_FREQ_ADAPTIVE: u16 = 15;

// Menu item IDs for update frequency
const IDM_FREQ_1MIN: u16 = 10;
const IDM_FREQ_30SEC: u16 = 14;
const IDM_FREQ_5MIN: u16 = 11;
const IDM_FREQ_15MIN: u16 = 12;
const IDM_FREQ_1HOUR: u16 = 13;
const IDM_START_WITH_WINDOWS: u16 = 20;
const IDM_RESET_POSITION: u16 = 30;
const IDM_VERSION_ACTION: u16 = 31;
const IDM_SHOW_DRAG_HANDLE: u16 = 32;
const IDM_LANG_SYSTEM: u16 = 40;
const IDM_LANG_ENGLISH: u16 = 41;
const IDM_LANG_DUTCH: u16 = 42;
const IDM_LANG_SPANISH: u16 = 43;
const IDM_LANG_FRENCH: u16 = 44;
const IDM_LANG_GERMAN: u16 = 45;
const IDM_LANG_JAPANESE: u16 = 46;
const IDM_LANG_KOREAN: u16 = 47;
const IDM_LANG_TRADITIONAL_CHINESE: u16 = 48;
const IDM_LANG_RUSSIAN: u16 = 49;
const IDM_LANG_PORTUGUESE_BRAZIL: u16 = 50;
const IDM_LANG_SIMPLIFIED_CHINESE: u16 = 51;
const IDM_MODEL_CLAUDE_CODE: u16 = 60;
const IDM_MODEL_CODEX: u16 = 61;
const IDM_MODEL_ANTIGRAVITY: u16 = 62;
const IDM_SHOW_SESSION_WINDOW: u16 = 71;
const IDM_SHOW_WEEKLY_WINDOW: u16 = 72;
const IDM_ALERT_OFF: u16 = 80;
const IDM_ALERT_2: u16 = 85;
const IDM_USAGE_DISPLAY_REMAINING: u16 = 73;
const IDM_USAGE_DISPLAY_USED: u16 = 74;
const IDM_USAGE_DISPLAY_MIXED: u16 = 77;
const IDM_BAR_COLOR_WINDOWS_ACCENT: u16 = 75;
const IDM_BAR_COLOR_CUSTOM: u16 = 76;
const IDM_ALERT_5: u16 = 81;
const IDM_ALERT_10: u16 = 82;
const IDM_ALERT_20: u16 = 83;
const IDM_ALERT_30: u16 = 84;
const IDM_CREDIT_DISPLAY_ALWAYS: u16 = 86;
const IDM_CREDIT_DISPLAY_WHEN_NEEDED: u16 = 87;
const IDM_CREDIT_DISPLAY_OFF: u16 = 88;
const IDM_CREDIT_POSITION_LEFT: u16 = 89;
const IDM_CREDIT_POSITION_RIGHT: u16 = 90;
const IDM_CREDIT_VALUE_CREDITS: u16 = 91;
const IDM_CREDIT_VALUE_USD: u16 = 92;
const IDM_ENABLE_CODEX_MCP: u16 = 93;
const IDM_OPEN_LOG_FILE: u16 = 94;

const WM_DPICHANGED_MSG: u32 = 0x02E0;
const WM_APP_UPDATE_CHECK_COMPLETE: u32 = WM_APP + 2;
/// Posted by provider workers so the UI thread re-arms TIMER_POLL.
const WM_APP_REARM_POLL: u32 = WM_APP + 4;
const TRAY_ICON_UPDATE_REPOSITION_SUPPRESS_MS: u64 = 750;

/// How often the watchdog thread polls for an explorer.exe restart (which
/// recreates the taskbar and wipes our tray-icon registration).
const TASKBAR_WATCH_INTERVAL_SECS: u64 = 2;

/// Coalesces shell layout events so none is dropped; see taskbar_layout.
static LAYOUT_GATE: Mutex<LayoutEventGate> = Mutex::new(LayoutEventGate::new());
/// Set only by the Exit command. Any other WM_DESTROY means Windows destroyed
/// our parent taskbar (for example a disconnected display); the message loop
/// then stays alive for watchdog recovery (adapted from upstream PR #27, ddddyyyy).
static EXIT_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Current system DPI (96 = 100% scaling, 144 = 150%, 192 = 200%, etc.)
static CURRENT_DPI: AtomicU32 = AtomicU32::new(96);
static QUOTA_WIDEST_DIGIT_CACHE: Mutex<Option<(u32, char)>> = Mutex::new(None);

/// Scale a base pixel value (designed at 96 DPI) to the current DPI.
fn sc(px: i32) -> i32 {
    scale_logical_at_dpi(px, CURRENT_DPI.load(Ordering::Relaxed))
}

fn scale_logical_at_dpi(px: i32, dpi: u32) -> i32 {
    (px as f64 * dpi as f64 / 96.0).round() as i32
}

/// Re-query the monitor DPI for our window and update the cached value.
/// Uses GetDpiForWindow which returns the live DPI (unlike GetDpiForSystem
/// which is cached at process startup and never changes).
fn refresh_dpi() {
    let hwnd = {
        let state = lock_state();
        state.as_ref().map(|s| s.hwnd.to_hwnd())
    };
    if let Some(hwnd) = hwnd {
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        if dpi > 0 {
            CURRENT_DPI.store(dpi, Ordering::Relaxed);
        }
    }
}

/// Spacing below which two relaunches are treated as a storm (e.g. explorer.exe
/// crash-looping); when detected we back off instead of spawning in a tight loop.
const RELAUNCH_THROTTLE_SECS: u64 = 10;
const RELAUNCH_BACKOFF_SECS: u64 = 30;
/// Environment flag set on a relaunched child so it waits for the previous
/// instance's single-instance mutex instead of exiting immediately.
const ENV_RELAUNCH: &str = "CODEX_USAGE_RELAUNCH";
/// Unix timestamp (seconds) of the relaunch that spawned this process, passed to
/// the child so it can detect a relaunch storm.
const ENV_LAST_RELAUNCH_UNIX: &str = "CODEX_USAGE_LAST_RELAUNCH_UNIX";

/// Relaunch the widget as a fresh process after explorer.exe has restarted.
///
/// When the shell restarts it destroys our embedded child window outright (the
/// window is gone, not merely orphaned - `IsWindow` returns false) and leaves
/// the UI thread parked in `GetMessage` with no window to recreate in place.
/// Spawning a clean new process - which re-embeds into the freshly created
/// taskbar - and exiting this one is the robust recovery. The child is flagged
/// via `ENV_RELAUNCH` so it waits for this instance's single-instance mutex to
/// be released before taking over (see the guard in `run`).
fn relaunch_self() {
    // Back off if we are relaunching very soon after the relaunch that spawned
    // us: that signals the shell is crash-looping, not a one-off restart.
    let now = now_unix_secs();
    let last = std::env::var(ENV_LAST_RELAUNCH_UNIX)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    if last != 0 && now.saturating_sub(last) < RELAUNCH_THROTTLE_SECS {
        diagnose::log("relaunch storm detected; backing off before relaunching");
        std::thread::sleep(Duration::from_secs(RELAUNCH_BACKOFF_SECS));
    }

    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            diagnose::log_error("watchdog: unable to resolve current executable", error);
            return;
        }
    };

    let args: Vec<String> = std::env::args().skip(1).collect();
    match std::process::Command::new(exe)
        .args(&args)
        .env(ENV_RELAUNCH, "1")
        .env(ENV_LAST_RELAUNCH_UNIX, now.to_string())
        .spawn()
    {
        Ok(_) => {
            diagnose::log("watchdog: relaunched fresh instance, exiting old one");
            std::process::exit(0);
        }
        Err(error) => {
            diagnose::log_error("watchdog: unable to spawn relaunched instance", error);
        }
    }
}

/// Detect explorer.exe restarts and recover from them.
///
/// Once explorer destroys the taskbar, our embedded child window is destroyed
/// and the UI message loop is dead, so recovery cannot happen in-process. This
/// dedicated thread (independent of the dead message loop) polls the taskbar
/// handle and, when it changes, relaunches the widget as a fresh process.
fn spawn_taskbar_watchdog() {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(TASKBAR_WATCH_INTERVAL_SECS));
        let stored = {
            let state = lock_state();
            state.as_ref().and_then(|s| s.taskbar_hwnd)
        };
        // Only relevant once we have embedded into a taskbar at least once.
        let Some(old) = stored else {
            continue;
        };
        let taskbars = native_interop::find_taskbars();
        let widget_alive = {
            let state = lock_state();
            state.as_ref().map(|s| s.hwnd.to_hwnd())
        }
        .is_some_and(native_interop::is_window);
        if !taskbars.is_empty()
            && (!widget_alive || !taskbars.iter().any(|taskbar| taskbar.hwnd == old))
        {
            let new = taskbars[0].hwnd;
            diagnose::log(format!(
                "watchdog: widget_alive={widget_alive} taskbar old={:?} new={:?} -> relaunching",
                old.0, new.0
            ));
            relaunch_self();
        }
    });
}

fn load_embedded_app_icons() -> (HICON, HICON) {
    unsafe {
        let mut exe_buf = [0u16; 260];
        let len = GetModuleFileNameW(None, &mut exe_buf) as usize;
        if len == 0 {
            return (HICON::default(), HICON::default());
        }

        let mut large_icon = HICON::default();
        let mut small_icon = HICON::default();
        let extracted = ExtractIconExW(
            PCWSTR::from_raw(exe_buf.as_ptr()),
            0,
            Some(&mut large_icon),
            Some(&mut small_icon),
            1,
        );

        if extracted == 0 {
            (HICON::default(), HICON::default())
        } else {
            (large_icon, small_icon)
        }
    }
}

unsafe impl Send for AppState {}

static STATE: Mutex<Option<AppState>> = Mutex::new(None);

/// Lock STATE safely, recovering from poisoned mutex
fn lock_state() -> MutexGuard<'static, Option<AppState>> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

const SETTINGS_DIR: &str = "CodexUsage";
const LEGACY_SETTINGS_DIR: &str = "ClaudeCodeUsageMonitor";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsSource {
    Current,
    Legacy,
    Defaults,
}

impl SettingsSource {
    fn label(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Legacy => "legacy",
            Self::Defaults => "defaults",
        }
    }
}

fn appdata_path(directory: &str) -> PathBuf {
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(appdata).join(directory).join("settings.json")
}

fn settings_path() -> PathBuf {
    appdata_path(SETTINGS_DIR)
}

fn legacy_settings_path() -> PathBuf {
    appdata_path(LEGACY_SETTINGS_DIR)
}

#[derive(Debug, Serialize, Deserialize)]
struct SettingsFile {
    #[serde(default)]
    tray_offset: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    manual_position: Option<bool>,
    #[serde(default)]
    taskbar_index: usize,
    #[serde(default = "default_poll_interval")]
    poll_interval_ms: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_update_check_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_codex_exec_refresh_unix: Option<u64>,
    #[serde(default = "default_widget_visible")]
    widget_visible: bool,
    #[serde(default = "default_show_claude_code")]
    show_claude_code: bool,
    #[serde(default = "default_show_codex")]
    show_codex: bool,
    #[serde(default = "default_show_antigravity")]
    show_antigravity: bool,
    #[serde(default = "default_show_usage_window")]
    show_session_window: bool,
    #[serde(default = "default_show_usage_window")]
    show_weekly_window: bool,
    #[serde(default)]
    show_drag_handle: bool,
    #[serde(default)]
    enable_codex_mcp: bool,
    #[serde(default = "default_usage_display")]
    usage_display: String,
    #[serde(default = "default_credit_display")]
    credit_display: String,
    #[serde(default = "default_credit_position")]
    credit_position: String,
    #[serde(default = "default_credit_value_mode")]
    credit_value_mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bar_color: Option<String>,
    #[serde(default)]
    alert_threshold_percent: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    alert_thresholds_percent: Option<Vec<u8>>,
    #[serde(default)]
    adaptive_polling: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    notified_quota_windows: Vec<String>,
    /// Read-only fallback to the Claude Desktop app's own sign-in when the
    /// Claude Code CLI sign-in has expired. Settings-file only.
    #[serde(default = "default_claude_desktop_sign_in")]
    use_claude_desktop_sign_in: bool,
}

fn default_claude_desktop_sign_in() -> bool {
    true
}

impl Default for SettingsFile {
    fn default() -> Self {
        Self {
            tray_offset: 0,
            manual_position: None,
            taskbar_index: 0,
            poll_interval_ms: default_poll_interval(),
            language: None,
            last_update_check_unix: None,
            last_codex_exec_refresh_unix: None,
            widget_visible: true,
            show_claude_code: false,
            show_codex: true,
            show_antigravity: false,
            show_session_window: true,
            show_weekly_window: true,
            show_drag_handle: false,
            enable_codex_mcp: false,
            usage_display: new_install_usage_display(),
            credit_display: default_credit_display(),
            credit_position: default_credit_position(),
            credit_value_mode: default_credit_value_mode(),
            bar_color: None,
            alert_threshold_percent: 0,
            alert_thresholds_percent: None,
            adaptive_polling: false,
            notified_quota_windows: Vec::new(),
            use_claude_desktop_sign_in: default_claude_desktop_sign_in(),
        }
    }
}

fn default_poll_interval() -> u32 {
    POLL_15_MIN
}

/// Existing settings files that predate the field keep Remaining, the value
/// they were already displaying; only brand-new installations start Mixed.
fn default_usage_display() -> String {
    "remaining".to_string()
}

fn new_install_usage_display() -> String {
    "mixed".to_string()
}

fn default_credit_display() -> String {
    "always".to_string()
}

fn default_credit_position() -> String {
    "left".to_string()
}

fn default_credit_value_mode() -> String {
    "credits".to_string()
}

/// Presentation only: quota text and bars. Alerts, adaptive
/// polling, Credits selection and MCP always use normalized used/remaining.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UsageDisplayMode {
    /// Provider-specific directions; see `DisplayDirections::MIXED`.
    Mixed,
    Remaining,
    Used,
}

/// `true` shows remaining quota (falling toward 0%) for that provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DisplayDirections {
    claude: bool,
    codex: bool,
    antigravity: bool,
}

impl DisplayDirections {
    /// Default (Mixed): Codex shows Remaining (falling toward 0%), matching
    /// Codex `/status` "% left"; Claude shows Used (rising toward 100%),
    /// matching Claude's "% used". Antigravity keeps the Remaining default.
    const MIXED: Self = Self {
        claude: false,
        codex: true,
        antigravity: true,
    };

    const fn uniform(remaining: bool) -> Self {
        Self {
            claude: remaining,
            codex: remaining,
            antigravity: remaining,
        }
    }
}

impl UsageDisplayMode {
    fn from_setting(value: &str) -> Self {
        if value.eq_ignore_ascii_case("used") {
            Self::Used
        } else if value.eq_ignore_ascii_case("mixed") {
            Self::Mixed
        } else {
            Self::Remaining
        }
    }

    fn directions(self) -> DisplayDirections {
        match self {
            Self::Mixed => DisplayDirections::MIXED,
            Self::Remaining => DisplayDirections::uniform(true),
            Self::Used => DisplayDirections::uniform(false),
        }
    }

    fn as_setting(self) -> &'static str {
        match self {
            Self::Mixed => "mixed",
            Self::Remaining => "remaining",
            Self::Used => "used",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CreditDisplayMode {
    Always,
    WhenNeeded,
    Off,
}

impl CreditDisplayMode {
    fn from_setting(value: &str) -> Self {
        if value.eq_ignore_ascii_case("when_needed") {
            Self::WhenNeeded
        } else if value.eq_ignore_ascii_case("off") {
            Self::Off
        } else {
            Self::Always
        }
    }

    fn as_setting(self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::WhenNeeded => "when_needed",
            Self::Off => "off",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CreditPosition {
    Left,
    Right,
}

impl CreditPosition {
    fn from_setting(value: &str) -> Self {
        if value.eq_ignore_ascii_case("right") {
            Self::Right
        } else {
            Self::Left
        }
    }

    fn as_setting(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CreditValueMode {
    Credits,
    UsdEstimate,
}

impl CreditValueMode {
    fn from_setting(value: &str) -> Self {
        if value.eq_ignore_ascii_case("usd_estimate") {
            Self::UsdEstimate
        } else {
            Self::Credits
        }
    }

    fn as_setting(self) -> &'static str {
        match self {
            Self::Credits => "credits",
            Self::UsdEstimate => "usd_estimate",
        }
    }
}

fn parse_hex_color(value: &str) -> Option<Color> {
    let hex = value.strip_prefix('#').unwrap_or(value);
    if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(Color::from_hex(hex))
}

fn canonical_hex_color(color: Color) -> String {
    format!("#{:02X}{:02X}{:02X}", color.r, color.g, color.b)
}

fn default_widget_visible() -> bool {
    true
}

fn default_show_claude_code() -> bool {
    false
}

fn default_show_codex() -> bool {
    true
}

fn default_show_antigravity() -> bool {
    false
}

fn default_show_usage_window() -> bool {
    true
}

fn load_settings(claude_code_available: bool) -> SettingsFile {
    let current_path = settings_path();
    let legacy_path = legacy_settings_path();
    let (settings, source) = load_settings_from_paths(&current_path, &legacy_path);
    let settings = normalize_settings(settings);
    let source_path = match source {
        SettingsSource::Current => current_path.display().to_string(),
        SettingsSource::Legacy => legacy_path.display().to_string(),
        SettingsSource::Defaults => format!(
            "no valid settings file (current path {})",
            current_path.display()
        ),
    };
    diagnose::log(format!(
        "MCP preference loaded enabled={} source={} path={}",
        settings.enable_codex_mcp,
        source.label(),
        source_path
    ));
    let (settings, claude_auto_disabled) =
        apply_claude_code_availability(settings, claude_code_available);
    let migrated = source == SettingsSource::Legacy;
    if migrated || claude_auto_disabled {
        save_settings(&settings);
        if migrated {
            diagnose::log(format!(
                "migrated settings from {} to {}",
                legacy_path.display(),
                current_path.display()
            ));
        }
        if claude_auto_disabled {
            diagnose::log(
                "disabled Claude Code monitoring because no CLI credentials are available",
            );
        }
    }
    settings
}

fn apply_claude_code_availability(
    mut settings: SettingsFile,
    claude_code_available: bool,
) -> (SettingsFile, bool) {
    let disabled = settings.show_claude_code && !claude_code_available;
    if disabled {
        settings.show_claude_code = false;
        settings = normalize_settings(settings);
    }
    (settings, disabled)
}

fn migrate_legacy_threshold_keys(notified: &mut Vec<String>, threshold: u8) {
    let mut migrated = Vec::new();
    notified.retain(|key| {
        let Some((prefix, reset)) = key.rsplit_once(':') else {
            return true;
        };
        if prefix.ends_with(":exhausted")
            || prefix.contains(":threshold:")
            || !(reset == "unknown" || reset.parse::<u64>().is_ok())
        {
            return true;
        }
        migrated.push(format!("{prefix}:threshold:{threshold}:{reset}"));
        false
    });
    notified.extend(migrated);
}

fn load_settings_from_paths(
    current_path: &std::path::Path,
    legacy_path: &std::path::Path,
) -> (SettingsFile, SettingsSource) {
    if let Ok(content) = std::fs::read_to_string(current_path) {
        if let Ok(settings) = serde_json::from_str(&content) {
            return (settings, SettingsSource::Current);
        }
    }

    if let Ok(content) = std::fs::read_to_string(legacy_path) {
        if let Ok(settings) = serde_json::from_str(&content) {
            return (settings, SettingsSource::Legacy);
        }
    }

    (SettingsFile::default(), SettingsSource::Defaults)
}

fn start_mcp_if_enabled(
    enabled: bool,
    start: impl FnOnce() -> Result<(), String>,
) -> Result<bool, String> {
    if !enabled {
        return Ok(false);
    }
    start().map(|()| true)
}

fn normalize_settings(mut settings: SettingsFile) -> SettingsFile {
    if !settings.show_claude_code && !settings.show_codex && !settings.show_antigravity {
        settings.show_codex = true;
    }
    if !settings.show_session_window && !settings.show_weekly_window {
        settings.show_session_window = true;
    }
    let legacy_threshold = settings.alert_threshold_percent;
    let thresholds_were_missing = settings.alert_thresholds_percent.is_none();
    let mut thresholds = settings
        .alert_thresholds_percent
        .take()
        .unwrap_or_else(|| match settings.alert_threshold_percent {
            0 => Vec::new(),
            legacy => vec![legacy],
        });
    thresholds.retain(|threshold| matches!(*threshold, 2 | 5 | 10 | 20 | 30));
    thresholds.sort_unstable();
    thresholds.dedup();
    settings.alert_threshold_percent = thresholds.first().copied().unwrap_or(0);
    settings.alert_thresholds_percent = Some(thresholds);
    if thresholds_were_missing && matches!(legacy_threshold, 2 | 5 | 10 | 20 | 30) {
        migrate_legacy_threshold_keys(&mut settings.notified_quota_windows, legacy_threshold);
    }
    if !matches!(
        settings.usage_display.to_ascii_lowercase().as_str(),
        "mixed" | "remaining" | "used"
    ) {
        settings.usage_display = default_usage_display();
    } else {
        settings.usage_display = UsageDisplayMode::from_setting(&settings.usage_display)
            .as_setting()
            .to_string();
    }
    settings.credit_display = CreditDisplayMode::from_setting(&settings.credit_display)
        .as_setting()
        .to_string();
    settings.credit_position = CreditPosition::from_setting(&settings.credit_position)
        .as_setting()
        .to_string();
    settings.credit_value_mode = CreditValueMode::from_setting(&settings.credit_value_mode)
        .as_setting()
        .to_string();
    settings.bar_color = settings
        .bar_color
        .as_deref()
        .and_then(parse_hex_color)
        .map(canonical_hex_color);
    settings.notified_quota_windows.sort();
    settings.notified_quota_windows.dedup();
    settings
}

fn save_settings(settings: &SettingsFile) {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(settings) {
        let _ = std::fs::write(path, json);
    }
}

fn save_state_settings() {
    let state = lock_state();
    if let Some(s) = state.as_ref() {
        save_settings(&SettingsFile {
            tray_offset: s.tray_offset,
            manual_position: Some(s.manual_position),
            taskbar_index: s.taskbar_index,
            poll_interval_ms: s.poll_interval_ms,
            language: s
                .language_override
                .map(|language| language.code().to_string()),
            last_update_check_unix: s.last_update_check_unix,
            last_codex_exec_refresh_unix: s.last_codex_exec_refresh_unix,
            widget_visible: s.widget_visible,
            show_claude_code: s.show_claude_code,
            show_codex: s.show_codex,
            show_antigravity: s.show_antigravity,
            show_session_window: s.show_session_window,
            show_weekly_window: s.show_weekly_window,
            show_drag_handle: s.show_drag_handle,
            enable_codex_mcp: s.enable_codex_mcp,
            usage_display: s.usage_display.as_setting().to_string(),
            credit_display: s.credit_display.as_setting().to_string(),
            credit_position: s.credit_position.as_setting().to_string(),
            credit_value_mode: s.credit_value_mode.as_setting().to_string(),
            bar_color: s.bar_color.clone(),
            alert_threshold_percent: s.alert_thresholds_percent.first().copied().unwrap_or(0),
            alert_thresholds_percent: Some(s.alert_thresholds_percent.clone()),
            adaptive_polling: s.adaptive_polling,
            notified_quota_windows: s.notified_quota_windows.iter().cloned().collect(),
            use_claude_desktop_sign_in: s.claude_desktop_sign_in,
        });
    }
}

fn format_precise_reset_time(resets_at: Option<SystemTime>) -> Option<String> {
    let local = native_interop::system_time_to_local(resets_at?)?;
    Some(format_local_system_time(local))
}

fn format_local_system_time(local: SYSTEMTIME) -> String {
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute
    )
}

fn service_tooltip(
    service: &str,
    session_text: &str,
    weekly_text: &str,
    show_session_window: bool,
    show_weekly_window: bool,
) -> String {
    service_tooltip_with_credit(
        service,
        session_text,
        weekly_text,
        show_session_window,
        show_weekly_window,
        None,
    )
}

fn service_tooltip_with_credit(
    service: &str,
    session_text: &str,
    weekly_text: &str,
    show_session_window: bool,
    show_weekly_window: bool,
    credit: Option<(&str, &str)>,
) -> String {
    let mut parts = Vec::new();
    if show_session_window {
        parts.push(format!("5h {session_text}"));
    }
    if show_weekly_window {
        parts.push(format!("7d {weekly_text}"));
    }
    if let Some((label, value)) = credit.filter(|(_, value)| !value.is_empty()) {
        parts.push(format!("{label} {value}"));
    }
    format!("{service}: {}", parts.join(" | "))
}

fn claude_code_menu_label(
    strings: Strings,
    language: LanguageId,
    claude_code_available: bool,
) -> String {
    if claude_code_available {
        strings.claude_code_model.to_string()
    } else if language == LanguageId::SimplifiedChinese {
        "Claude Code（需登录 CLI）".to_string()
    } else {
        "Claude Code (CLI login required)".to_string()
    }
}

struct QuotaAlert {
    kind: tray_icon::TrayIconKind,
    title: String,
    message: String,
}

#[derive(Clone, Copy)]
enum QuotaAlertType {
    Threshold,
    Exhausted,
}

// Providers can report slightly different absolute reset timestamps for the
// same quota window. Keep those shifts from re-arming an already sent alert.
const RESET_TIME_JITTER_SECONDS: u64 = 5 * 60;

fn collect_low_quota_alerts(state: &mut AppState, data: &AppUsageData) -> Vec<QuotaAlert> {
    if state.alert_thresholds_percent.is_empty() {
        return Vec::new();
    }

    let strings = state.language.strings();
    let mut alerts = Vec::new();
    if state.show_claude_code {
        if let Some(usage) = data.claude_code.as_ref() {
            append_provider_alerts(
                &mut alerts,
                &mut state.notified_quota_windows,
                &state.alert_thresholds_percent,
                state.language,
                tray_icon::TrayIconKind::Claude,
                "claude",
                strings.claude_code_model,
                usage,
                strings,
            );
        }
    }
    if state.show_codex {
        if let Some(usage) = data.codex.as_ref() {
            append_provider_alerts(
                &mut alerts,
                &mut state.notified_quota_windows,
                &state.alert_thresholds_percent,
                state.language,
                tray_icon::TrayIconKind::Codex,
                "codex",
                strings.codex_model,
                usage,
                strings,
            );
        }
    }
    if state.show_antigravity {
        if let Some(usage) = data.antigravity.as_ref() {
            append_provider_alerts(
                &mut alerts,
                &mut state.notified_quota_windows,
                &state.alert_thresholds_percent,
                state.language,
                tray_icon::TrayIconKind::Antigravity,
                "antigravity",
                strings.antigravity_model,
                usage,
                strings,
            );
        }
    }
    alerts
}

#[allow(clippy::too_many_arguments)]
fn append_provider_alerts(
    alerts: &mut Vec<QuotaAlert>,
    notified: &mut BTreeSet<String>,
    thresholds: &[u8],
    language: LanguageId,
    kind: tray_icon::TrayIconKind,
    provider_key: &str,
    provider_label: &str,
    usage: &crate::models::UsageData,
    strings: Strings,
) {
    append_quota_alerts(
        alerts,
        notified,
        thresholds,
        language,
        kind,
        provider_key,
        provider_label,
        "session",
        strings.session_window,
        &usage.session,
    );
    append_quota_alerts(
        alerts,
        notified,
        thresholds,
        language,
        kind,
        provider_key,
        provider_label,
        "weekly",
        strings.weekly_window,
        &usage.weekly,
    );
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
fn append_quota_alert(
    alerts: &mut Vec<QuotaAlert>,
    notified: &mut BTreeSet<String>,
    threshold: u8,
    language: LanguageId,
    kind: tray_icon::TrayIconKind,
    provider_key: &str,
    provider_label: &str,
    window_key: &str,
    window_label: &str,
    section: &crate::models::UsageSection,
) {
    if threshold == 0 {
        return;
    }
    append_quota_alerts(
        alerts,
        notified,
        &[threshold],
        language,
        kind,
        provider_key,
        provider_label,
        window_key,
        window_label,
        section,
    );
}

#[allow(clippy::too_many_arguments)]
fn append_quota_alerts(
    alerts: &mut Vec<QuotaAlert>,
    notified: &mut BTreeSet<String>,
    thresholds: &[u8],
    language: LanguageId,
    kind: tray_icon::TrayIconKind,
    provider_key: &str,
    provider_label: &str,
    window_key: &str,
    window_label: &str,
    section: &crate::models::UsageSection,
) {
    if thresholds.is_empty() {
        return;
    }

    let reset_key = section
        .resets_at
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|value| value.as_secs().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let remaining = poller::remaining_percentage(section.percentage).round() as u8;

    let exhausted_prefix = format!("{provider_key}:{window_key}:exhausted:");
    let (exhausted_key, exhausted_notified) = reconcile_quota_alert_window(
        notified,
        &exhausted_prefix,
        &reset_key,
        QuotaAlertType::Exhausted,
    );

    // A direct jump to 0% is represented by the exhaustion alert only. If the
    // threshold alert was sent on an earlier poll, the independent exhaustion
    // alert is still allowed through here.
    if remaining == 0 {
        for threshold in thresholds {
            let (threshold_key, threshold_notified) =
                threshold_alert_key(notified, provider_key, window_key, *threshold, &reset_key);
            if !threshold_notified {
                notified.insert(threshold_key);
            }
        }
        if exhausted_notified || !notified.insert(exhausted_key) {
            return;
        }
        append_quota_alert_notification(
            alerts,
            language,
            kind,
            provider_label,
            window_label,
            section,
            remaining,
            QuotaAlertType::Exhausted,
        );
        return;
    }

    let mut breached = thresholds
        .iter()
        .filter_map(|threshold| {
            let (key, notified_already) =
                threshold_alert_key(notified, provider_key, window_key, *threshold, &reset_key);
            (remaining <= *threshold && !notified_already).then_some((*threshold, key))
        })
        .collect::<Vec<_>>();
    if let Some((deepest_threshold, deepest_key)) = breached
        .iter()
        .min_by_key(|(threshold, _)| *threshold)
        .cloned()
    {
        if notified.insert(deepest_key) {
            append_quota_alert_notification(
                alerts,
                language,
                kind,
                provider_label,
                window_label,
                section,
                remaining,
                QuotaAlertType::Threshold,
            );
        }
        for (threshold, key) in breached.drain(..) {
            if threshold != deepest_threshold {
                notified.insert(key);
            }
        }
    }
}

fn threshold_alert_key(
    notified: &mut BTreeSet<String>,
    provider_key: &str,
    window_key: &str,
    threshold: u8,
    reset_key: &str,
) -> (String, bool) {
    let prefix = format!("{provider_key}:{window_key}:threshold:{threshold}:");
    let (key, handled) =
        reconcile_quota_alert_window(notified, &prefix, reset_key, QuotaAlertType::Threshold);

    (key, handled)
}

fn reconcile_quota_alert_window(
    notified: &mut BTreeSet<String>,
    prefix: &str,
    reset_key: &str,
    alert_type: QuotaAlertType,
) -> (String, bool) {
    let key = format!("{prefix}{reset_key}");
    let same_window_key = notified.iter().find(|existing| {
        let Some(existing_reset) = existing.strip_prefix(prefix) else {
            return false;
        };
        if !matches_alert_type(existing_reset, alert_type) {
            return false;
        }
        match (reset_key, existing_reset) {
            ("unknown", "unknown") => true,
            (current, previous) => match (current.parse::<u64>(), previous.parse::<u64>()) {
                (Ok(current), Ok(previous)) => {
                    current.abs_diff(previous) <= RESET_TIME_JITTER_SECONDS
                }
                _ => false,
            },
        }
    });
    if let Some(existing) = same_window_key {
        if existing != &key {
            let existing = existing.clone();
            notified.remove(&existing);
            notified.insert(key.clone());
        }
        return (key, true);
    }

    notified.retain(|existing| {
        let Some(existing_reset) = existing.strip_prefix(prefix) else {
            return true;
        };
        !matches_alert_type(existing_reset, alert_type)
    });
    (key, false)
}

fn matches_alert_type(reset_key: &str, alert_type: QuotaAlertType) -> bool {
    match alert_type {
        // Threshold keys predate the separate exhaustion namespace.
        QuotaAlertType::Threshold => !reset_key.starts_with("exhausted:"),
        QuotaAlertType::Exhausted => true,
    }
}

#[allow(clippy::too_many_arguments)]
fn append_quota_alert_notification(
    alerts: &mut Vec<QuotaAlert>,
    language: LanguageId,
    kind: tray_icon::TrayIconKind,
    provider_label: &str,
    window_label: &str,
    section: &crate::models::UsageSection,
    remaining: u8,
    alert_type: QuotaAlertType,
) {
    let reset = format_precise_reset_time(section.resets_at);
    let (title, message) = if language == LanguageId::SimplifiedChinese {
        match alert_type {
            QuotaAlertType::Threshold => (
                format!("{provider_label} 额度提醒"),
                format!(
                    "{window_label}额度仅剩 {remaining}%，重置时间：{}",
                    reset.unwrap_or_else(|| "未知".to_string())
                ),
            ),
            QuotaAlertType::Exhausted => (
                format!("{provider_label} 额度已用尽"),
                format!(
                    "{window_label}额度已用尽，仅剩 0%，重置时间：{}",
                    reset.unwrap_or_else(|| "未知".to_string())
                ),
            ),
        }
    } else {
        match alert_type {
            QuotaAlertType::Threshold => (
                format!("{provider_label} quota alert"),
                format!(
                    "{window_label} quota has {remaining}% remaining. Reset: {}",
                    reset.unwrap_or_else(|| "unknown".to_string())
                ),
            ),
            QuotaAlertType::Exhausted => (
                format!("{provider_label} quota exhausted"),
                format!(
                    "{window_label} quota has 0% remaining. Reset: {}",
                    reset.unwrap_or_else(|| "unknown".to_string())
                ),
            ),
        }
    };
    alerts.push(QuotaAlert {
        kind,
        title,
        message,
    });
}

fn tray_icon_data_from_state() -> Option<tray_icon::TrayIconData> {
    let state = lock_state();
    match state.as_ref() {
        Some(s) if s.last_poll_ok => {
            let mut services = Vec::new();
            let strings = s.language.strings();
            if s.show_claude_code {
                services.push(service_tooltip(
                    strings.claude_code_model,
                    &s.session_text,
                    &s.weekly_text,
                    s.show_session_window,
                    s.show_weekly_window,
                ));
            }
            if s.show_codex {
                services.push(service_tooltip_with_credit(
                    strings.codex_model,
                    &s.codex_session_text,
                    &s.codex_weekly_text,
                    s.show_session_window,
                    s.show_weekly_window,
                    Some((strings.credits, &s.codex_credit_text)),
                ));
            }
            if s.show_antigravity {
                services.push(service_tooltip(
                    strings.antigravity_model,
                    &s.antigravity_session_text,
                    &s.antigravity_weekly_text,
                    s.show_session_window,
                    s.show_weekly_window,
                ));
            }
            Some(tray_icon::TrayIconData {
                tooltip: if services.is_empty() {
                    strings.window_title.to_string()
                } else {
                    services.join("\n")
                },
            })
        }
        Some(s) => {
            let strings = s.language.strings();
            let tooltip = match (s.show_claude_code, s.show_codex, s.show_antigravity) {
                (false, true, false) => strings.codex_window_title,
                (false, false, true) => strings.antigravity_window_title,
                _ => strings.window_title,
            };
            Some(tray_icon::TrayIconData {
                tooltip: tooltip.to_string(),
            })
        }
        None => None,
    }
}

fn sync_tray_icons(hwnd: HWND) {
    let icon = tray_icon_data_from_state();
    tray_icon::sync(hwnd, icon.as_ref());
}

fn toggle_widget_visibility(hwnd: HWND) {
    let new_visible = {
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            s.widget_visible = !s.widget_visible;
            s.widget_visible
        } else {
            return;
        }
    };
    save_state_settings();
    unsafe {
        if new_visible {
            position_at_taskbar();
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            render_layered();
        } else {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
}

fn attach_to_taskbar(hwnd: HWND, requested_index: usize) -> bool {
    let taskbars = native_interop::find_taskbars();
    if taskbars.is_empty() {
        diagnose::log("taskbar not found; using fallback popup window");
        return false;
    }

    let index = requested_index.min(taskbars.len().saturating_sub(1));
    let taskbar = taskbars[index];
    diagnose::log(format!(
        "taskbar selected index={index} count={} hwnd={:?} rect=({}, {}, {}, {})",
        taskbars.len(),
        taskbar.hwnd,
        taskbar.rect.left,
        taskbar.rect.top,
        taskbar.rect.right,
        taskbar.rect.bottom
    ));

    let old_hook = {
        let mut state = lock_state();
        state.as_mut().and_then(|s| s.win_event_hook.take())
    };
    if let Some(hook) = old_hook {
        native_interop::unhook_win_event(hook);
    }

    native_interop::embed_in_taskbar(hwnd, taskbar.hwnd);

    let tray_notify = native_interop::find_child_window(taskbar.hwnd, "TrayNotifyWnd");
    if tray_notify.is_some() {
        diagnose::log("TrayNotifyWnd found");
    } else {
        diagnose::log("TrayNotifyWnd not found");
    }

    // Watch the taskbar's own thread so taskbars without a TrayNotifyWnd (a
    // secondary taskbar's nested clock) still report layout changes. The
    // callback filters to this taskbar and its descendants.
    let thread_id = native_interop::get_window_thread_id(taskbar.hwnd);
    let hook = native_interop::set_tray_event_hook(thread_id, on_tray_location_changed);
    if hook.is_some() {
        diagnose::log("tray event hook installed");
    } else {
        diagnose::log("tray event hook could not be installed");
    }

    let mut state = lock_state();
    if let Some(s) = state.as_mut() {
        s.taskbar_hwnd = Some(taskbar.hwnd);
        s.tray_notify_hwnd = tray_notify;
        s.win_event_hook = hook;
        s.taskbar_index = index;
        s.embedded = true;
    }
    true
}

fn taskbar_at_point(pt: POINT) -> Option<(usize, native_interop::TaskbarWindow)> {
    native_interop::find_taskbars()
        .into_iter()
        .enumerate()
        .find(|(_, taskbar)| {
            pt.x >= taskbar.rect.left
                && pt.x < taskbar.rect.right
                && pt.y >= taskbar.rect.top
                && pt.y < taskbar.rect.bottom
        })
}

/// Left edge of the notification area (Show hidden icons, tray icons, clock),
/// shared by every placement path. Taskbars without a discoverable
/// notification window reserve a scaled clock area instead of assuming the
/// right edge is free.
fn tray_left_for_taskbar(taskbar_hwnd: HWND, taskbar_rect: RECT) -> i32 {
    let notification = native_interop::taskbar_notification_rect(taskbar_hwnd);
    let reserve = if notification.is_some() {
        0
    } else {
        taskbar_layout::fallback_notification_width(
            taskbar_rect.bottom - taskbar_rect.top,
            native_interop::primary_notification_area(),
        )
    };
    taskbar_layout::notification_boundary(taskbar_rect, notification, reserve)
}

fn position_anchor_left(
    taskbar_rect: RECT,
    tray_left: i32,
    occupied: &[RECT],
    manual: bool,
) -> i32 {
    if manual {
        tray_left
    } else {
        taskbar_layout::safe_anchor_left(taskbar_rect, tray_left, occupied)
    }
}

fn offset_for_drop_point(
    taskbar_hwnd: HWND,
    taskbar_rect: RECT,
    pt: POINT,
    drag_start_client_x: i32,
) -> i32 {
    let tray_left = tray_left_for_taskbar(taskbar_hwnd, taskbar_rect);
    let width = total_widget_width();
    let offset = taskbar_layout::offset_for_left(tray_left, width, pt.x - drag_start_client_x);
    taskbar_layout::compute_placement(taskbar_rect, tray_left, width, sc(WIDGET_HEIGHT), offset)
        .offset
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn update_check_interval() -> Duration {
    Duration::from_secs(24 * 60 * 60)
}

fn auto_update_check_due(last_update_check_unix: Option<u64>) -> bool {
    let Some(last_update_check_unix) = last_update_check_unix else {
        return true;
    };

    now_unix_secs().saturating_sub(last_update_check_unix) >= update_check_interval().as_secs()
}

fn schedule_auto_update_check(hwnd: HWND) {
    let delay_ms = {
        let state = lock_state();
        let Some(s) = state.as_ref() else {
            return;
        };

        if auto_update_check_due(s.last_update_check_unix) {
            None
        } else {
            let elapsed = now_unix_secs().saturating_sub(s.last_update_check_unix.unwrap_or(0));
            let remaining_secs = update_check_interval().as_secs().saturating_sub(elapsed);
            Some((remaining_secs.saturating_mul(1000)).min(u32::MAX as u64) as u32)
        }
    };

    unsafe {
        let _ = KillTimer(hwnd, TIMER_UPDATE_CHECK);
        if let Some(delay_ms) = delay_ms {
            SetTimer(hwnd, TIMER_UPDATE_CHECK, delay_ms.max(1), None);
        }
    }
}

fn refresh_usage_texts(state: &mut AppState) {
    if !state.last_poll_ok {
        return;
    }

    let strings = state.language.strings();
    let is_simplified_chinese = state.language == LanguageId::SimplifiedChinese;
    let directions = state.usage_display.directions();
    let Some(data) = state.data.as_ref() else {
        return;
    };

    if let Some(claude_code) = data.claude_code.as_ref() {
        state.session_text = poller::format_line(
            &claude_code.session,
            strings,
            is_simplified_chinese,
            directions.claude,
            poller::UsageWindowKind::Session,
        );
        state.weekly_text = poller::format_line(
            &claude_code.weekly,
            strings,
            is_simplified_chinese,
            directions.claude,
            poller::UsageWindowKind::Weekly,
        );
    } else if state.show_claude_code {
        let error = state.provider_health[CLAUDE].error;
        state.session_text = missing_usage_text(&state.session_text, error, state.language);
        state.weekly_text = missing_usage_text(&state.weekly_text, error, state.language);
    }

    if let Some(codex) = data.codex.as_ref() {
        state.codex_credit_text = codex
            .credits
            .as_ref()
            .map(|balance| format_credit_value(balance, state.credit_value_mode))
            .unwrap_or_default();
        state.codex_session_text = poller::format_codex_line(
            &codex.session,
            strings,
            is_simplified_chinese,
            directions.codex,
            poller::UsageWindowKind::Session,
        );
        state.codex_weekly_text = poller::format_line(
            &codex.weekly,
            strings,
            is_simplified_chinese,
            directions.codex,
            poller::UsageWindowKind::Weekly,
        );
    } else if state.show_codex {
        let error = state.provider_health[CODEX].error;
        state.codex_credit_text.clear();
        state.codex_session_text =
            missing_usage_text(&state.codex_session_text, error, state.language);
        state.codex_weekly_text =
            missing_usage_text(&state.codex_weekly_text, error, state.language);
    }

    if let Some(antigravity) = data.antigravity.as_ref() {
        state.antigravity_session_text = poller::format_line(
            &antigravity.session,
            strings,
            is_simplified_chinese,
            directions.antigravity,
            poller::UsageWindowKind::Session,
        );
        state.antigravity_weekly_text =
            if antigravity.weekly.resets_at.is_none() && antigravity.weekly.percentage == 0.0 {
                "--".to_string()
            } else {
                poller::format_line(
                    &antigravity.weekly,
                    strings,
                    is_simplified_chinese,
                    directions.antigravity,
                    poller::UsageWindowKind::Weekly,
                )
            };
    } else if state.show_antigravity {
        let error = state.provider_health[2].error;
        state.antigravity_session_text =
            missing_usage_text(&state.antigravity_session_text, error, state.language);
        state.antigravity_weekly_text =
            missing_usage_text(&state.antigravity_weekly_text, error, state.language);
    }
    let now = std::time::SystemTime::now();
    sync_provider_percents(state, now);
    // Age against each provider's own cadence, never against an escalating
    // error delay or another provider's backoff.
    let base_interval = effective_poll_interval_ms(state);
    let unknown = [
        [state.session_percent, state.weekly_percent],
        [state.codex_session_percent, state.codex_weekly_percent],
        [
            state.antigravity_session_percent,
            state.antigravity_weekly_percent,
        ],
    ]
    .map(|row| row.map(f64::is_nan));
    for (id, session, weekly) in [
        (0, &mut state.session_text, &mut state.weekly_text),
        (
            1,
            &mut state.codex_session_text,
            &mut state.codex_weekly_text,
        ),
        (
            2,
            &mut state.antigravity_session_text,
            &mut state.antigravity_weekly_text,
        ),
    ] {
        let interval = provider_schedule::provider_interval_ms(id, base_interval);
        let stale = state.provider_health[id].stale(now, interval);
        for (text, unknown) in [(session, unknown[id][0]), (weekly, unknown[id][1])] {
            *text = if unknown {
                // The old value predates a reset that has passed: unknown,
                // never a guessed post-reset value.
                if stale { "!--" } else { "--" }.to_string()
            } else {
                crate::provider_health::stale_text(text, stale)
            };
        }
    }
}

/// Bar values follow the shown data; a window whose reset passed after the
/// observation is unknown (NaN draws an empty track in either direction).
fn sync_provider_percents(state: &mut AppState, now: SystemTime) {
    for id in 0..provider_schedule::PROVIDERS {
        let updated = state.provider_health[id].updated_at();
        let values = state
            .data
            .as_ref()
            .and_then(|data| provider_usage(data, id))
            .map(|usage| {
                [&usage.session, &usage.weekly].map(|section| {
                    if crate::provider_health::window_reset_since(section.resets_at, updated, now) {
                        f64::NAN
                    } else {
                        section.percentage
                    }
                })
            })
            .unwrap_or([0.0, 0.0]);
        let (session, weekly) = match id {
            CLAUDE => (&mut state.session_percent, &mut state.weekly_percent),
            CODEX => (
                &mut state.codex_session_percent,
                &mut state.codex_weekly_percent,
            ),
            _ => (
                &mut state.antigravity_session_percent,
                &mut state.antigravity_weekly_percent,
            ),
        };
        (*session, *weekly) = (values[0], values[1]);
    }
}

/// Providers whose shown values predate a reset that has now passed and
/// that are healthy; failing providers already retry on their own schedule.
fn reset_poll_pending(state: &AppState, now: SystemTime) -> [bool; provider_schedule::PROVIDERS] {
    std::array::from_fn(|id| {
        state.provider_health[id].error.is_none()
            && state
                .data
                .as_ref()
                .and_then(|data| provider_usage(data, id))
                .is_some_and(|usage| {
                    let updated = state.provider_health[id].updated_at();
                    [&usage.session, &usage.weekly].iter().any(|section| {
                        crate::provider_health::window_reset_since(section.resets_at, updated, now)
                    })
                })
    })
}

/// Restore each enabled provider's last snapshot for its current account
/// before the first poll, shown stale (`!`) until a successful refresh. For
/// Claude, Claude Code's own cached reading wins when it is newer.
fn restore_snapshots() {
    let enabled = {
        let state = lock_state();
        let Some(state) = state.as_ref() else {
            return;
        };
        enabled_providers(state)
    };
    // File reads happen outside the state lock.
    let restored: Vec<_> = (0..provider_schedule::PROVIDERS)
        .filter(|&id| enabled[id])
        .filter_map(|id| {
            let scope = poller::account_scope(id)?;
            let persisted = quota_cache::load(id, &scope);
            let mut best = persisted
                .clone()
                .map(|snapshot| (snapshot, poller::DataSource::Restored));
            if id == CLAUDE {
                if let Some((data, updated_at)) = crate::claude_local::read().cached_usage {
                    if best.as_ref().is_none_or(|(s, _)| updated_at > s.updated_at) {
                        best = Some((
                            quota_cache::Snapshot { data, updated_at },
                            poller::DataSource::ClaudeCodeCache,
                        ));
                    }
                }
            }
            best.map(|(snapshot, source)| (id, scope, snapshot, source, persisted))
        })
        .collect();
    if restored.is_empty() {
        diagnose::log("no persisted quota snapshots to restore");
        return;
    }

    let mut guard = lock_state();
    let Some(state) = guard.as_mut() else {
        return;
    };
    let now = SystemTime::now();
    for (id, scope, snapshot, source, persisted) in restored {
        diagnose::log(format!(
            "restored {} snapshot source={} age_s={}; shown stale until refreshed",
            poller::provider_name(id),
            source.name(),
            now.duration_since(snapshot.updated_at)
                .unwrap_or_default()
                .as_secs()
        ));
        state.provider_health[id].restore(scope, snapshot.updated_at);
        state.data_sources[id] = poller::DataSource::Restored;
        state.persisted[id] = persisted;
        codex_mcp::publish_observation(
            id,
            snapshot.data.clone(),
            snapshot.updated_at,
            poller::DataSource::Restored,
            false,
        );
        *provider_usage_mut(state.data.get_or_insert_with(Default::default), id) =
            Some(snapshot.data);
    }
    state.last_poll_ok = true;
    refresh_usage_texts(state);
}

/// Text for an enabled provider without usable data: its error label once it
/// has reported, otherwise the current placeholder (e.g. `...` while pending).
fn missing_usage_text(
    current: &str,
    error: Option<poller::PollError>,
    language: LanguageId,
) -> String {
    match error {
        Some(error) => poll_error_display_label(error, language).to_string(),
        None if current.contains('%') => "!".to_string(),
        None => current.to_string(),
    }
}

fn set_window_title(hwnd: HWND, strings: Strings) {
    unsafe {
        let title = native_interop::wide_str(strings.window_title);
        let _ = SetWindowTextW(hwnd, PCWSTR::from_raw(title.as_ptr()));
    }
}

fn show_info_message(hwnd: HWND, title: &str, message: &str) {
    unsafe {
        let title_wide = native_interop::wide_str(title);
        let message_wide = native_interop::wide_str(message);
        let _ = MessageBoxW(
            hwnd,
            PCWSTR::from_raw(message_wide.as_ptr()),
            PCWSTR::from_raw(title_wide.as_ptr()),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

fn show_error_message(hwnd: HWND, title: &str, message: &str) {
    unsafe {
        let title_wide = native_interop::wide_str(title);
        let message_wide = native_interop::wide_str(message);
        let _ = MessageBoxW(
            hwnd,
            PCWSTR::from_raw(message_wide.as_ptr()),
            PCWSTR::from_raw(title_wide.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

fn show_update_prompt(hwnd: HWND, strings: Strings, release: &ReleaseDescriptor) -> bool {
    let message = strings
        .update_prompt_now
        .replace("{version}", &release.latest_version);

    unsafe {
        let title_wide = native_interop::wide_str(strings.update_available);
        let message_wide = native_interop::wide_str(&message);
        MessageBoxW(
            hwnd,
            PCWSTR::from_raw(message_wide.as_ptr()),
            PCWSTR::from_raw(title_wide.as_ptr()),
            MB_YESNO | MB_ICONQUESTION,
        ) == IDYES
    }
}

fn apply_language_to_state(state: &mut AppState, language_override: Option<LanguageId>) {
    state.language_override = language_override;
    state.language = localization::resolve_language(language_override);
    set_window_title(state.hwnd.to_hwnd(), state.language.strings());
    refresh_usage_texts(state);
}

fn update_language_change() -> bool {
    let mut state = lock_state();
    let Some(app_state) = state.as_mut() else {
        return false;
    };

    if app_state.language_override.is_some() {
        return false;
    }

    let new_language = localization::detect_system_language();
    if new_language == app_state.language {
        return false;
    }

    apply_language_to_state(app_state, None);
    true
}

fn version_action_label(strings: Strings, status: &UpdateStatus) -> String {
    match status {
        UpdateStatus::Idle => strings.check_for_updates.to_string(),
        UpdateStatus::Checking => strings.checking_for_updates.to_string(),
        UpdateStatus::Applying => strings.applying_update.to_string(),
        UpdateStatus::UpToDate => strings.up_to_date_short.to_string(),
        UpdateStatus::Available(release) => {
            format!("{} v{}", strings.update_to, release.latest_version)
        }
    }
}

fn begin_update_check(hwnd: HWND, interactive: bool) {
    let send_hwnd = SendHwnd::from_hwnd(hwnd);
    let strings = {
        let mut state = lock_state();
        let Some(app_state) = state.as_mut() else {
            return;
        };

        if matches!(
            app_state.update_status,
            UpdateStatus::Checking | UpdateStatus::Applying
        ) {
            if interactive {
                show_info_message(
                    hwnd,
                    app_state.language.strings().updates,
                    app_state.language.strings().update_in_progress,
                );
            }
            return;
        }

        app_state.update_status = UpdateStatus::Checking;
        app_state.language.strings()
    };

    std::thread::spawn(move || {
        let hwnd = send_hwnd.to_hwnd();
        let checked_at = now_unix_secs();
        match updater::check_for_updates() {
            Ok(UpdateCheckResult::UpToDate) => {
                {
                    let mut state = lock_state();
                    if let Some(s) = state.as_mut() {
                        s.update_status = UpdateStatus::UpToDate;
                        s.last_update_check_unix = Some(checked_at);
                    }
                }
                save_state_settings();
                if interactive {
                    show_info_message(hwnd, strings.updates, strings.up_to_date);
                }
                unsafe {
                    let _ = PostMessageW(hwnd, WM_APP_UPDATE_CHECK_COMPLETE, WPARAM(0), LPARAM(0));
                }
            }
            Ok(UpdateCheckResult::Available(release)) => {
                {
                    let mut state = lock_state();
                    if let Some(s) = state.as_mut() {
                        s.update_status = UpdateStatus::Available(release.clone());
                        s.last_update_check_unix = Some(checked_at);
                    }
                }
                save_state_settings();
                if interactive && show_update_prompt(hwnd, strings, &release) {
                    begin_update_apply(hwnd, release);
                }
                unsafe {
                    let _ = PostMessageW(hwnd, WM_APP_UPDATE_CHECK_COMPLETE, WPARAM(0), LPARAM(0));
                }
            }
            Err(error) => {
                {
                    let mut state = lock_state();
                    if let Some(s) = state.as_mut() {
                        s.update_status = UpdateStatus::Idle;
                        s.last_update_check_unix = Some(checked_at);
                    }
                }
                save_state_settings();
                if interactive {
                    let message = format!("{}.\n\n{}", strings.update_failed, error);
                    show_error_message(hwnd, strings.updates, &message);
                }
                unsafe {
                    let _ = PostMessageW(hwnd, WM_APP_UPDATE_CHECK_COMPLETE, WPARAM(0), LPARAM(0));
                }
            }
        }
    });
}

fn begin_update_apply(hwnd: HWND, release: ReleaseDescriptor) {
    let send_hwnd = SendHwnd::from_hwnd(hwnd);
    let strings = {
        let mut state = lock_state();
        let Some(app_state) = state.as_mut() else {
            return;
        };

        if matches!(
            app_state.update_status,
            UpdateStatus::Checking | UpdateStatus::Applying
        ) {
            show_info_message(
                hwnd,
                app_state.language.strings().updates,
                app_state.language.strings().update_in_progress,
            );
            return;
        }

        app_state.update_status = UpdateStatus::Applying;
        app_state.language.strings()
    };

    std::thread::spawn(move || {
        let hwnd = send_hwnd.to_hwnd();
        match updater::begin_self_update(&release) {
            Ok(()) => unsafe {
                let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
            },
            Err(error) => {
                {
                    let mut state = lock_state();
                    if let Some(s) = state.as_mut() {
                        s.update_status = UpdateStatus::Available(release);
                    }
                }
                let message = format!("{}.\n\n{}", strings.update_failed, error);
                show_error_message(hwnd, strings.updates, &message);
                unsafe {
                    let _ = PostMessageW(hwnd, WM_APP_UPDATE_CHECK_COMPLETE, WPARAM(0), LPARAM(0));
                }
            }
        }
    });
}

const STARTUP_REGISTRY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const STARTUP_REGISTRY_KEY: &str = "CodexUsage";
const LEGACY_STARTUP_REGISTRY_KEY: &str = "ClaudeCodeUsageMonitor";

/// Returns true only if the startup registry value points to this executable.
fn is_startup_enabled() -> bool {
    let Some(reg_value) = read_startup_value(STARTUP_REGISTRY_KEY) else {
        return false;
    };
    let Some(current_exe) = current_exe_path_string() else {
        return false;
    };
    reg_value.eq_ignore_ascii_case(&current_exe)
}

fn current_exe_path_string() -> Option<String> {
    unsafe {
        let mut exe_buf = [0u16; 260];
        let len = GetModuleFileNameW(None, &mut exe_buf) as usize;
        (len > 0).then(|| String::from_utf16_lossy(&exe_buf[..len]))
    }
}

fn read_startup_value(key: &str) -> Option<String> {
    unsafe {
        let path = native_interop::wide_str(STARTUP_REGISTRY_PATH);
        let key_name = native_interop::wide_str(key);

        let mut hkey = HKEY::default();
        let result = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR::from_raw(path.as_ptr()),
            0,
            KEY_READ,
            &mut hkey,
        );
        if result.is_err() {
            return None;
        }

        // Query the size of the value
        let mut data_size: u32 = 0;
        let result = RegQueryValueExW(
            hkey,
            PCWSTR::from_raw(key_name.as_ptr()),
            None,
            None,
            None,
            Some(&mut data_size),
        );
        if result.is_err() || data_size == 0 {
            let _ = RegCloseKey(hkey);
            return None;
        }

        // Read the value
        let mut buf = vec![0u8; data_size as usize];
        let result = RegQueryValueExW(
            hkey,
            PCWSTR::from_raw(key_name.as_ptr()),
            None,
            None,
            Some(buf.as_mut_ptr()),
            Some(&mut data_size),
        );
        let _ = RegCloseKey(hkey);
        if result.is_err() {
            return None;
        }

        // Convert the registry value (UTF-16) to a string
        let wide_slice =
            std::slice::from_raw_parts(buf.as_ptr() as *const u16, data_size as usize / 2);
        Some(
            String::from_utf16_lossy(wide_slice)
                .trim_end_matches('\0')
                .to_string(),
        )
    }
}

fn delete_startup_value(key: &str) {
    unsafe {
        let path = native_interop::wide_str(STARTUP_REGISTRY_PATH);
        let key_name = native_interop::wide_str(key);
        let mut hkey = HKEY::default();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR::from_raw(path.as_ptr()),
            0,
            KEY_SET_VALUE,
            &mut hkey,
        )
        .is_ok()
        {
            let _ = RegDeleteValueW(hkey, PCWSTR::from_raw(key_name.as_ptr()));
            let _ = RegCloseKey(hkey);
        }
    }
}

fn migrate_legacy_startup_entry() {
    let legacy_exists = read_startup_value(LEGACY_STARTUP_REGISTRY_KEY).is_some();
    let current_exists = read_startup_value(STARTUP_REGISTRY_KEY).is_some();
    if !legacy_exists {
        return;
    }

    if should_write_migrated_startup(legacy_exists, current_exists) {
        set_startup_enabled(true);
    }

    if read_startup_value(STARTUP_REGISTRY_KEY).is_some() {
        delete_startup_value(LEGACY_STARTUP_REGISTRY_KEY);
        diagnose::log("migrated legacy startup registry entry to CodexUsage");
    }
}

fn should_write_migrated_startup(legacy_exists: bool, current_exists: bool) -> bool {
    legacy_exists && !current_exists
}

fn set_startup_enabled(enable: bool) {
    unsafe {
        let path = native_interop::wide_str(STARTUP_REGISTRY_PATH);

        let mut hkey = HKEY::default();
        let result = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR::from_raw(path.as_ptr()),
            0,
            KEY_SET_VALUE,
            &mut hkey,
        );
        if result.is_err() {
            return;
        }

        let key_name = native_interop::wide_str(STARTUP_REGISTRY_KEY);

        if enable {
            let mut exe_buf = [0u16; 260];
            let len = GetModuleFileNameW(None, &mut exe_buf) as usize;
            if len > 0 {
                // Write the wide string including null terminator
                let byte_len = ((len + 1) * 2) as u32;
                let _ = RegSetValueExW(
                    hkey,
                    PCWSTR::from_raw(key_name.as_ptr()),
                    0,
                    REG_SZ,
                    Some(std::slice::from_raw_parts(
                        exe_buf.as_ptr() as *const u8,
                        byte_len as usize,
                    )),
                );
            }
        } else {
            let _ = RegDeleteValueW(hkey, PCWSTR::from_raw(key_name.as_ptr()));
            let legacy_key_name = native_interop::wide_str(LEGACY_STARTUP_REGISTRY_KEY);
            let _ = RegDeleteValueW(hkey, PCWSTR::from_raw(legacy_key_name.as_ptr()));
        }

        let _ = RegCloseKey(hkey);
    }
}

// Dimensions matching the C# version
const SEGMENT_W: i32 = 10;
const SEGMENT_H: i32 = 13;
const SEGMENT_GAP: i32 = 1;
const SEGMENT_COUNT: i32 = 10;

const DRAG_HANDLE_HIT_W: i32 = 7;
const DRAG_GRIP_DOT_SIZE: i32 = 2;
const DRAG_GRIP_COLUMN_GAP: i32 = 1;
const DRAG_GRIP_ROW_GAP: i32 = 1;
const LABEL_WIDTH: i32 = 18;
const TEXT_WIDTH_FALLBACK: i32 = 62;
const SIMPLIFIED_CHINESE_LABEL_WIDTH: i32 = 20;
const SIMPLIFIED_CHINESE_TEXT_WIDTH_FALLBACK: i32 = 126;
const RIGHT_MARGIN: i32 = 6;
const HORIZONTAL_GUTTER: i32 = 4;
const CREDIT_TEXT_FALLBACK_WIDTH: i32 = 60;
const CREDIT_VERTICAL_GAP: i32 = 2;
const WIDGET_HEIGHT: i32 = 46;

fn drag_handle_reserved_width(show_drag_handle: bool) -> i32 {
    if show_drag_handle {
        sc(DRAG_HANDLE_HIT_W)
    } else {
        0
    }
}

fn is_drag_handle_point(show_drag_handle: bool, client_x: i32, client_y: i32) -> bool {
    if !show_drag_handle {
        return false;
    }
    let divider_h = sc(25);
    let divider_top = (sc(WIDGET_HEIGHT) - divider_h) / 2;
    client_x >= 0
        && client_x < sc(DRAG_HANDLE_HIT_W)
        && client_y >= divider_top
        && client_y < divider_top + divider_h
}

fn cursor_is_on_drag_handle(hwnd: HWND) -> bool {
    let show_drag_handle = {
        let state = lock_state();
        state.as_ref().map(|s| s.show_drag_handle).unwrap_or(false)
    };
    unsafe {
        let mut pt = POINT::default();
        if GetCursorPos(&mut pt).is_err() || !ScreenToClient(hwnd, &mut pt).as_bool() {
            return false;
        }
        is_drag_handle_point(show_drag_handle, pt.x, pt.y)
    }
}

fn active_model_count(show_claude_code: bool, show_codex: bool, show_antigravity: bool) -> i32 {
    (show_claude_code as i32 + show_codex as i32 + show_antigravity as i32).max(1)
}

fn row_bar_segment_count(active_models: i32) -> i32 {
    match active_models {
        1 => SEGMENT_COUNT,
        2 => 5,
        _ => 4,
    }
}

fn measure_text_width(text: &str) -> Option<i32> {
    unsafe {
        let hdc = GetDC(HWND::default());
        if hdc.is_invalid() {
            return None;
        }

        let font_name = native_interop::wide_str("Segoe UI");
        let font = CreateFontW(
            sc(-12),
            0,
            0,
            0,
            FW_MEDIUM.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_TT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            CLEARTYPE_QUALITY.0 as u32,
            (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            PCWSTR::from_raw(font_name.as_ptr()),
        );
        if font.is_invalid() {
            ReleaseDC(HWND::default(), hdc);
            return None;
        }

        let old_font = SelectObject(hdc, font);
        let text_wide: Vec<u16> = text.encode_utf16().collect();
        let mut size = SIZE::default();
        let measured = GetTextExtentPoint32W(hdc, &text_wide, &mut size).as_bool();
        SelectObject(hdc, old_font);
        let _ = DeleteObject(font);
        ReleaseDC(HWND::default(), hdc);

        measured
            .then(|| logical_width_from_physical(size.cx, CURRENT_DPI.load(Ordering::Relaxed)))?
    }
}

fn logical_width_from_physical(physical_width: i32, dpi: u32) -> Option<i32> {
    if physical_width <= 0 || dpi == 0 {
        return None;
    }
    Some(((physical_width as f64 * 96.0 / dpi as f64).ceil() as i32).max(1))
}

fn measured_column_width(texts: &[&str], fallback: i32) -> i32 {
    texts
        .iter()
        .filter_map(|text| measure_text_width(text))
        .max()
        .unwrap_or(fallback)
        .max(1)
}

fn quota_text_width_fallback(language: LanguageId) -> i32 {
    if language == LanguageId::SimplifiedChinese {
        SIMPLIFIED_CHINESE_TEXT_WIDTH_FALLBACK
    } else {
        TEXT_WIDTH_FALLBACK
    }
}

fn quota_text_width_for(language: LanguageId, texts: &[&str]) -> i32 {
    let widest_digit = texts
        .iter()
        .any(|text| text.chars().any(|ch| ch.is_ascii_digit()))
        .then(widest_measured_quota_digit);
    let stable_shapes: Vec<String> = texts
        .iter()
        .map(|text| {
            let shape = normalize_quota_text_shape(text.trim_start_matches('!'), widest_digit);
            if text.contains('%') {
                format!("!{shape}")
            } else {
                shape
            }
        })
        .collect();
    let stable_shape_refs: Vec<&str> = stable_shapes.iter().map(String::as_str).collect();
    measured_column_width(&stable_shape_refs, quota_text_width_fallback(language)) + 1
}

fn widest_measured_quota_digit() -> char {
    let dpi = CURRENT_DPI.load(Ordering::Relaxed);
    let mut cached = QUOTA_WIDEST_DIGIT_CACHE
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let Some((cached_dpi, digit)) = *cached {
        if cached_dpi == dpi {
            return digit;
        }
    }

    let widest = ('0'..='9')
        .max_by_key(|digit| measure_text_width(&digit.to_string()).unwrap_or(0))
        .unwrap_or('8');
    *cached = Some((dpi, widest));
    widest
}

fn normalize_quota_text_shape(text: &str, widest_digit: Option<char>) -> String {
    let Some(widest_digit) = widest_digit else {
        return text.to_string();
    };
    text.chars()
        .map(|ch| {
            if ch.is_ascii_digit() {
                widest_digit
            } else {
                ch
            }
        })
        .collect()
}

fn usage_label_width(language: LanguageId) -> i32 {
    let strings = language.strings();
    let label_fallback = if language == LanguageId::SimplifiedChinese {
        SIMPLIFIED_CHINESE_LABEL_WIDTH
    } else {
        LABEL_WIDTH
    };
    measured_column_width(
        &[strings.session_window, strings.weekly_window],
        label_fallback,
    )
}

fn usage_layout_widths(language: LanguageId, quota_texts: &[&str]) -> (i32, i32) {
    (
        usage_label_width(language),
        quota_text_width_for(language, quota_texts),
    )
}

fn credit_layout_widths(strings: Strings, current_value: &str) -> (i32, i32) {
    let header_width = measured_column_width(&[strings.credits], CREDIT_TEXT_FALLBACK_WIDTH);
    let actual_value_width = if current_value.is_empty() {
        header_width
    } else {
        measured_column_width(&[current_value], CREDIT_TEXT_FALLBACK_WIDTH)
    };
    (header_width, actual_value_width)
}

fn credit_outer_width(header_width: i32) -> i32 {
    header_width + HORIZONTAL_GUTTER
}

fn credit_outward_overflow(header_width: i32, value_width: i32) -> i32 {
    value_width.saturating_sub(header_width).max(0)
}

fn credit_value_x(
    header_x: i32,
    header_width: i32,
    value_width: i32,
    credit_position: CreditPosition,
) -> i32 {
    if value_width <= header_width {
        header_x + sc((header_width - value_width) / 2)
    } else if credit_position == CreditPosition::Left {
        header_x + sc(header_width - value_width)
    } else {
        header_x
    }
}

fn credit_panel_y_positions(height: i32) -> (i32, i32) {
    let segment_height = sc(SEGMENT_H);
    let gap = sc(CREDIT_VERTICAL_GAP);
    let stack_height = segment_height * 2 + gap;
    let header_y = (height - stack_height) / 2;
    (header_y, header_y + segment_height + gap)
}

fn quota_area_width_for(active_models: i32, language: LanguageId, text_width: i32) -> i32 {
    let bar_segments = row_bar_segment_count(active_models);
    let label_width = usage_label_width(language);
    let model_width = (sc(SEGMENT_W) + sc(SEGMENT_GAP)) * bar_segments - sc(SEGMENT_GAP)
        + sc(HORIZONTAL_GUTTER)
        + sc(text_width);

    sc(label_width)
        + sc(HORIZONTAL_GUTTER)
        + model_width * active_models
        + sc(HORIZONTAL_GUTTER) * (active_models - 1)
}

fn widget_content_positions_for(
    active_models: i32,
    language: LanguageId,
    show_drag_handle: bool,
    credit_panel_visible: bool,
    credit_position: CreditPosition,
    _credit_value_mode: CreditValueMode,
    credit_text: &str,
    text_width: i32,
) -> (i32, Option<i32>) {
    let mut base_content_x = drag_handle_reserved_width(show_drag_handle) + sc(HORIZONTAL_GUTTER);
    if !credit_panel_visible {
        return (base_content_x, None);
    }

    let (header_width, value_width) = credit_layout_widths(language.strings(), credit_text);
    let overflow = sc(credit_outward_overflow(header_width, value_width));
    if credit_position == CreditPosition::Left {
        // The window grows left under right-edge anchoring. Offset the quota
        // content by the same amount so its screen position remains fixed;
        // the outermost drag grip stays at x=0.
        base_content_x += overflow;
    }
    let quota_area_width = quota_area_width_for(active_models, language, text_width);
    match credit_position {
        CreditPosition::Left => (
            base_content_x + sc(credit_outer_width(header_width)),
            Some(base_content_x),
        ),
        CreditPosition::Right => (
            base_content_x,
            Some(base_content_x + quota_area_width + sc(HORIZONTAL_GUTTER)),
        ),
    }
}

fn usage_percent_for_display(display_remaining: bool, used_percentage: f64) -> f64 {
    if display_remaining {
        poller::remaining_percentage(used_percentage)
    } else {
        used_percentage.clamp(0.0, 100.0)
    }
}

fn format_credit_balance(balance: &CreditBalance) -> String {
    match balance {
        CreditBalance::Amount(amount) => format!("{:.0}", amount.round()),
        CreditBalance::Unlimited => "∞".to_string(),
    }
}

fn format_credit_value(balance: &CreditBalance, mode: CreditValueMode) -> String {
    match (balance, mode) {
        (CreditBalance::Amount(amount), CreditValueMode::Credits) => {
            format_credit_balance(&CreditBalance::Amount(*amount))
        }
        (CreditBalance::Amount(amount), CreditValueMode::UsdEstimate) => {
            format!("~${:.2}", amount * 0.04)
        }
        (CreditBalance::Unlimited, _) => "∞".to_string(),
    }
}

fn codex_credit_panel_visible(
    show_codex: bool,
    display: CreditDisplayMode,
    data: Option<&AppUsageData>,
) -> bool {
    if !show_codex {
        return false;
    }
    let Some(codex) = data.and_then(|data| data.codex.as_ref()) else {
        return false;
    };
    if codex.credits.is_none() {
        return false;
    }

    match display {
        CreditDisplayMode::Always => true,
        CreditDisplayMode::WhenNeeded => codex_included_usage_exhausted(codex),
        CreditDisplayMode::Off => false,
    }
}

fn codex_included_usage_exhausted(codex: &crate::models::UsageData) -> bool {
    poller::remaining_percentage(codex.session.percentage).round() == 0.0
        || poller::remaining_percentage(codex.weekly.percentage).round() == 0.0
}

fn credit_panel_reason(
    show_codex: bool,
    display: CreditDisplayMode,
    data: Option<&AppUsageData>,
) -> &'static str {
    if !show_codex || display == CreditDisplayMode::Off {
        return "disabled";
    }
    let Some(codex) = data.and_then(|data| data.codex.as_ref()) else {
        return "codex_unavailable";
    };
    if display == CreditDisplayMode::WhenNeeded && !codex_included_usage_exhausted(codex) {
        return "included_usage_available";
    }
    match codex.credits.as_ref() {
        Some(CreditBalance::Amount(amount)) if *amount > 0.0 => "finite_credits_available",
        Some(CreditBalance::Unlimited) => "unlimited_credits_available",
        Some(CreditBalance::Amount(_)) => "credits_exhausted",
        None => "credits_unknown",
    }
}

fn total_widget_width_for(
    active_models: i32,
    language: LanguageId,
    show_drag_handle: bool,
    credit_panel_visible: bool,
    _credit_value_mode: CreditValueMode,
    credit_text: &str,
    text_width: i32,
) -> i32 {
    let quota_area_width = quota_area_width_for(active_models, language, text_width);

    let credits_width = if credit_panel_visible {
        let (header_width, value_width) = credit_layout_widths(language.strings(), credit_text);
        sc(credit_outer_width(header_width) + credit_outward_overflow(header_width, value_width))
    } else {
        0
    };

    credits_width
        + drag_handle_reserved_width(show_drag_handle)
        + sc(HORIZONTAL_GUTTER)
        + quota_area_width
        + sc(RIGHT_MARGIN)
}

fn total_widget_width_for_state(state: &AppState) -> i32 {
    let quota_texts = quota_texts_for_state(state);
    total_widget_width_for(
        active_model_count(
            state.show_claude_code,
            state.show_codex,
            state.show_antigravity,
        ),
        state.language,
        state.show_drag_handle,
        codex_credit_panel_visible(state.show_codex, state.credit_display, state.data.as_ref()),
        state.credit_value_mode,
        &state.codex_credit_text,
        quota_text_width_for(state.language, &quota_texts),
    )
}

fn total_widget_width() -> i32 {
    let state = lock_state();
    match state.as_ref() {
        Some(s) => total_widget_width_for_state(s),
        None => total_widget_width_for(
            1,
            LanguageId::English,
            false,
            false,
            CreditValueMode::Credits,
            "",
            quota_text_width_fallback(LanguageId::English),
        ),
    }
}

fn quota_texts_for_state(state: &AppState) -> Vec<&str> {
    quota_texts_for_values(
        state.show_claude_code,
        state.show_codex,
        state.show_antigravity,
        state.show_session_window,
        state.show_weekly_window,
        &state.session_text,
        &state.weekly_text,
        &state.codex_session_text,
        &state.codex_weekly_text,
        &state.antigravity_session_text,
        &state.antigravity_weekly_text,
    )
}

fn quota_texts_for_values<'a>(
    show_claude_code: bool,
    show_codex: bool,
    show_antigravity: bool,
    show_session_window: bool,
    show_weekly_window: bool,
    session_text: &'a str,
    weekly_text: &'a str,
    codex_session_text: &'a str,
    codex_weekly_text: &'a str,
    antigravity_session_text: &'a str,
    antigravity_weekly_text: &'a str,
) -> Vec<&'a str> {
    let mut texts = Vec::new();
    if show_session_window {
        if show_claude_code {
            texts.push(session_text);
        }
        if show_codex {
            texts.push(codex_session_text);
        }
        if show_antigravity {
            texts.push(antigravity_session_text);
        }
    }
    if show_weekly_window {
        if show_claude_code {
            texts.push(weekly_text);
        }
        if show_codex {
            texts.push(codex_weekly_text);
        }
        if show_antigravity {
            texts.push(antigravity_weekly_text);
        }
    }
    texts
}

fn adaptive_poll_interval(data: Option<&AppUsageData>) -> u32 {
    let Some(data) = data else {
        return POLL_5_MIN;
    };

    let mut most_urgent = 100.0_f64;
    let mut found = false;
    for usage in [
        data.claude_code.as_ref(),
        data.codex.as_ref(),
        data.antigravity.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        for section in [&usage.session, &usage.weekly] {
            if !section.available
                || !section.percentage.is_finite()
                || !(0.0..=100.0).contains(&section.percentage)
            {
                continue;
            }
            let remaining = poller::remaining_percentage(section.percentage);
            if remaining.is_finite() {
                most_urgent = most_urgent.min(remaining);
                found = true;
            }
        }
    }

    if !found || most_urgent > 30.0 {
        POLL_5_MIN
    } else if most_urgent > 10.0 {
        POLL_1_MIN
    } else {
        POLL_30_SEC
    }
}

fn selected_bar_color(setting: Option<&str>) -> Color {
    setting
        .and_then(parse_hex_color)
        .unwrap_or_else(native_interop::windows_accent_color)
}

fn claude_usage_text_color(is_dark: bool) -> Color {
    if is_dark {
        Color::from_hex("#F09A7A")
    } else {
        Color::from_hex("#A94F32")
    }
}

fn codex_usage_text_color(is_dark: bool) -> Color {
    if is_dark {
        Color::from_hex("#F5F5F5")
    } else {
        Color::from_hex("#1F1F1F")
    }
}

fn antigravity_usage_text_color(is_dark: bool) -> Color {
    if is_dark {
        Color::from_hex("#8AB4F8")
    } else {
        Color::from_hex("#1967D2")
    }
}

pub fn run() {
    // Enable Per-Monitor DPI Awareness V2 for crisp rendering at any scale factor
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        CURRENT_DPI.store(GetDpiForSystem(), Ordering::Relaxed);
    }
    diagnose::log("window::run started");

    // Single-instance guard: silently exit if another instance is running.
    // Exception: when relaunched after an explorer restart (ENV_RELAUNCH set),
    // wait for the previous instance to release the mutex, then take over.
    let is_relaunch = std::env::var(ENV_RELAUNCH).is_ok();
    let mutex_name = native_interop::wide_str("Global\\CodexUsage");
    let _mutex = unsafe {
        let handle = CreateMutexW(None, true, PCWSTR::from_raw(mutex_name.as_ptr()));
        match handle {
            Ok(h) => {
                if GetLastError() == ERROR_ALREADY_EXISTS {
                    if is_relaunch {
                        diagnose::log("relaunch: waiting for previous instance to exit");
                        let wait_result = WaitForSingleObject(h, 10_000);
                        if wait_result != WAIT_OBJECT_0 && wait_result != WAIT_ABANDONED {
                            diagnose::log(format!(
                                "startup aborted: previous instance did not exit cleanly ({wait_result:?})"
                            ));
                            return;
                        }
                    } else {
                        diagnose::log("startup aborted: another instance is already running");
                        return;
                    }
                }
                h
            }
            Err(error) => {
                diagnose::log_error(
                    "startup aborted: unable to create single-instance mutex",
                    error,
                );
                return;
            }
        }
    };

    migrate_legacy_startup_entry();

    let class_name = native_interop::wide_str("CodexUsage");

    unsafe {
        let hinstance = GetModuleHandleW(PCWSTR::null()).unwrap();
        let (large_icon, small_icon) = load_embedded_app_icons();

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            hInstance: HINSTANCE(hinstance.0),
            hIcon: large_icon,
            hIconSm: small_icon,
            hCursor: LoadCursorW(HINSTANCE::default(), IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszClassName: PCWSTR::from_raw(class_name.as_ptr()),
            ..Default::default()
        };

        let atom = RegisterClassExW(&wc);
        if atom == 0 {
            diagnose::log("RegisterClassExW returned 0");
        }

        let claude_code_available = poller::claude_code_credentials_available();
        let settings = load_settings(claude_code_available);
        poller::set_claude_desktop_sign_in(settings.use_claude_desktop_sign_in);
        diagnose::log(format!(
            "Claude Desktop sign-in fallback enabled={}",
            settings.use_claude_desktop_sign_in
        ));
        let manual_position = settings
            .manual_position
            .unwrap_or(settings.tray_offset != 0);
        codex_mcp::set_monitoring_enabled(settings.show_codex, settings.show_claude_code);
        let usage_display = UsageDisplayMode::from_setting(&settings.usage_display);
        let language_override = settings.language.as_deref().and_then(LanguageId::from_code);
        let language = localization::resolve_language(language_override);
        // Create as layered popup (will be reparented into taskbar)
        let title = native_interop::wide_str(language.strings().window_title);
        let initial_model_count = active_model_count(
            settings.show_claude_code,
            settings.show_codex,
            settings.show_antigravity,
        );
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE,
            PCWSTR::from_raw(class_name.as_ptr()),
            PCWSTR::from_raw(title.as_ptr()),
            WS_POPUP,
            0,
            0,
            total_widget_width_for(
                initial_model_count,
                language,
                settings.show_drag_handle,
                false,
                CreditValueMode::Credits,
                "",
                quota_text_width_for(language, &["--"]),
            ),
            sc(WIDGET_HEIGHT),
            HWND::default(),
            HMENU::default(),
            hinstance,
            None,
        )
        .unwrap();

        if !large_icon.is_invalid() {
            let _ = SendMessageW(
                hwnd,
                WM_SETICON,
                WPARAM(ICON_BIG as usize),
                LPARAM(large_icon.0 as isize),
            );
        }
        if !small_icon.is_invalid() {
            let _ = SendMessageW(
                hwnd,
                WM_SETICON,
                WPARAM(ICON_SMALL as usize),
                LPARAM(small_icon.0 as isize),
            );
        }

        diagnose::log(format!("main window created hwnd={:?}", hwnd));

        let is_dark = theme::is_dark_mode();
        let mut embedded = false;

        {
            let mut state = lock_state();
            *state = Some(AppState {
                hwnd: SendHwnd::from_hwnd(hwnd),
                taskbar_hwnd: None,
                tray_notify_hwnd: None,
                win_event_hook: None,
                is_dark,
                embedded: false,
                language_override,
                language,
                session_percent: 0.0,
                session_text: "--".to_string(),
                weekly_percent: 0.0,
                weekly_text: "--".to_string(),
                codex_session_percent: 0.0,
                codex_session_text: "--".to_string(),
                codex_weekly_percent: 0.0,
                codex_weekly_text: "--".to_string(),
                credit_display: CreditDisplayMode::from_setting(&settings.credit_display),
                credit_position: CreditPosition::from_setting(&settings.credit_position),
                codex_credit_text: String::new(),
                antigravity_session_percent: 0.0,
                antigravity_session_text: "--".to_string(),
                antigravity_weekly_percent: 0.0,
                antigravity_weekly_text: "--".to_string(),
                claude_code_available,
                show_claude_code: settings.show_claude_code,
                show_codex: settings.show_codex,
                show_antigravity: settings.show_antigravity,
                show_session_window: settings.show_session_window,
                show_weekly_window: settings.show_weekly_window,
                show_drag_handle: settings.show_drag_handle,
                enable_codex_mcp: settings.enable_codex_mcp,
                usage_display,
                bar_color: settings.bar_color.clone(),
                credit_value_mode: CreditValueMode::from_setting(&settings.credit_value_mode),
                alert_thresholds_percent: settings.alert_thresholds_percent.unwrap_or_default(),
                notified_quota_windows: settings.notified_quota_windows.into_iter().collect(),
                data: None,
                provider_health: Default::default(),
                provider_generation: 0,
                data_sources: Default::default(),
                persisted: Default::default(),
                claude_desktop_sign_in: settings.use_claude_desktop_sign_in,
                poll_interval_ms: settings.poll_interval_ms,
                adaptive_polling: settings.adaptive_polling,
                schedule: Default::default(),
                codex_auth: CodexAuthEpisode::default(),
                last_codex_exec_refresh_unix: settings.last_codex_exec_refresh_unix,
                force_notify_auth_error: false,
                last_poll_ok: false,
                update_status: UpdateStatus::Idle,
                last_update_check_unix: settings.last_update_check_unix,
                taskbar_index: settings.taskbar_index,
                tray_offset: settings.tray_offset,
                manual_position,
                dragging: false,
                drag_start_mouse_x: 0,
                drag_start_client_x: 0,
                drag_start_offset: 0,
                widget_visible: settings.widget_visible,
            });
        }

        // Try to embed in taskbar
        if attach_to_taskbar(hwnd, settings.taskbar_index) {
            embedded = true;
        }

        // If not embedded, fall back to topmost popup with SetLayeredWindowAttributes
        if !embedded {
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA);
            let _ = SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }

        // Register system tray icon(s)
        sync_tray_icons(hwnd);

        codex_mcp::update_polling_metadata(
            settings.adaptive_polling,
            if settings.adaptive_polling {
                POLL_5_MIN
            } else {
                settings.poll_interval_ms
            },
        );
        diagnose::log(format!(
            "Codex MCP startup decision enabled={}",
            settings.enable_codex_mcp
        ));
        match start_mcp_if_enabled(settings.enable_codex_mcp, codex_mcp::start) {
            Ok(true) => {}
            Ok(false) => diagnose::log("Codex MCP server remains off (preference disabled)"),
            Err(error) => diagnose::log_error("Codex MCP integration failed to start", error),
        }

        // Position and show (only if widget_visible preference is true)
        position_at_taskbar();
        if settings.widget_visible {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        diagnose::log("window shown");

        // Initial render via UpdateLayeredWindow (for embedded) or InvalidateRect (fallback)
        render_layered();

        // Watch for explorer.exe restarts so we can re-embed and re-add the tray
        // icon (the shell discards tray registrations when it restarts). This
        // runs on a dedicated thread, NOT a window timer: once explorer destroys
        // the taskbar, our embedded child window stops receiving all messages
        // (WM_TIMER included), so a timer would never fire again.
        spawn_taskbar_watchdog();

        // Last known values first (marked stale), then the initial poll; each
        // provider then re-arms TIMER_POLL from its own deadline.
        restore_snapshots();
        position_at_taskbar();
        render_layered();
        spawn_poll(SendHwnd::from_hwnd(hwnd), PollTrigger::Scheduled);

        schedule_auto_update_check(hwnd);
        let should_check_updates = {
            let state = lock_state();
            state
                .as_ref()
                .map(|s| auto_update_check_due(s.last_update_check_unix))
                .unwrap_or(false)
        };
        if should_check_updates {
            begin_update_check(hwnd, false);
        }

        // Initial theme check
        check_theme_change();

        // Message loop
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        codex_mcp::stop();
    }
}

/// Render widget content and push to the layered window via UpdateLayeredWindow.
/// Renders fully opaque with the actual taskbar background colour so that
/// ClearType sub-pixel font rendering can be used for crisp, OS-native text.
fn render_layered() {
    refresh_dpi();
    let (
        hwnd_val,
        is_dark,
        embedded,
        language,
        strings,
        session_pct,
        session_text,
        weekly_pct,
        weekly_text,
        codex_session_pct,
        codex_session_text,
        codex_weekly_pct,
        codex_weekly_text,
        antigravity_session_pct,
        antigravity_session_text,
        antigravity_weekly_pct,
        antigravity_weekly_text,
        show_claude_code,
        show_codex,
        show_antigravity,
        show_session_window,
        show_weekly_window,
        show_drag_handle,
        directions,
        credit_panel_visible,
        credit_position,
        credit_value_mode,
        codex_credit_text,
        bar_color_setting,
    ) = {
        let state = lock_state();
        match state.as_ref() {
            Some(s) => (
                s.hwnd,
                s.is_dark,
                s.embedded,
                s.language,
                s.language.strings(),
                s.session_percent,
                s.session_text.clone(),
                s.weekly_percent,
                s.weekly_text.clone(),
                s.codex_session_percent,
                s.codex_session_text.clone(),
                s.codex_weekly_percent,
                s.codex_weekly_text.clone(),
                s.antigravity_session_percent,
                s.antigravity_session_text.clone(),
                s.antigravity_weekly_percent,
                s.antigravity_weekly_text.clone(),
                s.show_claude_code,
                s.show_codex,
                s.show_antigravity,
                s.show_session_window,
                s.show_weekly_window,
                s.show_drag_handle,
                s.usage_display.directions(),
                codex_credit_panel_visible(s.show_codex, s.credit_display, s.data.as_ref()),
                s.credit_position,
                s.credit_value_mode,
                s.codex_credit_text.clone(),
                s.bar_color.clone(),
            ),
            None => return,
        }
    };

    let hwnd = hwnd_val.to_hwnd();

    // For non-embedded fallback, just invalidate and let WM_PAINT handle it
    if !embedded {
        unsafe {
            let _ = InvalidateRect(hwnd, None, false);
        }
        return;
    }

    let quota_texts = quota_texts_for_values(
        show_claude_code,
        show_codex,
        show_antigravity,
        show_session_window,
        show_weekly_window,
        &session_text,
        &weekly_text,
        &codex_session_text,
        &codex_weekly_text,
        &antigravity_session_text,
        &antigravity_weekly_text,
    );
    let text_width = quota_text_width_for(language, &quota_texts);
    let layout_credit_text = if credit_panel_visible {
        codex_credit_text.as_str()
    } else {
        ""
    };
    let width = total_widget_width_for(
        active_model_count(show_claude_code, show_codex, show_antigravity),
        language,
        show_drag_handle,
        credit_panel_visible,
        credit_value_mode,
        layout_credit_text,
        text_width,
    );
    if native_interop::get_window_rect_safe(hwnd)
        .is_some_and(|rect| rect.right - rect.left != width)
    {
        // Keep the child-window anchor and the bitmap size tied to this same
        // render snapshot. UpdateLayeredWindow below changes size but leaves
        // its position untouched when the destination point is null.
        position_at_taskbar_with_width(width);
    }
    let height = sc(WIDGET_HEIGHT);

    let bar_color = selected_bar_color(bar_color_setting.as_deref());
    let track = if is_dark {
        Color::from_hex("#444444")
    } else {
        Color::from_hex("#AAAAAA")
    };
    let text_color = if is_dark {
        Color::from_hex("#888888")
    } else {
        Color::from_hex("#404040")
    };
    let bg_color = if is_dark {
        Color::from_hex("#1C1C1C")
    } else {
        Color::from_hex("#F3F3F3")
    };

    unsafe {
        let screen_dc = GetDC(hwnd);

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0, // BI_RGB
                ..Default::default()
            },
            ..Default::default()
        };

        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let mem_dc = CreateCompatibleDC(screen_dc);
        let dib =
            CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap_or_default();

        if dib.is_invalid() || bits.is_null() {
            let _ = DeleteDC(mem_dc);
            ReleaseDC(hwnd, screen_dc);
            return;
        }

        let old_bmp = SelectObject(mem_dc, dib);
        let pixel_count = (width * height) as usize;

        // Render once with the actual taskbar background colour.
        // Using an opaque background lets us use CLEARTYPE_QUALITY for
        // sub-pixel font rendering that matches the rest of the OS.
        paint_content(
            mem_dc,
            width,
            height,
            is_dark,
            &bg_color,
            &text_color,
            &bar_color,
            &track,
            language,
            strings,
            session_pct,
            &session_text,
            weekly_pct,
            &weekly_text,
            codex_session_pct,
            &codex_session_text,
            codex_weekly_pct,
            &codex_weekly_text,
            antigravity_session_pct,
            &antigravity_session_text,
            antigravity_weekly_pct,
            &antigravity_weekly_text,
            show_claude_code,
            show_codex,
            show_antigravity,
            show_session_window,
            show_weekly_window,
            show_drag_handle,
            directions,
            credit_panel_visible,
            credit_position,
            credit_value_mode,
            &codex_credit_text,
        );

        // Background pixels → alpha 1 (nearly invisible but still hittable for right-click).
        // Content pixels → fully opaque (preserves ClearType sub-pixel rendering).
        let bg_bgr = bg_color.to_colorref();
        let pixel_data = std::slice::from_raw_parts_mut(bits as *mut u32, pixel_count);
        for px in pixel_data.iter_mut() {
            let rgb = *px & 0x00FFFFFF;
            if rgb == bg_bgr {
                *px = 0x01000000;
            } else {
                *px = rgb | 0xFF000000;
            }
        }

        // Push to window via UpdateLayeredWindow
        let pt_src = POINT { x: 0, y: 0 };
        let sz = SIZE {
            cx: width,
            cy: height,
        };
        let blend = BLENDFUNCTION {
            BlendOp: 0, // AC_SRC_OVER
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: 1, // AC_SRC_ALPHA
        };

        let _ = UpdateLayeredWindow(
            hwnd,
            screen_dc,
            None,
            Some(&sz),
            mem_dc,
            Some(&pt_src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );

        // Cleanup
        SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(dib);
        let _ = DeleteDC(mem_dc);
        ReleaseDC(hwnd, screen_dc);
    }
}

/// Paint all widget content onto a DC with a given background color.
fn paint_content(
    hdc: HDC,
    width: i32,
    height: i32,
    is_dark: bool,
    bg: &Color,
    text_color: &Color,
    bar_color: &Color,
    track: &Color,
    language: LanguageId,
    strings: Strings,
    session_pct: f64,
    session_text: &str,
    weekly_pct: f64,
    weekly_text: &str,
    codex_session_pct: f64,
    codex_session_text: &str,
    codex_weekly_pct: f64,
    codex_weekly_text: &str,
    antigravity_session_pct: f64,
    antigravity_session_text: &str,
    antigravity_weekly_pct: f64,
    antigravity_weekly_text: &str,
    show_claude_code: bool,
    show_codex: bool,
    show_antigravity: bool,
    show_session_window: bool,
    show_weekly_window: bool,
    show_drag_handle: bool,
    directions: DisplayDirections,
    credit_panel_visible: bool,
    credit_position: CreditPosition,
    credit_value_mode: CreditValueMode,
    credit_text: &str,
) {
    unsafe {
        let session_pct = usage_percent_for_display(directions.claude, session_pct);
        let weekly_pct = usage_percent_for_display(directions.claude, weekly_pct);
        let codex_session_pct = usage_percent_for_display(directions.codex, codex_session_pct);
        let codex_weekly_pct = usage_percent_for_display(directions.codex, codex_weekly_pct);
        let antigravity_session_pct =
            usage_percent_for_display(directions.antigravity, antigravity_session_pct);
        let antigravity_weekly_pct =
            usage_percent_for_display(directions.antigravity, antigravity_weekly_pct);
        let quota_texts = quota_texts_for_values(
            show_claude_code,
            show_codex,
            show_antigravity,
            show_session_window,
            show_weekly_window,
            session_text,
            weekly_text,
            codex_session_text,
            codex_weekly_text,
            antigravity_session_text,
            antigravity_weekly_text,
        );
        let (label_width, text_width) = usage_layout_widths(language, &quota_texts);
        let layout_credit_text = if credit_panel_visible {
            credit_text
        } else {
            ""
        };
        let (credit_header_width, credit_value_width) =
            credit_layout_widths(strings, layout_credit_text);

        let client_rect = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        };

        let bg_brush = CreateSolidBrush(COLORREF(bg.to_colorref()));
        FillRect(hdc, &client_rect, bg_brush);
        let _ = DeleteObject(bg_brush);

        if show_drag_handle {
            // Compact 2x3 grab grip. The hit area remains wider than the visible
            // dots so dragging is easy without adding a divider to the widget.
            let grip_color = if is_dark {
                native_interop::colorref(100, 100, 100)
            } else {
                native_interop::colorref(150, 150, 150)
            };
            let grip_brush = CreateSolidBrush(COLORREF(grip_color));
            let dot_size = sc(DRAG_GRIP_DOT_SIZE);
            let column_gap = sc(DRAG_GRIP_COLUMN_GAP);
            let row_gap = sc(DRAG_GRIP_ROW_GAP);
            let grip_width = dot_size * 2 + column_gap;
            let grip_height = dot_size * 3 + row_gap * 2;
            let grip_left = (sc(DRAG_HANDLE_HIT_W) - grip_width) / 2;
            let grip_top = (height - grip_height) / 2;
            for row in 0..3 {
                for column in 0..2 {
                    let left = grip_left + column * (dot_size + column_gap);
                    let top = grip_top + row * (dot_size + row_gap);
                    let dot_region = CreateRoundRectRgn(
                        left,
                        top,
                        left + dot_size + 1,
                        top + dot_size + 1,
                        dot_size,
                        dot_size,
                    );
                    let _ = FillRgn(hdc, dot_region, grip_brush);
                    let _ = DeleteObject(dot_region);
                }
            }
            let _ = DeleteObject(grip_brush);
        }

        let active_models = active_model_count(show_claude_code, show_codex, show_antigravity);
        let (content_x, credit_x) = widget_content_positions_for(
            active_models,
            language,
            show_drag_handle,
            credit_panel_visible,
            credit_position,
            credit_value_mode,
            layout_credit_text,
            text_width,
        );
        let row2_y = height - sc(5) - sc(SEGMENT_H);
        let row1_y = row2_y - sc(10) - sc(SEGMENT_H);
        let single_row_y = (height - sc(SEGMENT_H)) / 2;

        let _ = SetBkMode(hdc, TRANSPARENT);
        let _ = SetTextColor(hdc, COLORREF(text_color.to_colorref()));

        let font_name = native_interop::wide_str("Segoe UI");
        let font = CreateFontW(
            sc(-12),
            0,
            0,
            0,
            FW_MEDIUM.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_TT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            CLEARTYPE_QUALITY.0 as u32,
            (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            PCWSTR::from_raw(font_name.as_ptr()),
        );
        let old_font = SelectObject(hdc, font);

        if let Some(credit_x) = credit_x {
            let (credit_header_y, credit_value_y) = credit_panel_y_positions(height);
            draw_credit_panel(
                hdc,
                credit_x,
                credit_header_y,
                credit_value_y,
                strings.credits,
                credit_text,
                text_color,
                credit_header_width,
                credit_value_width,
                credit_position,
            );
        }

        if show_session_window {
            draw_row(
                hdc,
                content_x,
                if show_weekly_window {
                    row1_y
                } else {
                    single_row_y
                },
                is_dark,
                text_color,
                strings.session_window,
                session_pct,
                session_text,
                codex_session_pct,
                codex_session_text,
                antigravity_session_pct,
                antigravity_session_text,
                show_claude_code,
                show_codex,
                show_antigravity,
                bar_color,
                track,
                label_width,
                text_width,
            );
        }
        if show_weekly_window {
            draw_row(
                hdc,
                content_x,
                if show_session_window {
                    row2_y
                } else {
                    single_row_y
                },
                is_dark,
                text_color,
                strings.weekly_window,
                weekly_pct,
                weekly_text,
                codex_weekly_pct,
                codex_weekly_text,
                antigravity_weekly_pct,
                antigravity_weekly_text,
                show_claude_code,
                show_codex,
                show_antigravity,
                bar_color,
                track,
                label_width,
                text_width,
            );
        }

        SelectObject(hdc, old_font);
        let _ = DeleteObject(font);
    }
}

fn poll_error_display_label(error: poller::PollError, language: LanguageId) -> &'static str {
    match error {
        poller::PollError::AuthRequired
        | poller::PollError::NoCredentials
        | poller::PollError::TokenExpired => "!",
        poller::PollError::NetworkUnavailable => {
            if language == LanguageId::SimplifiedChinese {
                "网络"
            } else {
                "NET"
            }
        }
        poller::PollError::RateLimited => {
            if language == LanguageId::SimplifiedChinese {
                "限流"
            } else {
                "429"
            }
        }
        poller::PollError::ServerError => {
            if language == LanguageId::SimplifiedChinese {
                "服务"
            } else {
                "5XX"
            }
        }
        poller::PollError::RequestFailed => {
            if language == LanguageId::SimplifiedChinese {
                "错误"
            } else {
                "ERR"
            }
        }
    }
}

fn effective_poll_interval_ms(state: &AppState) -> u32 {
    if state.adaptive_polling {
        adaptive_poll_interval(state.data.as_ref())
    } else {
        state.poll_interval_ms
    }
}

fn is_codex_auth_error(error: Option<poller::PollError>) -> bool {
    matches!(
        error,
        Some(poller::PollError::AuthRequired | poller::PollError::TokenExpired)
    )
}

fn credential_snapshot_changed(
    previous: &poller::CredentialWatchSnapshot,
    current: &poller::CredentialWatchSnapshot,
) -> bool {
    previous != current
}

fn codex_exec_cooldown_elapsed(last_attempt: Option<u64>, now: u64) -> bool {
    last_attempt
        .map(|last| now.saturating_sub(last) >= CODEX_EXEC_REFRESH_COOLDOWN_SECS)
        .unwrap_or(true)
}

/// What started a dispatch; decides which schedule gates a provider may bypass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PollTrigger {
    /// TIMER_POLL: start providers whose own deadline has arrived.
    Scheduled,
    /// Explicit user refresh: bypass cadence, backoff and Claude spacing, but
    /// never a server Retry-After deadline.
    Manual,
    /// Frequency or provider selection change: refresh idle providers promptly
    /// while active backoff, spacing and server deadlines are preserved.
    Reconfigure,
    /// A displayed reset time has passed for that provider.
    ResetBoundary,
    /// Codex credentials changed during auth recovery.
    CodexCredentialChange,
}

fn poll_request(
    trigger: PollTrigger,
    enabled: [bool; provider_schedule::PROVIDERS],
    past_reset: [bool; provider_schedule::PROVIDERS],
    codex_auth_active: bool,
) -> provider_schedule::Request {
    use provider_schedule::Request;
    // Codex auth recovery runs its own retry ladder; only a scheduled tick,
    // an explicit refresh or a credential change may start it early.
    let outside_codex_recovery: [bool; provider_schedule::PROVIDERS] =
        std::array::from_fn(|id| enabled[id] && !(id == CODEX && codex_auth_active));
    match trigger {
        PollTrigger::Scheduled => Request {
            providers: enabled,
            ..Default::default()
        },
        PollTrigger::Manual => Request {
            providers: enabled,
            ignore_due: true,
            ignore_backoff: true,
            ignore_spacing: true,
        },
        PollTrigger::Reconfigure => Request {
            providers: outside_codex_recovery,
            ignore_due: true,
            ..Default::default()
        },
        PollTrigger::ResetBoundary => Request {
            providers: std::array::from_fn(|id| outside_codex_recovery[id] && past_reset[id]),
            ignore_due: true,
            ..Default::default()
        },
        PollTrigger::CodexCredentialChange => Request {
            providers: [false, enabled[CODEX], false],
            ignore_due: true,
            ignore_backoff: true,
            ..Default::default()
        },
    }
}

fn enabled_providers(state: &AppState) -> [bool; provider_schedule::PROVIDERS] {
    [
        state.show_claude_code,
        state.show_codex,
        state.show_antigravity,
    ]
}

/// Start every provider the trigger allows, each on its own worker thread, so
/// a slow or failing provider can neither block nor delay the others.
fn spawn_poll(send_hwnd: SendHwnd, trigger: PollTrigger) {
    let started = {
        let mut guard = lock_state();
        let Some(state) = guard.as_mut() else {
            return;
        };
        let enabled = enabled_providers(state);
        let past_reset = reset_poll_pending(state, SystemTime::now());
        let request = poll_request(trigger, enabled, past_reset, state.codex_auth.active);
        let started = state.schedule.begin(Instant::now(), request);
        if started.contains(&true) {
            diagnose::log(format!(
                "usage poll dispatch trigger={trigger:?} cadence_mode={} base_interval_ms={} started=claude:{},codex:{},antigravity:{}",
                if state.adaptive_polling { "adaptive" } else { "fixed" },
                effective_poll_interval_ms(state),
                started[0],
                started[1],
                started[2]
            ));
        }
        if trigger == PollTrigger::Manual {
            for (id, start) in started.into_iter().enumerate() {
                // Antigravity keeps its text, as before Build 6.
                if start && id != 2 {
                    set_provider_texts(state, id, "...".to_string());
                }
            }
        }
        let generation = state.provider_generation;
        started.map(|start| start.then_some(generation))
    };

    for (id, generation) in started.into_iter().enumerate() {
        let Some(generation) = generation else {
            continue;
        };
        std::thread::spawn(move || {
            if id == CODEX {
                maybe_refresh_codex_auth();
            }
            let result = poller::poll_provider(id);
            apply_provider_poll(send_hwnd.to_hwnd(), id, generation, result);
            // The UI thread owns TIMER_POLL; ask it to re-arm.
            unsafe {
                let _ = PostMessageW(send_hwnd.to_hwnd(), WM_APP_REARM_POLL, WPARAM(0), LPARAM(0));
            }
        });
    }
    rearm_poll_timer(send_hwnd.to_hwnd());
}

/// Arm TIMER_POLL for the earliest idle provider deadline. With every enabled
/// provider in flight, the finishing worker's WM_APP_REARM_POLL re-arms it.
/// Called only on the UI thread that owns the window.
fn rearm_poll_timer(hwnd: HWND) {
    let delay = {
        let state = lock_state();
        state.as_ref().and_then(|state| {
            state
                .schedule
                .next_delay_ms(Instant::now(), enabled_providers(state))
        })
    };
    unsafe {
        match delay {
            Some(delay) => {
                SetTimer(hwnd, TIMER_POLL, delay, None);
            }
            None => {
                let _ = KillTimer(hwnd, TIMER_POLL);
            }
        }
    }
}

fn should_attempt_codex_model_free_refresh(passive_failures: u8, attempted: bool) -> bool {
    !attempted && passive_failures >= CODEX_AUTH_REFRESH_AFTER_FAILURES
}

fn should_attempt_codex_exec_fallback(
    passive_failures: u8,
    model_free_attempted: bool,
    model_free_succeeded: bool,
    exec_attempted: bool,
    cooldown_elapsed: bool,
) -> bool {
    // Automatic monitoring is strictly model-free (upstream PR #25 policy).
    ALLOW_AUTOMATIC_MODEL_RECOVERY
        && model_free_attempted
        && !model_free_succeeded
        && !exec_attempted
        && cooldown_elapsed
        && passive_failures >= CODEX_AUTH_EXEC_AFTER_FAILURES
}

fn maybe_refresh_codex_auth() {
    enum Action {
        ModelFree,
        LastResortExec,
    }
    let action = {
        let mut state = lock_state();
        let Some(state) = state.as_mut() else {
            return;
        };
        if !state.show_codex || !state.codex_auth.active {
            return;
        }
        if should_attempt_codex_model_free_refresh(
            state.codex_auth.passive_failures,
            state.codex_auth.model_free_refresh_attempted,
        ) {
            state.codex_auth.model_free_refresh_attempted = true;
            Some(Action::ModelFree)
        } else {
            let cooldown_elapsed =
                codex_exec_cooldown_elapsed(state.last_codex_exec_refresh_unix, now_unix_secs());
            if should_attempt_codex_exec_fallback(
                state.codex_auth.passive_failures,
                state.codex_auth.model_free_refresh_attempted,
                state.codex_auth.model_free_refresh_succeeded,
                state.codex_auth.exec_attempted,
                cooldown_elapsed,
            ) {
                state.codex_auth.exec_attempted = true;
                state.last_codex_exec_refresh_unix = Some(now_unix_secs());
                Some(Action::LastResortExec)
            } else {
                None
            }
        }
    };

    match action {
        Some(Action::ModelFree) => {
            let before = poller::credential_watch_snapshot(poller::CredentialWatchMode::Codex);
            diagnose::log(
                "Codex auth recovery entering official model-free refresh after passive retries",
            );
            let success = poller::refresh_codex_token_model_free();
            let after = poller::credential_watch_snapshot(poller::CredentialWatchMode::Codex);
            if credential_snapshot_changed(&before, &after) {
                diagnose::log("Codex credential change detected after model-free refresh");
            }
            let mut state = lock_state();
            if let Some(state) = state.as_mut() {
                state.codex_auth.model_free_refresh_succeeded = success;
                state.codex_auth.credential_snapshot = after;
            }
        }
        Some(Action::LastResortExec) => {
            diagnose::log("Codex auth recovery invoking cooldown-protected last-resort model task");
            save_state_settings();
            let before = poller::credential_watch_snapshot(poller::CredentialWatchMode::Codex);
            let success = poller::run_codex_exec_last_resort();
            let after = poller::credential_watch_snapshot(poller::CredentialWatchMode::Codex);
            diagnose::log(format!("last-resort Codex model task completed={success}"));
            if credential_snapshot_changed(&before, &after) {
                diagnose::log("Codex credential change detected after last-resort model task");
            }
            let mut state = lock_state();
            if let Some(state) = state.as_mut() {
                state.codex_auth.credential_snapshot = after;
            }
        }
        None => {}
    }
}

fn record_codex_auth_failure(
    state: &mut AppState,
    snapshot: poller::CredentialWatchSnapshot,
) -> (bool, u32) {
    let is_new_episode = !state.codex_auth.active;
    let notify = is_new_episode || state.force_notify_auth_error;
    state.force_notify_auth_error = false;
    state.codex_auth.active = true;
    state.codex_auth.passive_failures = state.codex_auth.passive_failures.saturating_add(1);
    state.codex_auth.credential_snapshot = snapshot;
    mark_model_free_refresh_ineffective_after_auth_rejection(&mut state.codex_auth);
    // Display text follows refresh_usage_texts: a retained same-credential
    // snapshot shows stale (`!88% ...`), no snapshot shows `!`.
    let delay = codex_auth_retry_delay_ms(state.codex_auth.passive_failures);
    diagnose::log(format!(
        "Codex auth recovery {} passive_failure={} retry_ms={delay}",
        if is_new_episode {
            "started"
        } else {
            "continuing"
        },
        state.codex_auth.passive_failures
    ));
    (notify, delay)
}

fn codex_auth_retry_delay_ms(passive_failures: u8) -> u32 {
    match passive_failures {
        0 | 1 => 30_000,
        2 => 60_000,
        3 => 120_000,
        _ => 300_000,
    }
}

fn finish_codex_auth_recovery(state: &mut AppState) -> bool {
    if !finish_codex_auth_episode(&mut state.codex_auth) {
        return false;
    }
    diagnose::log("Codex auth recovery ended after successful usage poll");
    true
}

fn finish_codex_auth_episode(episode: &mut CodexAuthEpisode) -> bool {
    if !episode.active {
        return false;
    }
    *episode = CodexAuthEpisode::default();
    true
}

fn mark_model_free_refresh_ineffective_after_auth_rejection(episode: &mut CodexAuthEpisode) {
    if episode.model_free_refresh_attempted {
        episode.model_free_refresh_succeeded = false;
    }
}

fn provider_usage(data: &AppUsageData, id: usize) -> Option<&crate::models::UsageData> {
    match id {
        CLAUDE => data.claude_code.as_ref(),
        CODEX => data.codex.as_ref(),
        _ => data.antigravity.as_ref(),
    }
}

fn provider_usage_mut(data: &mut AppUsageData, id: usize) -> &mut Option<crate::models::UsageData> {
    match id {
        CLAUDE => &mut data.claude_code,
        CODEX => &mut data.codex,
        _ => &mut data.antigravity,
    }
}

/// Apply one provider's result to the display data. A failure keeps that
/// provider's credential-scoped cache (shown stale); an invalidated scope
/// drops it. Other providers are never touched.
fn merge_provider_result(
    data: &mut AppUsageData,
    id: usize,
    fresh: Option<crate::models::UsageData>,
    invalidate: bool,
) {
    let slot = provider_usage_mut(data, id);
    if fresh.is_some() || invalidate {
        *slot = fresh;
    }
}

fn is_auth_wait_error(error: Option<poller::PollError>) -> bool {
    matches!(
        error,
        Some(
            poller::PollError::AuthRequired
                | poller::PollError::TokenExpired
                | poller::PollError::NoCredentials
        )
    )
}

/// How a finished request moves that provider's own schedule.
fn schedule_outcome(
    error: Option<poller::PollError>,
    retry_after_ms: Option<u32>,
    codex_auth_delay_ms: Option<u32>,
    base_interval_ms: u32,
) -> provider_schedule::Outcome {
    use provider_schedule::Outcome;
    match (error, codex_auth_delay_ms) {
        (None, _) => Outcome::Success,
        (Some(_), Some(delay_ms)) => Outcome::Wait { delay_ms },
        (error, None) if is_auth_wait_error(error) => Outcome::Wait {
            delay_ms: base_interval_ms,
        },
        (Some(_), None) => Outcome::Transient {
            server_delay_ms: retry_after_ms,
        },
    }
}

fn set_provider_texts(state: &mut AppState, id: usize, text: String) {
    let (session, weekly) = match id {
        CLAUDE => (&mut state.session_text, &mut state.weekly_text),
        CODEX => (&mut state.codex_session_text, &mut state.codex_weekly_text),
        _ => (
            &mut state.antigravity_session_text,
            &mut state.antigravity_weekly_text,
        ),
    };
    *session = text.clone();
    *weekly = text;
}

fn auth_balloon(
    state: &AppState,
    id: usize,
) -> (tray_icon::TrayIconKind, &'static str, &'static str) {
    let strings = state.language.strings();
    match id {
        CLAUDE => (
            tray_icon::TrayIconKind::Claude,
            strings.token_expired_title,
            strings.token_expired_body,
        ),
        CODEX => (
            tray_icon::TrayIconKind::Codex,
            strings.codex_token_expired_title,
            strings.codex_token_expired_body,
        ),
        _ => (
            tray_icon::TrayIconKind::Antigravity,
            strings.antigravity_token_expired_title,
            strings.antigravity_token_expired_body,
        ),
    }
}

/// Publish one provider's completed request. Runs on that provider's worker;
/// every other provider's data, health and schedule are left untouched.
fn apply_provider_poll(hwnd: HWND, id: usize, generation: u64, result: poller::ProviderPoll) {
    let now = Instant::now();
    let codex_auth_failed = id == CODEX && is_codex_auth_error(result.error);
    let codex_snapshot = codex_auth_failed
        .then(|| poller::credential_watch_snapshot(poller::CredentialWatchMode::Codex));
    let fresh = result.data;
    let mut quota_alerts = Vec::new();
    let mut save_settings = false;
    let reposition;
    let mut balloon = None;
    let mut codex_watch = None;
    let polling_metadata;
    let mut persist: Option<(String, quota_cache::Snapshot)> = None;
    let mut adopted_fallback = None;
    let observed_at = result.observed_at.unwrap_or_else(SystemTime::now);

    {
        let mut guard = lock_state();
        let Some(state) = guard.as_mut() else {
            return;
        };
        let enabled = enabled_providers(state);
        if state.provider_generation != generation || !enabled[id] {
            state.schedule.discard(id, now, result.retry_after_ms);
            diagnose::log(format!(
                "discarding {} poll after provider selection changed",
                poller::provider_name(id)
            ));
            return;
        }
        let width_before = total_widget_width_for_state(state);
        let credit_panel_before =
            codex_credit_panel_visible(state.show_codex, state.credit_display, state.data.as_ref());

        let previous_error = state.provider_health[id].error;
        let invalidate =
            state.provider_health[id].record(result.scope.clone(), result.error, observed_at);
        codex_mcp::record_provider_health(id, invalidate, result.error);
        merge_provider_result(
            state.data.get_or_insert_with(Default::default),
            id,
            fresh.clone(),
            invalidate,
        );
        if invalidate {
            state.persisted[id] = None;
            state.data_sources[id] = poller::DataSource::Live;
        }
        if let Some(usage) = fresh.as_ref() {
            state.data_sources[id] = result.source;
            if let Some(scope) = result.scope.clone().filter(|_| {
                quota_cache::should_write(state.persisted[id].as_ref(), usage, observed_at)
            }) {
                let snapshot = quota_cache::Snapshot {
                    data: usage.clone(),
                    updated_at: observed_at,
                };
                state.persisted[id] = Some(snapshot.clone());
                persist = Some((scope, snapshot));
            }
        } else if let Some((data, fetched_at)) = result.stale_fallback.clone() {
            // The request failed, but Claude Code itself fetched newer
            // official values for this account: show them, still marked stale.
            if state.provider_health[id].adopt_observation(fetched_at) {
                diagnose::log(format!(
                    "Claude showing Claude Code's newer cached reading age_s={} while the sign-in is unavailable",
                    SystemTime::now()
                        .duration_since(fetched_at)
                        .unwrap_or_default()
                        .as_secs()
                ));
                state.data_sources[id] = poller::DataSource::ClaudeCodeCache;
                *provider_usage_mut(state.data.get_or_insert_with(Default::default), id) =
                    Some(data.clone());
                adopted_fallback = Some((data, fetched_at));
            }
        }

        if fresh.is_some() && !reset_poll_pending(state, SystemTime::now()).contains(&true) {
            unsafe {
                let _ = KillTimer(hwnd, TIMER_RESET_POLL);
            }
        }
        if let Some(usage) = fresh.as_ref() {
            let mut only_this_provider = AppUsageData::default();
            *provider_usage_mut(&mut only_this_provider, id) = Some(usage.clone());
            let notified_before = state.notified_quota_windows.clone();
            quota_alerts = collect_low_quota_alerts(state, &only_this_provider);
            save_settings =
                !quota_alerts.is_empty() || state.notified_quota_windows != notified_before;
        }

        let mut codex_auth_delay_ms = None;
        if codex_auth_failed {
            let (notify, delay) =
                record_codex_auth_failure(state, codex_snapshot.unwrap_or_default());
            codex_auth_delay_ms = Some(delay);
            codex_watch = Some(true);
            if notify {
                balloon = Some(auth_balloon(state, id));
            }
        } else if id == CODEX && fresh.is_some() && finish_codex_auth_recovery(state) {
            codex_watch = Some(false);
        }
        if id != CODEX && is_auth_wait_error(result.error) {
            // Notify once per sign-in episode, and only when no other enabled
            // provider is currently healthy (the pre-Build 6 all-failed rule).
            let new_episode = !is_auth_wait_error(previous_error);
            let others_failing = (0..provider_schedule::PROVIDERS)
                .filter(|&other| other != id && enabled[other])
                .all(|other| state.provider_health[other].error.is_some());
            if (new_episode || state.force_notify_auth_error) && others_failing {
                balloon = Some(auth_balloon(state, id));
            }
            state.force_notify_auth_error = false;
        } else if result.error.is_none() {
            state.force_notify_auth_error = false;
        }

        let base_interval = effective_poll_interval_ms(state);
        let outcome = schedule_outcome(
            result.error,
            result.retry_after_ms,
            codex_auth_delay_ms,
            base_interval,
        );
        state.schedule.finish(
            id,
            now,
            outcome,
            provider_schedule::provider_interval_ms(id, base_interval),
        );
        if let provider_schedule::Outcome::Transient { server_delay_ms } = outcome {
            diagnose::log(format!(
                "{} poll failure retry={} server_retry_after_ms={server_delay_ms:?}; other providers keep their own schedules",
                poller::provider_name(id),
                state.schedule.retry_count(id)
            ));
        }
        diagnose::log(format!(
            "provider schedule {}",
            state.schedule.describe(Instant::now(), enabled)
        ));

        state.last_poll_ok = state.data.as_ref().is_some_and(|data| {
            (0..provider_schedule::PROVIDERS).any(|other| provider_usage(data, other).is_some())
        });
        if state.last_poll_ok {
            refresh_usage_texts(state);
        } else if let Some(error) = result.error {
            set_provider_texts(
                state,
                id,
                poll_error_display_label(error, state.language).to_string(),
            );
        }

        let credit_panel_after =
            codex_credit_panel_visible(state.show_codex, state.credit_display, state.data.as_ref());
        if credit_panel_before != credit_panel_after {
            diagnose::log(format!(
                "credit panel visibility changed from {} to {} reason={}",
                credit_panel_before,
                credit_panel_after,
                credit_panel_reason(state.show_codex, state.credit_display, state.data.as_ref(),)
            ));
        }
        reposition = credit_panel_before != credit_panel_after
            || width_before != total_widget_width_for_state(state);
        polling_metadata = (state.adaptive_polling, base_interval);
    }

    // Serialize publication with selection changes, including disable/re-enable.
    {
        let publication_guard = lock_state();
        if publication_guard
            .as_ref()
            .is_none_or(|s| s.provider_generation != generation)
        {
            return;
        }
        let (adaptive, interval) = polling_metadata;
        codex_mcp::update_polling_metadata(adaptive, interval);
        if let Some(data) = fresh {
            codex_mcp::publish_observation(id, data, observed_at, result.source, true);
        } else if let Some((data, fetched_at)) = adopted_fallback {
            codex_mcp::publish_observation(
                id,
                data,
                fetched_at,
                poller::DataSource::ClaudeCodeCache,
                false,
            );
        }
    }

    // Persist outside the state lock; older observations never replace
    // newer ones and other accounts' files are never matched.
    if let Some((scope, snapshot)) = persist {
        if let Err(error) = quota_cache::save(id, &scope, &snapshot.data, snapshot.updated_at) {
            diagnose::log_error(
                &format!("{} quota snapshot not saved", poller::provider_name(id)),
                error,
            );
        }
    }

    unsafe {
        match codex_watch {
            Some(true) => {
                SetTimer(hwnd, TIMER_CREDENTIAL_WATCH, CODEX_AUTH_WATCH_MS, None);
            }
            Some(false) => {
                let _ = KillTimer(hwnd, TIMER_CREDENTIAL_WATCH);
            }
            None => {}
        }
    }
    if let Some((kind, title, message)) = balloon {
        tray_icon::notify_balloon(hwnd, kind, title, message);
    }
    if reposition {
        position_at_taskbar();
    }
    for alert in &quota_alerts {
        tray_icon::notify_balloon(hwnd, alert.kind, &alert.title, &alert.message);
        diagnose::log(format!(
            "low quota alert emitted title={} message={}",
            alert.title, alert.message
        ));
    }
    if save_settings {
        save_state_settings();
    }
    unsafe {
        let _ = PostMessageW(hwnd, WM_APP_USAGE_UPDATED, WPARAM(0), LPARAM(0));
    }
}

fn schedule_countdown_timer() {
    let state = lock_state();
    let s = match state.as_ref() {
        Some(s) => s,
        None => return,
    };

    let hwnd = s.hwnd.to_hwnd();
    if !s.last_poll_ok {
        unsafe {
            let _ = KillTimer(hwnd, TIMER_COUNTDOWN);
            let _ = KillTimer(hwnd, TIMER_RESET_POLL);
        }
        return;
    }

    let data = match &s.data {
        Some(d) => d,
        None => return,
    };

    // If a healthy provider's reset time has passed, poll every 5s to pick
    // up fresh data; failing providers keep their own retry schedule.
    if reset_poll_pending(s, SystemTime::now()).contains(&true) {
        unsafe {
            SetTimer(hwnd, TIMER_RESET_POLL, 5_000, None);
        }
    } else {
        unsafe {
            let _ = KillTimer(hwnd, TIMER_RESET_POLL);
        }
    }

    let delays = [
        data.claude_code
            .as_ref()
            .and_then(|usage| poller::time_until_display_change(usage.session.resets_at)),
        data.claude_code
            .as_ref()
            .and_then(|usage| poller::time_until_display_change(usage.weekly.resets_at)),
        data.codex
            .as_ref()
            .and_then(|usage| poller::time_until_display_change(usage.session.resets_at)),
        data.codex
            .as_ref()
            .and_then(|usage| poller::time_until_display_change(usage.weekly.resets_at)),
        data.antigravity
            .as_ref()
            .and_then(|usage| poller::time_until_display_change(usage.session.resets_at)),
        data.antigravity
            .as_ref()
            .and_then(|usage| poller::time_until_display_change(usage.weekly.resets_at)),
    ];
    let min_delay = delays.into_iter().flatten().min();

    let ms = min_delay
        .unwrap_or(Duration::from_secs(60))
        .as_millis()
        .max(1000) as u32;

    unsafe {
        SetTimer(hwnd, TIMER_COUNTDOWN, ms, None);
    }
}

fn check_theme_change() {
    let new_dark = theme::is_dark_mode();
    let changed = {
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            if s.is_dark != new_dark {
                s.is_dark = new_dark;
                true
            } else {
                false
            }
        } else {
            false
        }
    };
    if changed {
        render_layered();
    }
}

fn check_language_change() {
    if update_language_change() {
        render_layered();
    }
}

fn update_display() {
    let mut state = lock_state();
    let s = match state.as_mut() {
        Some(s) => s,
        None => return,
    };

    // Don't overwrite error text with stale cached data
    if !s.last_poll_ok {
        return;
    }

    refresh_usage_texts(s);
}

fn layout_gate() -> MutexGuard<'static, LayoutEventGate> {
    LAYOUT_GATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Arm the one-shot reconciliation timer (UI thread only).
fn arm_layout_reconcile(hwnd: HWND, delay_ms: u32) {
    unsafe {
        SetTimer(hwnd, TIMER_LAYOUT_RECONCILE, delay_ms.max(1), None);
    }
}

/// Our own tray-icon update makes Explorer relayout the tray; ignore the
/// transient geometry, then re-read the settled geometry once it ends.
fn suppress_tray_reposition_for(hwnd: HWND, duration: Duration) {
    let delay = layout_gate().suppress(Instant::now(), duration);
    arm_layout_reconcile(hwnd, delay);
}

/// TIMER_LAYOUT_RECONCILE: re-read the final shell geometry after events that
/// were deferred, throttled or may have reported intermediate geometry.
fn reconcile_taskbar_layout(hwnd: HWND) {
    unsafe {
        let _ = KillTimer(hwnd, TIMER_LAYOUT_RECONCILE);
    }
    let due = layout_gate().take_due(Instant::now());
    match due {
        Err(delay) => arm_layout_reconcile(hwnd, delay),
        Ok(()) => {
            refresh_dpi();
            if let Some((from, to)) = position_at_taskbar_with_width(total_widget_width()) {
                diagnose::log(format!(
                    "layout reconcile corrected widget from left={} right={} to left={} right={}",
                    from.left, from.right, to.left, to.right
                ));
                render_layered();
            }
        }
    }
}

fn position_at_taskbar() {
    refresh_dpi();
    position_at_taskbar_with_width(total_widget_width());
}

/// Place the widget against the shared notification-area boundary. Returns
/// the previous and new desktop rectangles when the widget actually moved.
fn position_at_taskbar_with_width(widget_width: i32) -> Option<(RECT, RECT)> {
    // Drop the app-state lock before any Win32 call that may synchronously
    // re-enter our window procedure.
    let (hwnd, embedded, tray_offset, manual_position, taskbar_hwnd) = {
        let state = lock_state();
        let s = match state.as_ref() {
            Some(s) => s,
            None => return None,
        };

        // Don't fight the user's drag
        if s.dragging {
            return None;
        }

        let taskbar_hwnd = match s.taskbar_hwnd {
            Some(h) => h,
            None => {
                diagnose::log("position_at_taskbar skipped: no taskbar handle");
                return None;
            }
        };

        (
            s.hwnd.to_hwnd(),
            s.embedded,
            s.tray_offset,
            s.manual_position,
            taskbar_hwnd,
        )
    };

    let taskbar_rect = match native_interop::get_taskbar_rect(taskbar_hwnd) {
        Some(r) => r,
        None => {
            diagnose::log("position_at_taskbar skipped: unable to query taskbar rect");
            return None;
        }
    };

    let tray_left = tray_left_for_taskbar(taskbar_hwnd, taskbar_rect);
    let occupied = native_interop::visible_child_window_rects(taskbar_hwnd, Some(hwnd));
    let anchor = position_anchor_left(taskbar_rect, tray_left, &occupied, manual_position);
    let widget_height = sc(WIDGET_HEIGHT);
    // The saved offset stays the user's preference; only the applied offset
    // is clamped, so a temporarily narrower taskbar cannot erase it.
    let placement = taskbar_layout::compute_placement(
        taskbar_rect,
        anchor,
        widget_width,
        widget_height,
        tray_offset,
    );
    let target = placement.rect;
    let current = native_interop::get_window_rect_safe(hwnd);
    if current == Some(target) {
        return None;
    }

    if anchor != tray_left {
        diagnose::log(format!(
            "taskbar anchor adjusted for occupied shell geometry from {tray_left} to {anchor}"
        ));
    }
    if embedded {
        // Child window: coordinates relative to parent (taskbar)
        let x = target.left - taskbar_rect.left;
        let y = target.top - taskbar_rect.top;
        native_interop::move_window(hwnd, x, y, widget_width, widget_height);
        diagnose::log(format!(
            "positioned embedded widget at x={x} y={y} w={widget_width} h={widget_height} boundary={} offset={}",
            tray_left - taskbar_rect.left,
            placement.offset
        ));
    } else {
        // Topmost popup: screen coordinates
        native_interop::move_window(hwnd, target.left, target.top, widget_width, widget_height);
        diagnose::log(format!(
            "positioned fallback widget at x={} y={} w={widget_width} h={widget_height} boundary={tray_left} offset={}",
            target.left, target.top, placement.offset
        ));
    }
    current.map(|from| (from, target))
}

/// WinEvent callback for taskbar layout changes (tray growth, a new tray
/// icon, clock or taskbar resizes). Every relevant event either repositions
/// now or leaves a pending reconciliation; none is dropped.
unsafe extern "system" fn on_tray_location_changed(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    // Window moves only; caret and cursor location events are irrelevant.
    if id_object != OBJID_WINDOW.0 || hwnd == HWND::default() {
        return;
    }
    let target = {
        let state = lock_state();
        state.as_ref().and_then(|s| {
            let widget = s.hwnd.to_hwnd();
            let taskbar = s.taskbar_hwnd?;
            (hwnd != widget
                && (s.tray_notify_hwnd == Some(hwnd)
                    || native_interop::is_taskbar_or_descendant(taskbar, hwnd)))
            .then_some(widget)
        })
    };
    let Some(widget) = target else {
        return;
    };

    let action = layout_gate().on_shell_event(Instant::now());
    match action {
        LayoutAction::Immediate { reconcile_ms } => {
            if position_at_taskbar_with_width({
                refresh_dpi();
                total_widget_width()
            })
            .is_some()
            {
                render_layered();
            }
            arm_layout_reconcile(widget, reconcile_ms);
        }
        LayoutAction::Deferred { reconcile_ms } => arm_layout_reconcile(widget, reconcile_ms),
    }
}

/// Main window procedure
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            // For non-embedded fallback, paint normally
            let embedded = {
                let state = lock_state();
                state.as_ref().map(|s| s.embedded).unwrap_or(false)
            };
            if embedded {
                // Layered windows don't use WM_PAINT; just validate the region
                let mut ps = PAINTSTRUCT::default();
                let _ = BeginPaint(hwnd, &mut ps);
                let _ = EndPaint(hwnd, &ps);
            } else {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                paint(hdc, hwnd);
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_DISPLAYCHANGE | WM_DPICHANGED_MSG | WM_SETTINGCHANGE => {
            if msg == WM_DPICHANGED_MSG {
                let new_dpi = (wparam.0 & 0xFFFF) as u32;
                CURRENT_DPI.store(new_dpi, Ordering::Relaxed);
            }
            if msg == WM_SETTINGCHANGE {
                check_theme_change();
                check_language_change();
            }
            refresh_dpi();
            position_at_taskbar();
            render_layered();
            LRESULT(0)
        }
        WM_TIMER => {
            let timer_id = wparam.0;
            match timer_id {
                TIMER_POLL => {
                    // Claude passive auth recovery needs no pause here: the
                    // poller resends no rejected token until credentials change.
                    spawn_poll(SendHwnd::from_hwnd(hwnd), PollTrigger::Scheduled);
                }
                TIMER_CREDENTIAL_WATCH => {
                    let previous = {
                        let state = lock_state();
                        state.as_ref().and_then(|s| {
                            s.codex_auth
                                .active
                                .then(|| s.codex_auth.credential_snapshot.clone())
                        })
                    };
                    if let Some(previous) = previous {
                        let current =
                            poller::credential_watch_snapshot(poller::CredentialWatchMode::Codex);
                        if credential_snapshot_changed(&previous, &current) {
                            {
                                let mut state = lock_state();
                                if let Some(state) = state.as_mut() {
                                    if state.codex_auth.active {
                                        state.codex_auth.credential_snapshot = current;
                                    }
                                }
                            }
                            diagnose::log("Codex credential change detected during auth recovery; retrying immediately");
                            spawn_poll(
                                SendHwnd::from_hwnd(hwnd),
                                PollTrigger::CodexCredentialChange,
                            );
                        }
                    }
                }
                TIMER_COUNTDOWN => {
                    let width_before = total_widget_width();
                    update_display();
                    // Also a periodic geometry check: a shell change that
                    // raised no event is corrected within one countdown tick.
                    let suppressed = layout_gate().is_suppressed(Instant::now());
                    if width_before != total_widget_width() || !suppressed {
                        position_at_taskbar();
                    }
                    render_layered();
                    schedule_countdown_timer();
                }
                TIMER_RESET_POLL => {
                    // Only providers past their own reset and not backing off.
                    spawn_poll(SendHwnd::from_hwnd(hwnd), PollTrigger::ResetBoundary);
                }
                TIMER_UPDATE_CHECK => {
                    begin_update_check(hwnd, false);
                }
                TIMER_LAYOUT_RECONCILE => reconcile_taskbar_layout(hwnd),
                _ => {}
            }
            LRESULT(0)
        }
        WM_APP_USAGE_UPDATED => {
            check_theme_change();
            check_language_change();
            render_layered();
            schedule_countdown_timer();
            suppress_tray_reposition_for(
                hwnd,
                Duration::from_millis(TRAY_ICON_UPDATE_REPOSITION_SUPPRESS_MS),
            );
            sync_tray_icons(hwnd);
            LRESULT(0)
        }
        WM_APP_UPDATE_CHECK_COMPLETE => {
            schedule_auto_update_check(hwnd);
            LRESULT(0)
        }
        WM_APP_REARM_POLL => {
            rearm_poll_timer(hwnd);
            LRESULT(0)
        }
        WM_SETCURSOR => {
            let is_dragging = {
                let state = lock_state();
                state.as_ref().map(|s| s.dragging).unwrap_or(false)
            };
            if is_dragging {
                let cursor = LoadCursorW(HINSTANCE::default(), IDC_SIZEWE).unwrap_or_default();
                SetCursor(cursor);
                return LRESULT(1);
            }
            if cursor_is_on_drag_handle(hwnd) {
                let cursor = LoadCursorW(HINSTANCE::default(), IDC_SIZEWE).unwrap_or_default();
                SetCursor(cursor);
                return LRESULT(1);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_LBUTTONDOWN => {
            let client_x = (lparam.0 & 0xFFFF) as i16 as i32;
            let client_y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let show_drag_handle = {
                let state = lock_state();
                state.as_ref().map(|s| s.show_drag_handle).unwrap_or(false)
            };
            if !is_drag_handle_point(show_drag_handle, client_x, client_y) {
                return LRESULT(0);
            }

            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let mut state = lock_state();
            if let Some(s) = state.as_mut() {
                // Start from the applied position, which may differ from a
                // saved offset the current taskbar cannot hold.
                if let (Some(taskbar), Some(rect)) =
                    (s.taskbar_hwnd, native_interop::get_window_rect_safe(hwnd))
                {
                    if let Some(taskbar_rect) = native_interop::get_taskbar_rect(taskbar) {
                        s.tray_offset =
                            (tray_left_for_taskbar(taskbar, taskbar_rect) - rect.right).max(0);
                    }
                }
                s.manual_position = true;
                s.dragging = true;
                s.drag_start_mouse_x = pt.x;
                s.drag_start_client_x = client_x;
                s.drag_start_offset = s.tray_offset;
            }
            SetCapture(hwnd);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let is_dragging = {
                let state = lock_state();
                state.as_ref().map(|s| s.dragging).unwrap_or(false)
            };
            if is_dragging {
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let move_target = {
                    let mut state = lock_state();
                    let s = match state.as_mut() {
                        Some(s) => s,
                        None => return LRESULT(0),
                    };

                    // Moving mouse left = positive delta = larger offset (further left)
                    let delta = s.drag_start_mouse_x - pt.x;
                    let requested_offset = s.drag_start_offset + delta;

                    let embedded = s.embedded;
                    let hwnd_val = s.hwnd.to_hwnd();
                    let widget_width = total_widget_width_for_state(s);

                    // The same notification-area clamp as automatic layout,
                    // so a drag can never cover Show hidden icons or the clock.
                    let placement = s.taskbar_hwnd.and_then(|taskbar_hwnd| {
                        let taskbar_rect = native_interop::get_taskbar_rect(taskbar_hwnd)?;
                        let tray_left = tray_left_for_taskbar(taskbar_hwnd, taskbar_rect);
                        Some((
                            taskbar_rect,
                            taskbar_layout::compute_placement(
                                taskbar_rect,
                                tray_left,
                                widget_width,
                                sc(WIDGET_HEIGHT),
                                requested_offset,
                            ),
                        ))
                    });
                    s.tray_offset = placement
                        .map(|(_, placement)| placement.offset)
                        .unwrap_or(requested_offset.max(0));
                    placement.map(|(taskbar_rect, placement)| {
                        (hwnd_val, embedded, taskbar_rect, placement.rect)
                    })
                };

                if let Some((hwnd_val, embedded, taskbar_rect, target)) = move_target {
                    let width = target.right - target.left;
                    let height = target.bottom - target.top;
                    if embedded {
                        native_interop::move_window(
                            hwnd_val,
                            target.left - taskbar_rect.left,
                            target.top - taskbar_rect.top,
                            width,
                            height,
                        );
                    } else {
                        native_interop::move_window(
                            hwnd_val,
                            target.left,
                            target.top,
                            width,
                            height,
                        );
                    }
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let drag_result = {
                let mut state = lock_state();
                if let Some(s) = state.as_mut() {
                    if s.dragging {
                        s.dragging = false;
                        Some((s.taskbar_index, s.drag_start_client_x))
                    } else {
                        None
                    }
                } else {
                    None
                }
            };
            if let Some((current_taskbar_index, drag_start_client_x)) = drag_result {
                let _ = ReleaseCapture();
                if let Some((target_index, target_taskbar)) = taskbar_at_point(pt) {
                    if target_index != current_taskbar_index {
                        let new_offset = offset_for_drop_point(
                            target_taskbar.hwnd,
                            target_taskbar.rect,
                            pt,
                            drag_start_client_x,
                        );
                        {
                            let mut state = lock_state();
                            if let Some(s) = state.as_mut() {
                                s.tray_offset = new_offset;
                            }
                        }
                        if attach_to_taskbar(hwnd, target_index) {
                            position_at_taskbar();
                            render_layered();
                        }
                    }
                }
                save_state_settings();
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            show_context_menu(hwnd);
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = wparam.0 as u16;
            match id {
                1 => {
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.force_notify_auth_error = true;
                        }
                    }
                    // Started providers show `...`; one under a server
                    // Retry-After deadline keeps showing its stale value.
                    spawn_poll(SendHwnd::from_hwnd(hwnd), PollTrigger::Manual);
                    render_layered();
                }
                IDM_VERSION_ACTION => {
                    let release = {
                        let state = lock_state();
                        match state.as_ref() {
                            Some(s) => match &s.update_status {
                                UpdateStatus::Available(release) => Some(release.clone()),
                                _ => None,
                            },
                            None => None,
                        }
                    };

                    if let Some(release) = release {
                        begin_update_apply(hwnd, release);
                    } else {
                        begin_update_check(hwnd, true);
                    }
                }
                2 => {
                    EXIT_REQUESTED.store(true, Ordering::Relaxed);
                    let hook = {
                        let state = lock_state();
                        state.as_ref().and_then(|s| s.win_event_hook)
                    };
                    if let Some(h) = hook {
                        native_interop::unhook_win_event(h);
                    }
                    PostQuitMessage(0);
                }
                IDM_RESET_POSITION => {
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.tray_offset = 0;
                            s.manual_position = false;
                        }
                    }
                    save_state_settings();
                    position_at_taskbar();
                }
                IDM_SHOW_DRAG_HANDLE => {
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.show_drag_handle = !s.show_drag_handle;
                        }
                    }
                    save_state_settings();
                    position_at_taskbar();
                    render_layered();
                }
                IDM_ENABLE_CODEX_MCP => {
                    let enabled = {
                        let mut state = lock_state();
                        state.as_mut().map(|s| {
                            s.enable_codex_mcp = !s.enable_codex_mcp;
                            s.enable_codex_mcp
                        })
                    }
                    .unwrap_or(false);
                    save_state_settings();
                    if enabled {
                        if let Err(error) = codex_mcp::start() {
                            diagnose::log_error("Codex MCP integration failed to start", error);
                        }
                    } else {
                        codex_mcp::stop();
                    }
                }
                IDM_OPEN_LOG_FILE => open_log_file(hwnd),
                IDM_START_WITH_WINDOWS => {
                    set_startup_enabled(!is_startup_enabled());
                }
                IDM_FREQ_ADAPTIVE | IDM_FREQ_30SEC | IDM_FREQ_1MIN | IDM_FREQ_5MIN
                | IDM_FREQ_15MIN | IDM_FREQ_1HOUR => {
                    let new_interval = match id {
                        IDM_FREQ_30SEC => POLL_30_SEC,
                        IDM_FREQ_1MIN => POLL_1_MIN,
                        IDM_FREQ_5MIN => POLL_5_MIN,
                        IDM_FREQ_15MIN => POLL_15_MIN,
                        IDM_FREQ_1HOUR => POLL_1_HOUR,
                        IDM_FREQ_ADAPTIVE => POLL_5_MIN,
                        _ => POLL_15_MIN,
                    };
                    let timer_interval = {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.adaptive_polling = id == IDM_FREQ_ADAPTIVE;
                            if !s.adaptive_polling {
                                s.poll_interval_ms = new_interval;
                            }
                            effective_poll_interval_ms(s)
                        } else {
                            new_interval
                        }
                    };
                    save_state_settings();
                    codex_mcp::update_polling_metadata(id == IDM_FREQ_ADAPTIVE, timer_interval);
                    // Idle providers refresh promptly; a provider in backoff,
                    // auth recovery or under a server deadline keeps it.
                    diagnose::log(format!(
                        "poll frequency changed base_interval_ms={timer_interval}; providers in recovery keep their deadlines"
                    ));
                    spawn_poll(SendHwnd::from_hwnd(hwnd), PollTrigger::Reconfigure);
                }
                IDM_USAGE_DISPLAY_MIXED | IDM_USAGE_DISPLAY_REMAINING | IDM_USAGE_DISPLAY_USED => {
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.usage_display = match id {
                                IDM_USAGE_DISPLAY_MIXED => UsageDisplayMode::Mixed,
                                IDM_USAGE_DISPLAY_USED => UsageDisplayMode::Used,
                                _ => UsageDisplayMode::Remaining,
                            };
                            refresh_usage_texts(s);
                        }
                    }
                    save_state_settings();
                    position_at_taskbar();
                    render_layered();
                    sync_tray_icons(hwnd);
                }
                IDM_CREDIT_DISPLAY_ALWAYS
                | IDM_CREDIT_DISPLAY_WHEN_NEEDED
                | IDM_CREDIT_DISPLAY_OFF => {
                    let visibility_changed = {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            let was_visible = codex_credit_panel_visible(
                                s.show_codex,
                                s.credit_display,
                                s.data.as_ref(),
                            );
                            s.credit_display = match id {
                                IDM_CREDIT_DISPLAY_WHEN_NEEDED => CreditDisplayMode::WhenNeeded,
                                IDM_CREDIT_DISPLAY_OFF => CreditDisplayMode::Off,
                                _ => CreditDisplayMode::Always,
                            };
                            was_visible
                                != codex_credit_panel_visible(
                                    s.show_codex,
                                    s.credit_display,
                                    s.data.as_ref(),
                                )
                        } else {
                            false
                        }
                    };
                    save_state_settings();
                    if visibility_changed {
                        position_at_taskbar();
                    }
                    render_layered();
                }
                IDM_CREDIT_POSITION_LEFT | IDM_CREDIT_POSITION_RIGHT => {
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.credit_position = if id == IDM_CREDIT_POSITION_RIGHT {
                                CreditPosition::Right
                            } else {
                                CreditPosition::Left
                            };
                        }
                    }
                    save_state_settings();
                    position_at_taskbar();
                    render_layered();
                }
                IDM_CREDIT_VALUE_CREDITS | IDM_CREDIT_VALUE_USD => {
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.credit_value_mode = if id == IDM_CREDIT_VALUE_USD {
                                CreditValueMode::UsdEstimate
                            } else {
                                CreditValueMode::Credits
                            };
                            refresh_usage_texts(s);
                        }
                    }
                    save_state_settings();
                    position_at_taskbar();
                    render_layered();
                }
                IDM_BAR_COLOR_WINDOWS_ACCENT => {
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.bar_color = None;
                        }
                    }
                    save_state_settings();
                    position_at_taskbar();
                    render_layered();
                }
                IDM_BAR_COLOR_CUSTOM => {
                    let initial = {
                        let state = lock_state();
                        state
                            .as_ref()
                            .map(|s| selected_bar_color(s.bar_color.as_deref()))
                            .unwrap_or_else(native_interop::windows_accent_color)
                    };
                    if let Some(color) = native_interop::choose_custom_color(hwnd, initial) {
                        {
                            let mut state = lock_state();
                            if let Some(s) = state.as_mut() {
                                s.bar_color = Some(canonical_hex_color(color));
                            }
                        }
                        save_state_settings();
                        render_layered();
                    }
                }
                IDM_SHOW_SESSION_WINDOW | IDM_SHOW_WEEKLY_WINDOW => {
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            match id {
                                IDM_SHOW_SESSION_WINDOW
                                    if s.show_weekly_window || !s.show_session_window =>
                                {
                                    s.show_session_window = !s.show_session_window;
                                }
                                IDM_SHOW_WEEKLY_WINDOW
                                    if s.show_session_window || !s.show_weekly_window =>
                                {
                                    s.show_weekly_window = !s.show_weekly_window;
                                }
                                _ => {}
                            }
                        }
                    }
                    save_state_settings();
                    render_layered();
                    sync_tray_icons(hwnd);
                }
                IDM_ALERT_OFF | IDM_ALERT_2 | IDM_ALERT_5 | IDM_ALERT_10 | IDM_ALERT_20
                | IDM_ALERT_30 => {
                    let threshold = match id {
                        IDM_ALERT_2 => 2,
                        IDM_ALERT_5 => 5,
                        IDM_ALERT_10 => 10,
                        IDM_ALERT_20 => 20,
                        IDM_ALERT_30 => 30,
                        _ => 0,
                    };
                    let alerts = {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            if threshold == 0 {
                                s.alert_thresholds_percent.clear();
                                s.notified_quota_windows.clear();
                                Vec::new()
                            } else {
                                if let Some(index) = s
                                    .alert_thresholds_percent
                                    .iter()
                                    .position(|selected| *selected == threshold)
                                {
                                    s.alert_thresholds_percent.remove(index);
                                } else {
                                    s.alert_thresholds_percent.push(threshold);
                                    s.alert_thresholds_percent.sort_unstable();
                                }
                                if s.alert_thresholds_percent.is_empty() {
                                    s.notified_quota_windows.clear();
                                    Vec::new()
                                } else if let Some(data) = s.data.clone() {
                                    collect_low_quota_alerts(s, &data)
                                } else {
                                    Vec::new()
                                }
                            }
                        } else {
                            Vec::new()
                        }
                    };
                    for alert in &alerts {
                        tray_icon::notify_balloon(hwnd, alert.kind, &alert.title, &alert.message);
                    }
                    save_state_settings();
                }
                IDM_MODEL_CLAUDE_CODE | IDM_MODEL_CODEX | IDM_MODEL_ANTIGRAVITY => {
                    let codex_disabled = {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            match id {
                                IDM_MODEL_CLAUDE_CODE => {
                                    if s.claude_code_available
                                        && (s.show_codex
                                            || s.show_antigravity
                                            || !s.show_claude_code)
                                    {
                                        s.show_claude_code = !s.show_claude_code;
                                    }
                                }
                                IDM_MODEL_CODEX => {
                                    if s.show_claude_code || s.show_antigravity || !s.show_codex {
                                        s.show_codex = !s.show_codex;
                                    }
                                }
                                IDM_MODEL_ANTIGRAVITY => {
                                    if s.show_claude_code || s.show_codex || !s.show_antigravity {
                                        s.show_antigravity = !s.show_antigravity;
                                    }
                                }
                                _ => {}
                            }
                            let codex_disabled = id == IDM_MODEL_CODEX && !s.show_codex;
                            if codex_disabled {
                                let _ = finish_codex_auth_episode(&mut s.codex_auth);
                            }
                            s.provider_generation = s.provider_generation.wrapping_add(1);
                            let enabled = enabled_providers(s);
                            for (id, enabled) in enabled.into_iter().enumerate() {
                                if !enabled {
                                    s.provider_health[id] = Default::default();
                                    s.data_sources[id] = Default::default();
                                    s.persisted[id] = None;
                                    // Keeps any server Retry-After deadline.
                                    s.schedule.disable(id);
                                    if let Some(data) = s.data.as_mut() {
                                        *provider_usage_mut(data, id) = None;
                                    }
                                }
                            }
                            codex_mcp::set_monitoring_enabled(s.show_codex, s.show_claude_code);
                            s.session_text = "...".to_string();
                            s.weekly_text = "...".to_string();
                            s.codex_session_text = "...".to_string();
                            s.codex_weekly_text = "...".to_string();
                            s.antigravity_session_text = "...".to_string();
                            s.antigravity_weekly_text = "...".to_string();
                            // Providers with retained data keep showing it; a
                            // provider that is not re-polled yet stays `...`.
                            refresh_usage_texts(s);
                            codex_disabled
                        } else {
                            false
                        }
                    };
                    if codex_disabled {
                        let _ = KillTimer(hwnd, TIMER_CREDENTIAL_WATCH);
                        diagnose::log(
                            "Codex auth recovery ended because Codex polling was disabled",
                        );
                    }
                    save_state_settings();
                    position_at_taskbar();
                    render_layered();
                    sync_tray_icons(hwnd);
                    spawn_poll(SendHwnd::from_hwnd(hwnd), PollTrigger::Reconfigure);
                }
                IDM_LANG_SYSTEM
                | IDM_LANG_ENGLISH
                | IDM_LANG_DUTCH
                | IDM_LANG_SPANISH
                | IDM_LANG_FRENCH
                | IDM_LANG_GERMAN
                | IDM_LANG_JAPANESE
                | IDM_LANG_KOREAN
                | IDM_LANG_SIMPLIFIED_CHINESE
                | IDM_LANG_TRADITIONAL_CHINESE
                | IDM_LANG_RUSSIAN
                | IDM_LANG_PORTUGUESE_BRAZIL => {
                    let language_override = match id {
                        IDM_LANG_SYSTEM => None,
                        IDM_LANG_ENGLISH => Some(LanguageId::English),
                        IDM_LANG_DUTCH => Some(LanguageId::Dutch),
                        IDM_LANG_SPANISH => Some(LanguageId::Spanish),
                        IDM_LANG_FRENCH => Some(LanguageId::French),
                        IDM_LANG_GERMAN => Some(LanguageId::German),
                        IDM_LANG_JAPANESE => Some(LanguageId::Japanese),
                        IDM_LANG_KOREAN => Some(LanguageId::Korean),
                        IDM_LANG_SIMPLIFIED_CHINESE => Some(LanguageId::SimplifiedChinese),
                        IDM_LANG_TRADITIONAL_CHINESE => Some(LanguageId::TraditionalChinese),
                        IDM_LANG_RUSSIAN => Some(LanguageId::Russian),
                        IDM_LANG_PORTUGUESE_BRAZIL => Some(LanguageId::PortugueseBrazil),
                        _ => None,
                    };
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            apply_language_to_state(s, language_override);
                        }
                    }
                    save_state_settings();
                    position_at_taskbar();
                    render_layered();
                }
                id if id == tray_icon::IDM_TOGGLE_WIDGET => {
                    toggle_widget_visibility(hwnd);
                }
                _ => {}
            }
            LRESULT(0)
        }
        _ if msg == WM_APP_TRAY => {
            match tray_icon::handle_message(lparam) {
                tray_icon::TrayAction::ToggleWidget => {
                    toggle_widget_visibility(hwnd);
                }
                tray_icon::TrayAction::ShowContextMenu => {
                    show_context_menu(hwnd);
                }
                tray_icon::TrayAction::None => {}
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let hook = {
                let state = lock_state();
                state.as_ref().and_then(|s| s.win_event_hook)
            };
            if let Some(h) = hook {
                native_interop::unhook_win_event(h);
            }
            tray_icon::remove_all(hwnd);
            if EXIT_REQUESTED.load(Ordering::Relaxed) {
                PostQuitMessage(0);
            } else {
                // Disconnecting a display destroys its taskbar and our child
                // window. Keep the message loop alive so the watchdog can
                // relaunch the widget on a remaining taskbar.
                diagnose::log(
                    "widget window destroyed unexpectedly; waiting for watchdog recovery",
                );
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

struct ShellOpenLogFileCommand {
    target: PathBuf,
    file_exists: bool,
    operation_wide: Vec<u16>,
    file_wide: Vec<u16>,
    parameters_wide: Option<Vec<u16>>,
    working_directory: PathBuf,
    working_directory_wide: Vec<u16>,
}

fn encode_windows_path(path: &std::path::Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn shell_open_log_file_command(
    log_path: &std::path::Path,
    file_exists: bool,
) -> ShellOpenLogFileCommand {
    let target = diagnose::log_open_target(log_path, file_exists);
    let working_directory = target.parent().unwrap_or(&target).to_path_buf();

    ShellOpenLogFileCommand {
        operation_wide: native_interop::wide_str("open"),
        file_wide: encode_windows_path(&target),
        parameters_wide: None,
        working_directory_wide: encode_windows_path(&working_directory),
        target,
        file_exists,
        working_directory,
    }
}

fn open_log_file(hwnd: HWND) {
    let Some(log_path) = diagnose::active_log_file_path() else {
        diagnose::log("Open log file skipped: Local application data directory is unavailable");
        return;
    };
    let command = shell_open_log_file_command(&log_path, log_path.is_file());
    diagnose::log(format!(
        "Open log file requested logger_path={} path_source={} target={} exists={} api=ShellExecuteW verb=open parameters=null working_directory={} file_quoted=false write_failures={}",
        log_path.display(),
        if diagnose::is_enabled() {
            "active_logger"
        } else {
            "local_app_data_fallback"
        },
        command.target.display(),
        command.file_exists,
        command.working_directory.display(),
        diagnose::write_failure_count()
    ));

    let parameters = command
        .parameters_wide
        .as_ref()
        .map_or_else(PCWSTR::null, |value| PCWSTR::from_raw(value.as_ptr()));
    let result = unsafe {
        ShellExecuteW(
            hwnd,
            PCWSTR::from_raw(command.operation_wide.as_ptr()),
            PCWSTR::from_raw(command.file_wide.as_ptr()),
            parameters,
            PCWSTR::from_raw(command.working_directory_wide.as_ptr()),
            SW_SHOWNORMAL,
        )
    };
    if result.0 as usize <= 32 {
        diagnose::log(format!(
            "Open log file shell request failed target={} ShellExecuteW status={}",
            command.target.display(),
            result.0 as usize
        ));
    } else {
        diagnose::log(format!(
            "Open log file shell request accepted target={}",
            command.target.display()
        ));
    }
}

fn show_context_menu(hwnd: HWND) {
    unsafe {
        let (
            current_interval,
            adaptive_polling,
            strings,
            language,
            language_override,
            update_status,
            widget_visible,
            show_claude_code,
            claude_code_available,
            show_codex,
            show_antigravity,
            show_session_window,
            show_weekly_window,
            show_drag_handle,
            usage_display,
            credit_display,
            credit_position,
            credit_value_mode,
            bar_color,
            alert_thresholds_percent,
            enable_codex_mcp,
        ) = {
            let state = lock_state();
            match state.as_ref() {
                Some(s) => (
                    s.poll_interval_ms,
                    s.adaptive_polling,
                    s.language.strings(),
                    s.language,
                    s.language_override,
                    s.update_status.clone(),
                    s.widget_visible,
                    s.show_claude_code,
                    s.claude_code_available,
                    s.show_codex,
                    s.show_antigravity,
                    s.show_session_window,
                    s.show_weekly_window,
                    s.show_drag_handle,
                    s.usage_display,
                    s.credit_display,
                    s.credit_position,
                    s.credit_value_mode,
                    s.bar_color.clone(),
                    s.alert_thresholds_percent.clone(),
                    s.enable_codex_mcp,
                ),
                None => (
                    POLL_15_MIN,
                    false,
                    LanguageId::English.strings(),
                    LanguageId::English,
                    None,
                    UpdateStatus::Idle,
                    true,
                    true,
                    false,
                    false,
                    false,
                    true,
                    true,
                    false,
                    UsageDisplayMode::Remaining,
                    CreditDisplayMode::Always,
                    CreditPosition::Left,
                    CreditValueMode::Credits,
                    None,
                    Vec::new(),
                    false,
                ),
            }
        };

        let menu = CreatePopupMenu().unwrap();

        let refresh_str = native_interop::wide_str(strings.refresh);
        let _ = AppendMenuW(
            menu,
            MENU_ITEM_FLAGS(0),
            1,
            PCWSTR::from_raw(refresh_str.as_ptr()),
        );

        // Update Frequency submenu
        let freq_menu = CreatePopupMenu().unwrap();
        let adaptive_label = native_interop::wide_str(strings.adaptive);
        let _ = AppendMenuW(
            freq_menu,
            if adaptive_polling {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            },
            IDM_FREQ_ADAPTIVE as usize,
            PCWSTR::from_raw(adaptive_label.as_ptr()),
        );
        let freq_items: [(u16, u32, &str); 5] = [
            (
                IDM_FREQ_30SEC,
                POLL_30_SEC,
                if language == LanguageId::SimplifiedChinese {
                    "30秒"
                } else {
                    "30 Seconds"
                },
            ),
            (IDM_FREQ_1MIN, POLL_1_MIN, strings.one_minute),
            (IDM_FREQ_5MIN, POLL_5_MIN, strings.five_minutes),
            (IDM_FREQ_15MIN, POLL_15_MIN, strings.fifteen_minutes),
            (IDM_FREQ_1HOUR, POLL_1_HOUR, strings.one_hour),
        ];
        for (id, interval, label) in freq_items {
            let label_str = native_interop::wide_str(label);
            let flags = if !adaptive_polling && interval == current_interval {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            };
            let _ = AppendMenuW(
                freq_menu,
                flags,
                id as usize,
                PCWSTR::from_raw(label_str.as_ptr()),
            );
        }

        let freq_label = native_interop::wide_str(strings.update_frequency);
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            freq_menu.0 as usize,
            PCWSTR::from_raw(freq_label.as_ptr()),
        );

        // Models submenu
        let models_menu = CreatePopupMenu().unwrap();
        let claude_label = claude_code_menu_label(strings, language, claude_code_available);
        let claude_model = native_interop::wide_str(&claude_label);
        let claude_flags = if !claude_code_available {
            MF_GRAYED
        } else if show_claude_code {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            models_menu,
            claude_flags,
            IDM_MODEL_CLAUDE_CODE as usize,
            PCWSTR::from_raw(claude_model.as_ptr()),
        );

        let codex_model = native_interop::wide_str(strings.codex_model);
        let codex_flags = if show_codex {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            models_menu,
            codex_flags,
            IDM_MODEL_CODEX as usize,
            PCWSTR::from_raw(codex_model.as_ptr()),
        );

        let antigravity_model = native_interop::wide_str(strings.antigravity_model);
        let antigravity_flags = if show_antigravity {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            models_menu,
            antigravity_flags,
            IDM_MODEL_ANTIGRAVITY as usize,
            PCWSTR::from_raw(antigravity_model.as_ptr()),
        );

        let models_label = native_interop::wide_str(strings.models);
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            models_menu.0 as usize,
            PCWSTR::from_raw(models_label.as_ptr()),
        );

        // Usage window visibility submenu. Keep at least one window enabled.
        let usage_menu = CreatePopupMenu().unwrap();
        let session_label =
            native_interop::wide_str(if language == LanguageId::SimplifiedChinese {
                "5 小时额度"
            } else {
                "5-hour quota"
            });
        let session_flags = if show_session_window {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            usage_menu,
            session_flags,
            IDM_SHOW_SESSION_WINDOW as usize,
            PCWSTR::from_raw(session_label.as_ptr()),
        );
        let weekly_label = native_interop::wide_str(if language == LanguageId::SimplifiedChinese {
            "每周额度"
        } else {
            "Weekly quota"
        });
        let weekly_flags = if show_weekly_window {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            usage_menu,
            weekly_flags,
            IDM_SHOW_WEEKLY_WINDOW as usize,
            PCWSTR::from_raw(weekly_label.as_ptr()),
        );
        let _ = AppendMenuW(usage_menu, MF_SEPARATOR, 0, PCWSTR::null());
        let mixed_label = native_interop::wide_str(if language == LanguageId::SimplifiedChinese {
            "默认（混合）"
        } else {
            "Default (Mixed)"
        });
        let mixed_flags = if usage_display == UsageDisplayMode::Mixed {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            usage_menu,
            mixed_flags,
            IDM_USAGE_DISPLAY_MIXED as usize,
            PCWSTR::from_raw(mixed_label.as_ptr()),
        );
        let remaining_label =
            native_interop::wide_str(if language == LanguageId::SimplifiedChinese {
                "剩余"
            } else {
                "Remaining"
            });
        let remaining_flags = if usage_display == UsageDisplayMode::Remaining {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            usage_menu,
            remaining_flags,
            IDM_USAGE_DISPLAY_REMAINING as usize,
            PCWSTR::from_raw(remaining_label.as_ptr()),
        );
        let used_label = native_interop::wide_str(if language == LanguageId::SimplifiedChinese {
            "已用"
        } else {
            "Used"
        });
        let used_flags = if usage_display == UsageDisplayMode::Used {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            usage_menu,
            used_flags,
            IDM_USAGE_DISPLAY_USED as usize,
            PCWSTR::from_raw(used_label.as_ptr()),
        );
        let usage_label = native_interop::wide_str(if language == LanguageId::SimplifiedChinese {
            "显示用量"
        } else {
            "Usage display"
        });
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            usage_menu.0 as usize,
            PCWSTR::from_raw(usage_label.as_ptr()),
        );

        let credit_menu = CreatePopupMenu().unwrap();
        let credit_items = [
            (
                IDM_CREDIT_DISPLAY_ALWAYS,
                CreditDisplayMode::Always,
                strings.credit_always,
            ),
            (
                IDM_CREDIT_DISPLAY_WHEN_NEEDED,
                CreditDisplayMode::WhenNeeded,
                strings.credit_when_needed,
            ),
            (
                IDM_CREDIT_DISPLAY_OFF,
                CreditDisplayMode::Off,
                strings.credit_off,
            ),
        ];
        for (id, mode, label) in credit_items {
            let label = native_interop::wide_str(label);
            let flags = if credit_display == mode {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            };
            let _ = AppendMenuW(
                credit_menu,
                flags,
                id as usize,
                PCWSTR::from_raw(label.as_ptr()),
            );
        }
        let _ = AppendMenuW(credit_menu, MF_SEPARATOR, 0, PCWSTR::null());
        let position_items = [
            (
                IDM_CREDIT_POSITION_LEFT,
                CreditPosition::Left,
                strings.credit_left,
            ),
            (
                IDM_CREDIT_POSITION_RIGHT,
                CreditPosition::Right,
                strings.credit_right,
            ),
        ];
        for (id, position, label) in position_items {
            let label = native_interop::wide_str(label);
            let flags = if credit_position == position {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            };
            let _ = AppendMenuW(
                credit_menu,
                flags,
                id as usize,
                PCWSTR::from_raw(label.as_ptr()),
            );
        }
        let _ = AppendMenuW(credit_menu, MF_SEPARATOR, 0, PCWSTR::null());
        let credit_value_items = [
            (
                IDM_CREDIT_VALUE_CREDITS,
                CreditValueMode::Credits,
                strings.credits,
            ),
            (
                IDM_CREDIT_VALUE_USD,
                CreditValueMode::UsdEstimate,
                strings.credit_usd_estimate,
            ),
        ];
        for (id, mode, label) in credit_value_items {
            let label = native_interop::wide_str(label);
            let flags = if credit_value_mode == mode {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            };
            let _ = AppendMenuW(
                credit_menu,
                flags,
                id as usize,
                PCWSTR::from_raw(label.as_ptr()),
            );
        }
        let credit_label = native_interop::wide_str(strings.credit_display);
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            credit_menu.0 as usize,
            PCWSTR::from_raw(credit_label.as_ptr()),
        );

        // Low-quota alert thresholds. Each threshold is independently selectable.
        let alert_menu = CreatePopupMenu().unwrap();
        let off_label = native_interop::wide_str(if language == LanguageId::SimplifiedChinese {
            "关闭"
        } else {
            "Off"
        });
        let _ = AppendMenuW(
            alert_menu,
            if alert_thresholds_percent.is_empty() {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            },
            IDM_ALERT_OFF as usize,
            PCWSTR::from_raw(off_label.as_ptr()),
        );
        let _ = AppendMenuW(alert_menu, MF_SEPARATOR, 0, PCWSTR::null());
        let alert_items = [
            (IDM_ALERT_2, 2u8),
            (IDM_ALERT_5, 5u8),
            (IDM_ALERT_10, 10u8),
            (IDM_ALERT_20, 20u8),
            (IDM_ALERT_30, 30u8),
        ];
        for (id, threshold) in alert_items {
            let label = if language == LanguageId::SimplifiedChinese {
                format!("剩余 {threshold}%")
            } else {
                format!("{threshold}% remaining")
            };
            let label = native_interop::wide_str(&label);
            let flags = if alert_thresholds_percent.contains(&threshold) {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            };
            let _ = AppendMenuW(
                alert_menu,
                flags,
                id as usize,
                PCWSTR::from_raw(label.as_ptr()),
            );
        }
        let alert_label = native_interop::wide_str(if language == LanguageId::SimplifiedChinese {
            "额度提醒"
        } else {
            "Quota alerts"
        });
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            alert_menu.0 as usize,
            PCWSTR::from_raw(alert_label.as_ptr()),
        );

        // Settings submenu
        let settings_menu = CreatePopupMenu().unwrap();

        let startup_str = native_interop::wide_str(strings.start_with_windows);
        let startup_flags = if is_startup_enabled() {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            settings_menu,
            startup_flags,
            IDM_START_WITH_WINDOWS as usize,
            PCWSTR::from_raw(startup_str.as_ptr()),
        );

        let drag_handle_str = native_interop::wide_str(strings.show_drag_handle);
        let drag_handle_flags = if show_drag_handle {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            settings_menu,
            drag_handle_flags,
            IDM_SHOW_DRAG_HANDLE as usize,
            PCWSTR::from_raw(drag_handle_str.as_ptr()),
        );

        let reset_pos_str = native_interop::wide_str(strings.reset_position);
        let _ = AppendMenuW(
            settings_menu,
            MENU_ITEM_FLAGS(0),
            IDM_RESET_POSITION as usize,
            PCWSTR::from_raw(reset_pos_str.as_ptr()),
        );

        let codex_mcp_label = native_interop::wide_str(strings.enable_codex_mcp);
        let codex_mcp_flags = if enable_codex_mcp {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            settings_menu,
            codex_mcp_flags,
            IDM_ENABLE_CODEX_MCP as usize,
            PCWSTR::from_raw(codex_mcp_label.as_ptr()),
        );

        let bar_color_menu = CreatePopupMenu().unwrap();
        let windows_accent_label =
            native_interop::wide_str(if language == LanguageId::SimplifiedChinese {
                "Windows 强调色"
            } else {
                "Windows accent"
            });
        let windows_accent_flags = if bar_color.is_none() {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            bar_color_menu,
            windows_accent_flags,
            IDM_BAR_COLOR_WINDOWS_ACCENT as usize,
            PCWSTR::from_raw(windows_accent_label.as_ptr()),
        );
        let custom_color_label =
            native_interop::wide_str(if language == LanguageId::SimplifiedChinese {
                "自定义..."
            } else {
                "Custom..."
            });
        let custom_color_flags = if bar_color.is_some() {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            bar_color_menu,
            custom_color_flags,
            IDM_BAR_COLOR_CUSTOM as usize,
            PCWSTR::from_raw(custom_color_label.as_ptr()),
        );
        let bar_color_label =
            native_interop::wide_str(if language == LanguageId::SimplifiedChinese {
                "进度条颜色"
            } else {
                "Bar color"
            });
        let _ = AppendMenuW(
            settings_menu,
            MF_POPUP,
            bar_color_menu.0 as usize,
            PCWSTR::from_raw(bar_color_label.as_ptr()),
        );

        let language_menu = CreatePopupMenu().unwrap();
        let system_label = native_interop::wide_str(strings.system_default);
        let system_flags = if language_override.is_none() {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            language_menu,
            system_flags,
            IDM_LANG_SYSTEM as usize,
            PCWSTR::from_raw(system_label.as_ptr()),
        );

        for language in LanguageId::ALL {
            let id = match language {
                LanguageId::English => IDM_LANG_ENGLISH,
                LanguageId::Dutch => IDM_LANG_DUTCH,
                LanguageId::Spanish => IDM_LANG_SPANISH,
                LanguageId::French => IDM_LANG_FRENCH,
                LanguageId::German => IDM_LANG_GERMAN,
                LanguageId::Japanese => IDM_LANG_JAPANESE,
                LanguageId::Korean => IDM_LANG_KOREAN,
                LanguageId::SimplifiedChinese => IDM_LANG_SIMPLIFIED_CHINESE,
                LanguageId::TraditionalChinese => IDM_LANG_TRADITIONAL_CHINESE,
                LanguageId::Russian => IDM_LANG_RUSSIAN,
                LanguageId::PortugueseBrazil => IDM_LANG_PORTUGUESE_BRAZIL,
            };
            let label_str = native_interop::wide_str(language.native_name());
            let flags = if language_override == Some(language) {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            };
            let _ = AppendMenuW(
                language_menu,
                flags,
                id as usize,
                PCWSTR::from_raw(label_str.as_ptr()),
            );
        }

        let language_label = native_interop::wide_str(strings.language);
        let _ = AppendMenuW(
            settings_menu,
            MF_POPUP,
            language_menu.0 as usize,
            PCWSTR::from_raw(language_label.as_ptr()),
        );

        let _ = AppendMenuW(settings_menu, MF_SEPARATOR, 0, PCWSTR::null());

        let identity_label = native_interop::wide_str(&build_info::identity());
        let _ = AppendMenuW(
            settings_menu,
            MF_GRAYED,
            0,
            PCWSTR::from_raw(identity_label.as_ptr()),
        );

        let version_label = version_action_label(strings, &update_status);
        let version_str = native_interop::wide_str(&version_label);
        let version_flags = if matches!(
            update_status,
            UpdateStatus::Checking | UpdateStatus::Applying
        ) {
            MF_GRAYED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            settings_menu,
            version_flags,
            IDM_VERSION_ACTION as usize,
            PCWSTR::from_raw(version_str.as_ptr()),
        );

        let open_log_label = native_interop::wide_str(strings.open_log_file);
        let _ = AppendMenuW(
            settings_menu,
            MENU_ITEM_FLAGS(0),
            IDM_OPEN_LOG_FILE as usize,
            PCWSTR::from_raw(open_log_label.as_ptr()),
        );

        let settings_label = native_interop::wide_str(strings.settings);
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            settings_menu.0 as usize,
            PCWSTR::from_raw(settings_label.as_ptr()),
        );

        let widget_label = native_interop::wide_str(strings.show_widget);
        let widget_flags = if widget_visible {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            menu,
            widget_flags,
            tray_icon::IDM_TOGGLE_WIDGET as usize,
            PCWSTR::from_raw(widget_label.as_ptr()),
        );

        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

        let exit_str = native_interop::wide_str(strings.exit);
        let _ = AppendMenuW(
            menu,
            MENU_ITEM_FLAGS(0),
            2,
            PCWSTR::from_raw(exit_str.as_ptr()),
        );

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, None);
        let _ = DestroyMenu(menu);
    }
}

/// Paint for non-embedded fallback (normal WM_PAINT path)
fn paint(hdc: HDC, hwnd: HWND) {
    let (
        is_dark,
        language,
        strings,
        session_pct,
        session_text,
        weekly_pct,
        weekly_text,
        codex_session_pct,
        codex_session_text,
        codex_weekly_pct,
        codex_weekly_text,
        antigravity_session_pct,
        antigravity_session_text,
        antigravity_weekly_pct,
        antigravity_weekly_text,
        show_claude_code,
        show_codex,
        show_antigravity,
        show_session_window,
        show_weekly_window,
        show_drag_handle,
        directions,
        credit_panel_visible,
        credit_position,
        credit_value_mode,
        codex_credit_text,
        bar_color_setting,
    ) = {
        let state = lock_state();
        match state.as_ref() {
            Some(s) => (
                s.is_dark,
                s.language,
                s.language.strings(),
                s.session_percent,
                s.session_text.clone(),
                s.weekly_percent,
                s.weekly_text.clone(),
                s.codex_session_percent,
                s.codex_session_text.clone(),
                s.codex_weekly_percent,
                s.codex_weekly_text.clone(),
                s.antigravity_session_percent,
                s.antigravity_session_text.clone(),
                s.antigravity_weekly_percent,
                s.antigravity_weekly_text.clone(),
                s.show_claude_code,
                s.show_codex,
                s.show_antigravity,
                s.show_session_window,
                s.show_weekly_window,
                s.show_drag_handle,
                s.usage_display.directions(),
                codex_credit_panel_visible(s.show_codex, s.credit_display, s.data.as_ref()),
                s.credit_position,
                s.credit_value_mode,
                s.codex_credit_text.clone(),
                s.bar_color.clone(),
            ),
            None => return,
        }
    };

    let bar_color = selected_bar_color(bar_color_setting.as_deref());
    let track = if is_dark {
        Color::from_hex("#444444")
    } else {
        Color::from_hex("#AAAAAA")
    };
    let text_color = if is_dark {
        Color::from_hex("#888888")
    } else {
        Color::from_hex("#404040")
    };
    let bg_color = if is_dark {
        Color::from_hex("#1C1C1C")
    } else {
        Color::from_hex("#F3F3F3")
    };

    unsafe {
        let mut client_rect = RECT::default();
        let _ = GetClientRect(hwnd, &mut client_rect);
        let width = client_rect.right - client_rect.left;
        let height = client_rect.bottom - client_rect.top;

        if width <= 0 || height <= 0 {
            return;
        }

        let mem_dc = CreateCompatibleDC(hdc);
        let mem_bmp = CreateCompatibleBitmap(hdc, width, height);
        let old_bmp = SelectObject(mem_dc, mem_bmp);

        paint_content(
            mem_dc,
            width,
            height,
            is_dark,
            &bg_color,
            &text_color,
            &bar_color,
            &track,
            language,
            strings,
            session_pct,
            &session_text,
            weekly_pct,
            &weekly_text,
            codex_session_pct,
            &codex_session_text,
            codex_weekly_pct,
            &codex_weekly_text,
            antigravity_session_pct,
            &antigravity_session_text,
            antigravity_weekly_pct,
            &antigravity_weekly_text,
            show_claude_code,
            show_codex,
            show_antigravity,
            show_session_window,
            show_weekly_window,
            show_drag_handle,
            directions,
            credit_panel_visible,
            credit_position,
            credit_value_mode,
            &codex_credit_text,
        );

        let _ = BitBlt(hdc, 0, 0, width, height, mem_dc, 0, 0, SRCCOPY);

        SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(mem_bmp);
        let _ = DeleteDC(mem_dc);
    }
}

fn draw_credit_panel(
    hdc: HDC,
    x: i32,
    header_y: i32,
    value_y: i32,
    header: &str,
    value: &str,
    text_color: &Color,
    header_width: i32,
    value_width: i32,
    credit_position: CreditPosition,
) {
    unsafe {
        let _ = SetTextColor(hdc, COLORREF(text_color.to_colorref()));
        let value_x = credit_value_x(x, header_width, value_width, credit_position);
        for (text, text_x, text_width, y) in [
            (header, x, header_width, header_y),
            (value, value_x, value_width, value_y),
        ] {
            let mut text_wide: Vec<u16> = text.encode_utf16().collect();
            let mut text_rect = RECT {
                left: text_x,
                top: y,
                right: text_x + sc(text_width),
                bottom: y + sc(SEGMENT_H),
            };
            let _ = DrawTextW(
                hdc,
                &mut text_wide,
                &mut text_rect,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE,
            );
        }
    }
}

fn draw_row(
    hdc: HDC,
    x: i32,
    y: i32,
    is_dark: bool,
    text_color: &Color,
    label: &str,
    claude_percent: f64,
    claude_text: &str,
    codex_percent: f64,
    codex_text: &str,
    antigravity_percent: f64,
    antigravity_text: &str,
    show_claude_code: bool,
    show_codex: bool,
    show_antigravity: bool,
    bar_color: &Color,
    track: &Color,
    label_width: i32,
    text_width: i32,
) {
    let seg_h = sc(SEGMENT_H);
    let active_models = active_model_count(show_claude_code, show_codex, show_antigravity);
    let segment_count = row_bar_segment_count(active_models);
    let use_model_text_colors = active_models > 1;
    let claude_value_color = if use_model_text_colors {
        claude_usage_text_color(is_dark)
    } else {
        *text_color
    };
    let codex_value_color = if use_model_text_colors {
        codex_usage_text_color(is_dark)
    } else {
        *text_color
    };
    let antigravity_value_color = if use_model_text_colors {
        antigravity_usage_text_color(is_dark)
    } else {
        *text_color
    };

    unsafe {
        let _ = SetTextColor(hdc, COLORREF(text_color.to_colorref()));
        let mut label_wide: Vec<u16> = label.encode_utf16().collect();
        let mut label_rect = RECT {
            left: x,
            top: y,
            right: x + sc(label_width),
            bottom: y + seg_h,
        };
        let _ = DrawTextW(
            hdc,
            &mut label_wide,
            &mut label_rect,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );

        let mut model_x = x + sc(label_width) + sc(HORIZONTAL_GUTTER);
        if show_claude_code {
            draw_usage_bar(
                hdc,
                model_x,
                y,
                segment_count,
                claude_percent,
                claude_text,
                bar_color,
                track,
                &claude_value_color,
                text_width,
            );
            model_x += model_usage_width(segment_count, text_width) + sc(HORIZONTAL_GUTTER);
        }
        if show_codex {
            draw_usage_bar(
                hdc,
                model_x,
                y,
                segment_count,
                codex_percent,
                codex_text,
                bar_color,
                track,
                &codex_value_color,
                text_width,
            );
            model_x += model_usage_width(segment_count, text_width) + sc(HORIZONTAL_GUTTER);
        }
        if show_antigravity {
            draw_usage_bar(
                hdc,
                model_x,
                y,
                segment_count,
                antigravity_percent,
                antigravity_text,
                bar_color,
                track,
                &antigravity_value_color,
                text_width,
            );
        }
    }
}

fn model_usage_width(segment_count: i32, text_width: i32) -> i32 {
    (sc(SEGMENT_W) + sc(SEGMENT_GAP)) * segment_count - sc(SEGMENT_GAP)
        + sc(HORIZONTAL_GUTTER)
        + sc(text_width)
}

fn draw_usage_bar(
    hdc: HDC,
    bar_x: i32,
    y: i32,
    segment_count: i32,
    percent: f64,
    text: &str,
    bar_color: &Color,
    track: &Color,
    text_color: &Color,
    text_width: i32,
) {
    let seg_w = sc(SEGMENT_W);
    let seg_h = sc(SEGMENT_H);
    let seg_gap = sc(SEGMENT_GAP);
    let bar_width = segment_count * (seg_w + seg_gap) - seg_gap;
    let corner_r = seg_h / 2;

    unsafe {
        let percent_clamped = percent.clamp(0.0, 100.0);
        let bar_rect = RECT {
            left: bar_x,
            top: y,
            right: bar_x + bar_width,
            bottom: y + seg_h,
        };
        draw_rounded_rect(hdc, &bar_rect, track, corner_r);

        let fill_width = (bar_width as f64 * percent_clamped / 100.0).round() as i32;
        if fill_width > 0 {
            let fill_rect = RECT {
                left: bar_x,
                top: y,
                right: bar_x + fill_width,
                bottom: y + seg_h,
            };
            let rgn = CreateRoundRectRgn(
                bar_rect.left,
                bar_rect.top,
                bar_rect.right + 1,
                bar_rect.bottom + 1,
                corner_r * 2,
                corner_r * 2,
            );
            let _ = SelectClipRgn(hdc, rgn);
            let brush = CreateSolidBrush(COLORREF(bar_color.to_colorref()));
            FillRect(hdc, &fill_rect, brush);
            let _ = DeleteObject(brush);
            let _ = SelectClipRgn(hdc, HRGN::default());
            let _ = DeleteObject(rgn);
        }

        let text_x = bar_x + bar_width + sc(HORIZONTAL_GUTTER);
        let mut text_wide: Vec<u16> = text.encode_utf16().collect();
        let mut text_rect = RECT {
            left: text_x,
            top: y,
            right: text_x + sc(text_width),
            bottom: y + seg_h,
        };
        let _ = SetTextColor(hdc, COLORREF(text_color.to_colorref()));
        let _ = DrawTextW(
            hdc,
            &mut text_wide,
            &mut text_rect,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );
    }
}

fn draw_rounded_rect(hdc: HDC, rect: &RECT, color: &Color, radius: i32) {
    unsafe {
        let brush = CreateSolidBrush(COLORREF(color.to_colorref()));
        let rgn = CreateRoundRectRgn(
            rect.left,
            rect.top,
            rect.right + 1,
            rect.bottom + 1,
            radius * 2,
            radius * 2,
        );
        let _ = FillRgn(hdc, rgn, brush);
        let _ = DeleteObject(rgn);
        let _ = DeleteObject(brush);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_prefix_preserves_width_and_auxiliary_anchors() {
        for text in ["88% 1h04m", "12% 1h04m", "100% 5h", "5% 1d23h"] {
            let fresh = quota_text_width_for(LanguageId::English, &[text]);
            let stale = quota_text_width_for(LanguageId::English, &[&format!("!{text}")]);
            assert_eq!(fresh, stale);
            for dpi in [96, 120, 144, 192] {
                assert_eq!(
                    scale_logical_at_dpi(fresh, dpi),
                    scale_logical_at_dpi(stale, dpi)
                );
                for side in [CreditPosition::Left, CreditPosition::Right] {
                    for grip in [false, true] {
                        assert_eq!(
                            widget_content_positions_for(
                                2,
                                LanguageId::English,
                                grip,
                                true,
                                side,
                                CreditValueMode::Credits,
                                "360",
                                fresh
                            ),
                            widget_content_positions_for(
                                2,
                                LanguageId::English,
                                grip,
                                true,
                                side,
                                CreditValueMode::Credits,
                                "360",
                                stale
                            )
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn claude_auth_failure_retains_only_credential_scoped_data() {
        let mut data = AppUsageData::default();
        let mut usage = crate::models::UsageData::default();
        usage.session.percentage = 12.0;
        data.claude_code = Some(usage);
        data.codex = Some(crate::models::UsageData::default());
        // Same-credential auth failure: keep Claude's snapshot (shown stale).
        merge_provider_result(&mut data, CLAUDE, None, false);
        assert_eq!(data.claude_code.as_ref().unwrap().session.percentage, 12.0);
        assert!(data.codex.is_some());
        // Invalidating a changed scope removes the only source of cached data.
        merge_provider_result(&mut data, CLAUDE, None, true);
        assert!(data.claude_code.is_none());
        assert!(data.codex.is_some());
    }

    #[test]
    fn provider_failures_back_off_only_that_provider_and_block_its_reset_polling() {
        use poller::PollError;
        use provider_schedule::Outcome;
        assert_eq!(
            schedule_outcome(
                Some(PollError::RateLimited),
                Some(471_000),
                None,
                POLL_30_SEC
            ),
            Outcome::Transient {
                server_delay_ms: Some(471_000)
            }
        );
        for error in [
            PollError::NetworkUnavailable,
            PollError::ServerError,
            PollError::RequestFailed,
        ] {
            assert_eq!(
                schedule_outcome(Some(error), None, None, POLL_30_SEC),
                Outcome::Transient {
                    server_delay_ms: None
                }
            );
        }
        for error in [
            PollError::AuthRequired,
            PollError::TokenExpired,
            PollError::NoCredentials,
        ] {
            assert_eq!(
                schedule_outcome(Some(error), None, None, POLL_30_SEC),
                Outcome::Wait {
                    delay_ms: POLL_30_SEC
                }
            );
        }
        assert_eq!(
            schedule_outcome(
                Some(PollError::AuthRequired),
                None,
                Some(120_000),
                POLL_30_SEC
            ),
            Outcome::Wait { delay_ms: 120_000 }
        );
        assert_eq!(
            schedule_outcome(None, None, None, POLL_30_SEC),
            Outcome::Success
        );

        // A reset boundary refreshes only the provider past its reset, and never
        // one that is backing off.
        let enabled = [true, true, false];
        let request = poll_request(
            PollTrigger::ResetBoundary,
            enabled,
            [false, true, false],
            false,
        );
        assert_eq!(request.providers, [false, true, false]);
        assert!(!request.ignore_backoff && !request.ignore_spacing);
        let now = Instant::now();
        let mut schedule = provider_schedule::ProviderSchedule::default();
        schedule.begin(
            now,
            poll_request(PollTrigger::Scheduled, enabled, [false; 3], false),
        );
        schedule.finish(
            CODEX,
            now,
            Outcome::Transient {
                server_delay_ms: None,
            },
            POLL_30_SEC,
        );
        assert!(!schedule.begin(now + Duration::from_secs(5), request)[CODEX]);
    }

    #[test]
    fn manual_refresh_bypasses_cadence_and_spacing_but_not_server_deadlines() {
        let request = poll_request(PollTrigger::Manual, [true, true, false], [false; 3], false);
        assert_eq!(request.providers, [true, true, false]);
        assert!(request.ignore_due && request.ignore_backoff && request.ignore_spacing);
        let now = Instant::now();
        let mut schedule = provider_schedule::ProviderSchedule::default();
        schedule.begin(now, request);
        schedule.finish(
            CLAUDE,
            now,
            provider_schedule::Outcome::Transient {
                server_delay_ms: Some(300_000),
            },
            provider_schedule::CLAUDE_MIN_POLL_MS,
        );
        schedule.finish(CODEX, now, provider_schedule::Outcome::Success, POLL_30_SEC);
        let soon = now + Duration::from_secs(10);
        assert_eq!(schedule.begin(soon, request), [false, true, false]);
    }

    #[test]
    fn missing_data_shows_pending_until_reported_then_the_error_label() {
        use poller::PollError;
        assert_eq!(missing_usage_text("...", None, LanguageId::English), "...");
        assert_eq!(
            missing_usage_text("...", Some(PollError::RateLimited), LanguageId::English),
            "429"
        );
        assert_eq!(
            missing_usage_text(
                "!88% 1h04m",
                Some(PollError::AuthRequired),
                LanguageId::English
            ),
            "!"
        );
        assert_eq!(
            missing_usage_text("88% 1h04m", None, LanguageId::English),
            "!"
        );
    }

    #[test]
    fn poll_timer_rearm_releases_state_before_layout() {
        rearm_poll_timer(HWND::default());
        assert!(
            STATE.try_lock().is_ok(),
            "timer re-arm must release STATE before sizing"
        );
        let _ = total_widget_width();
    }

    #[test]
    fn manual_position_keeps_the_saved_anchor_and_reset_restores_collision_avoidance() {
        let taskbar = RECT {
            left: 0,
            top: 0,
            right: 1000,
            bottom: 48,
        };
        let occupied = [RECT {
            left: 760,
            top: 0,
            right: 900,
            bottom: 48,
        }];
        assert_eq!(position_anchor_left(taskbar, 900, &occupied, true), 900);
        assert_eq!(position_anchor_left(taskbar, 900, &occupied, false), 760);
        for manual in [false, true] {
            let settings = SettingsFile {
                manual_position: Some(manual),
                ..SettingsFile::default()
            };
            let restored: SettingsFile =
                serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
            assert_eq!(restored.manual_position, Some(manual));
        }
        let legacy: SettingsFile = serde_json::from_str(r#"{"tray_offset":321}"#).unwrap();
        assert!(legacy.manual_position.unwrap_or(legacy.tray_offset != 0));
    }

    fn decode_nul_terminated_wide(value: &[u16]) -> String {
        assert_eq!(value.last(), Some(&0));
        String::from_utf16(&value[..value.len() - 1]).unwrap()
    }

    #[test]
    fn log_open_command_uses_raw_shell_association_arguments() {
        let log_path = std::path::Path::new(
            r"C:\Users\Example User\AppData\Local\CodexUsage\logs\codex-usage.log",
        );
        let command = shell_open_log_file_command(log_path, true);

        assert!(command.file_exists);
        assert_eq!(command.target, log_path);
        assert_eq!(decode_nul_terminated_wide(&command.operation_wide), "open");
        assert_eq!(
            decode_nul_terminated_wide(&command.file_wide),
            log_path.to_string_lossy()
        );
        assert_eq!(command.parameters_wide, None);
        assert_eq!(
            command.working_directory,
            log_path.parent().unwrap().to_path_buf()
        );
        assert_eq!(
            decode_nul_terminated_wide(&command.working_directory_wide),
            log_path.parent().unwrap().to_string_lossy()
        );
        assert!(!decode_nul_terminated_wide(&command.file_wide).starts_with('"'));

        let fallback = shell_open_log_file_command(log_path, false);
        assert!(!fallback.file_exists);
        assert_eq!(fallback.target, log_path.parent().unwrap().to_path_buf());
        assert_eq!(
            decode_nul_terminated_wide(&fallback.file_wide),
            log_path.parent().unwrap().to_string_lossy()
        );
        assert_eq!(fallback.parameters_wide, None);
    }

    fn test_quota_section(remaining: f64, reset_offset: u64) -> crate::models::UsageSection {
        crate::models::UsageSection {
            percentage: 100.0 - remaining,
            resets_at: Some(UNIX_EPOCH + Duration::from_secs(2_000_000_000 + reset_offset)),
            available: true,
        }
    }

    fn append_test_codex_session_alert(
        alerts: &mut Vec<QuotaAlert>,
        notified: &mut BTreeSet<String>,
        threshold: u8,
        remaining: f64,
        reset_offset: u64,
    ) {
        let section = test_quota_section(remaining, reset_offset);
        append_quota_alert(
            alerts,
            notified,
            threshold,
            LanguageId::English,
            tray_icon::TrayIconKind::Codex,
            "codex",
            "Codex",
            "session",
            "5-hour",
            &section,
        );
    }

    fn append_test_codex_session_alerts(
        alerts: &mut Vec<QuotaAlert>,
        notified: &mut BTreeSet<String>,
        thresholds: &[u8],
        remaining: f64,
        reset_offset: u64,
    ) {
        let section = test_quota_section(remaining, reset_offset);
        append_quota_alerts(
            alerts,
            notified,
            thresholds,
            LanguageId::English,
            tray_icon::TrayIconKind::Codex,
            "codex",
            "Codex",
            "session",
            "5-hour",
            &section,
        );
    }

    fn append_test_codex_weekly_alert(
        alerts: &mut Vec<QuotaAlert>,
        notified: &mut BTreeSet<String>,
        threshold: u8,
        remaining: f64,
        reset_offset: u64,
    ) {
        let section = test_quota_section(remaining, reset_offset);
        append_quota_alert(
            alerts,
            notified,
            threshold,
            LanguageId::English,
            tray_icon::TrayIconKind::Codex,
            "codex",
            "Codex",
            "weekly",
            "7d",
            &section,
        );
    }

    fn append_test_codex_weekly_alerts(
        alerts: &mut Vec<QuotaAlert>,
        notified: &mut BTreeSet<String>,
        thresholds: &[u8],
        remaining: f64,
        reset_offset: u64,
    ) {
        let section = test_quota_section(remaining, reset_offset);
        append_quota_alerts(
            alerts,
            notified,
            thresholds,
            LanguageId::English,
            tray_icon::TrayIconKind::Codex,
            "codex",
            "Codex",
            "weekly",
            "7d",
            &section,
        );
    }

    fn test_codex_data(
        credits: Option<CreditBalance>,
        session_remaining: f64,
        weekly_remaining: f64,
    ) -> AppUsageData {
        AppUsageData {
            codex: Some(crate::models::UsageData {
                session: crate::models::UsageSection {
                    percentage: 100.0 - session_remaining,
                    resets_at: None,
                    available: true,
                },
                weekly: crate::models::UsageSection {
                    percentage: 100.0 - weekly_remaining,
                    resets_at: None,
                    available: true,
                },
                credits,
            }),
            ..AppUsageData::default()
        }
    }

    #[test]
    fn service_tooltip_combines_visible_quota_rows() {
        assert_eq!(
            service_tooltip(
                "Codex",
                "剩余13% 19:04重置",
                "剩余86% 07/18重置",
                true,
                true
            ),
            "Codex: 5h 剩余13% 19:04重置 | 7d 剩余86% 07/18重置"
        );
        assert_eq!(
            service_tooltip("Claude Code", "13%", "86%", false, true),
            "Claude Code: 7d 86%"
        );
    }

    #[test]
    fn unavailable_claude_cli_has_an_explicit_menu_label() {
        assert_eq!(
            claude_code_menu_label(
                LanguageId::SimplifiedChinese.strings(),
                LanguageId::SimplifiedChinese,
                false,
            ),
            "Claude Code（需登录 CLI）"
        );
        assert_eq!(
            claude_code_menu_label(LanguageId::English.strings(), LanguageId::English, true),
            "Claude Code"
        );
    }

    fn test_settings_json(language: &str) -> String {
        format!(
            r#"{{
  "tray_offset": 321,
  "taskbar_index": 1,
  "poll_interval_ms": 60000,
  "language": "{language}",
  "widget_visible": true,
  "show_claude_code": false,
  "show_codex": true,
  "show_antigravity": false
}}"#
        )
    }

    #[test]
    fn loads_legacy_settings_when_new_path_is_missing() {
        let base = std::env::temp_dir().join(format!(
            "codex-usage-settings-test-{}-{}",
            std::process::id(),
            now_unix_secs()
        ));
        let current = base.join("CodexUsage").join("settings.json");
        let legacy = base.join("ClaudeCodeUsageMonitor").join("settings.json");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, test_settings_json("zh-CN")).unwrap();

        let (settings, source) = load_settings_from_paths(&current, &legacy);

        assert_eq!(source, SettingsSource::Legacy);
        assert_eq!(settings.tray_offset, 321);
        assert_eq!(settings.poll_interval_ms, 60_000);
        assert_eq!(settings.language.as_deref(), Some("zh-CN"));
        assert!(settings.show_codex);
        assert!(!settings.show_claude_code);
        assert!(settings.show_session_window);
        assert!(settings.show_weekly_window);
        assert_eq!(settings.usage_display, "remaining");
        assert_eq!(settings.bar_color, None);
        assert_eq!(settings.alert_threshold_percent, 0);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn new_settings_take_precedence_over_legacy_settings() {
        let base = std::env::temp_dir().join(format!(
            "codex-usage-settings-precedence-test-{}-{}",
            std::process::id(),
            now_unix_secs()
        ));
        let current = base.join("CodexUsage").join("settings.json");
        let legacy = base.join("ClaudeCodeUsageMonitor").join("settings.json");
        std::fs::create_dir_all(current.parent().unwrap()).unwrap();
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&current, test_settings_json("en")).unwrap();
        std::fs::write(&legacy, test_settings_json("zh-CN")).unwrap();

        let (settings, source) = load_settings_from_paths(&current, &legacy);

        assert_eq!(source, SettingsSource::Current);
        assert_eq!(settings.language.as_deref(), Some("en"));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn startup_migration_only_writes_when_legacy_exists_without_current_entry() {
        assert!(should_write_migrated_startup(true, false));
        assert!(!should_write_migrated_startup(false, false));
        assert!(!should_write_migrated_startup(true, true));
        assert!(!should_write_migrated_startup(false, true));
    }

    #[test]
    fn displays_distinct_transient_error_categories() {
        assert_eq!(
            poll_error_display_label(
                poller::PollError::NetworkUnavailable,
                LanguageId::SimplifiedChinese,
            ),
            "网络"
        );
        assert_eq!(
            poll_error_display_label(
                poller::PollError::RateLimited,
                LanguageId::SimplifiedChinese,
            ),
            "限流"
        );
        assert_eq!(
            poll_error_display_label(poller::PollError::ServerError, LanguageId::English),
            "5XX"
        );
        assert_eq!(
            poll_error_display_label(poller::PollError::RequestFailed, LanguageId::English),
            "ERR"
        );
    }

    #[test]
    fn normalizes_usage_display_and_alert_settings() {
        let settings = normalize_settings(SettingsFile {
            show_session_window: false,
            show_weekly_window: false,
            usage_display: "USED".into(),
            bar_color: Some("123456".into()),
            alert_threshold_percent: 17,
            notified_quota_windows: vec!["codex:weekly:1".into(), "codex:weekly:1".into()],
            ..SettingsFile::default()
        });

        assert!(settings.show_session_window);
        assert!(!settings.show_weekly_window);
        assert_eq!(settings.usage_display, "used");
        assert_eq!(settings.bar_color.as_deref(), Some("#123456"));
        assert_eq!(settings.alert_threshold_percent, 0);
        assert_eq!(settings.notified_quota_windows.len(), 1);
    }

    #[test]
    fn accepts_new_persisted_settings_values() {
        let settings: SettingsFile = serde_json::from_str(
            r##"{
                "poll_interval_ms": 30000,
                "usage_display": "used",
                "bar_color": "#abcdef",
                "alert_threshold_percent": 5
            }"##,
        )
        .unwrap();
        let settings = normalize_settings(settings);

        assert_eq!(settings.poll_interval_ms, POLL_30_SEC);
        assert_eq!(settings.usage_display, "used");
        assert_eq!(settings.bar_color.as_deref(), Some("#ABCDEF"));
        assert_eq!(settings.alert_threshold_percent, 5);
        assert_eq!(settings.alert_thresholds_percent, Some(vec![5]));
        let serialized = serde_json::to_string(&settings).unwrap();
        assert!(serialized.contains("\"usage_display\":\"used\""));
        assert!(serialized.contains("\"bar_color\":\"#ABCDEF\""));
    }

    #[test]
    fn legacy_threshold_and_notification_state_migrate_without_loss() {
        let settings = normalize_settings(SettingsFile {
            alert_threshold_percent: 10,
            notified_quota_windows: vec!["codex:weekly:2000000000".to_string()],
            ..SettingsFile::default()
        });

        assert_eq!(settings.alert_thresholds_percent, Some(vec![10]));
        assert_eq!(
            settings.notified_quota_windows,
            vec!["codex:weekly:threshold:10:2000000000"]
        );
    }

    #[test]
    fn usage_display_percentage_complements_used_percentage() {
        assert_eq!(usage_percent_for_display(true, 18.0), 82.0);
        assert_eq!(usage_percent_for_display(false, 18.0), 18.0);
    }

    #[test]
    fn mixed_usage_display_maps_each_provider_direction() {
        let mixed = UsageDisplayMode::Mixed.directions();
        assert!(
            mixed.codex,
            "Mixed shows Codex remaining, falling toward 0%"
        );
        assert!(!mixed.claude, "Mixed shows Claude used, rising toward 100%");
        assert!(mixed.antigravity, "Antigravity keeps the Remaining default");
        assert_eq!(
            UsageDisplayMode::Remaining.directions(),
            DisplayDirections::uniform(true)
        );
        assert_eq!(
            UsageDisplayMode::Used.directions(),
            DisplayDirections::uniform(false)
        );

        // Bars and labels use the same direction, including 0% and 100%.
        let strings = LanguageId::English.strings();
        for (used, codex_label, claude_label) in [
            (0.0, "100%", "0%"),
            (100.0, "0%", "100%"),
            (98.0, "2%", "98%"),
            (37.0, "63%", "37%"),
        ] {
            let section = crate::models::UsageSection {
                percentage: used,
                resets_at: None,
                available: true,
            };
            let codex = poller::format_codex_line(
                &section,
                strings,
                false,
                mixed.codex,
                poller::UsageWindowKind::Session,
            );
            let claude = poller::format_line(
                &section,
                strings,
                false,
                mixed.claude,
                poller::UsageWindowKind::Session,
            );
            assert_eq!(codex, codex_label);
            assert_eq!(claude, claude_label);
            assert_eq!(
                usage_percent_for_display(mixed.codex, used),
                codex_label.trim_end_matches('%').parse::<f64>().unwrap()
            );
            assert_eq!(
                usage_percent_for_display(mixed.claude, used),
                claude_label.trim_end_matches('%').parse::<f64>().unwrap()
            );
        }

        // Simplified Chinese keeps its own labels for each direction.
        let section = crate::models::UsageSection {
            percentage: 30.0,
            resets_at: None,
            available: true,
        };
        let chinese = LanguageId::SimplifiedChinese.strings();
        let codex = poller::format_line(
            &section,
            chinese,
            true,
            mixed.codex,
            poller::UsageWindowKind::Weekly,
        );
        let claude = poller::format_line(
            &section,
            chinese,
            true,
            mixed.claude,
            poller::UsageWindowKind::Weekly,
        );
        assert!(codex.contains("剩余") && codex.contains("70%"));
        assert!(claude.contains("已用") && claude.contains("30%"));

        // Stale markers keep their prefix whatever the direction.
        let claude_stale = crate::provider_health::stale_text(
            &poller::format_line(
                &crate::models::UsageSection {
                    percentage: 88.0,
                    resets_at: None,
                    available: true,
                },
                strings,
                false,
                mixed.claude,
                poller::UsageWindowKind::Session,
            ),
            true,
        );
        assert_eq!(claude_stale, "!88%");
    }

    #[test]
    fn mixed_display_keeps_stale_prefix_and_stable_widths() {
        // Mixed introduces no new text shapes: each provider's label is one
        // of the shapes Remaining or Used already produce.
        for text in ["98% 1h04m", "2% 1d23h", "100% 5h", "0% 12m"] {
            let fresh = quota_text_width_for(LanguageId::English, &[text]);
            let stale = quota_text_width_for(LanguageId::English, &[&format!("!{text}")]);
            assert_eq!(fresh, stale);
        }
        assert_eq!(
            crate::provider_health::stale_text("98% 1h04m", true),
            "!98% 1h04m"
        );
        assert_eq!(
            crate::provider_health::stale_text("2% 1d23h", false),
            "2% 1d23h"
        );
    }

    #[test]
    fn alerts_use_normalized_remaining_in_every_display_mode() {
        // Alerts take normalized usage data; the display mode is not an
        // input. Codex at 98% used (Mixed shows "2%") alerts at 2% remaining;
        // Claude at 98% used (Mixed shows "98%") is still 2% remaining.
        for mode in [
            UsageDisplayMode::Mixed,
            UsageDisplayMode::Remaining,
            UsageDisplayMode::Used,
        ] {
            let mut alerts = Vec::new();
            let mut notified = BTreeSet::new();
            append_test_codex_session_alerts(&mut alerts, &mut notified, &[2, 5, 30], 2.0, 0);
            assert_eq!(alerts.len(), 1, "{mode:?}");
            assert!(alerts[0].message.contains("2%"), "{mode:?}");
            assert!(notified.iter().any(|key| key.contains(":threshold:2:")));
            let directions = mode.directions();
            let (codex, claude) = match mode {
                UsageDisplayMode::Mixed => (2.0, 98.0),
                UsageDisplayMode::Remaining => (2.0, 2.0),
                UsageDisplayMode::Used => (98.0, 98.0),
            };
            assert_eq!(usage_percent_for_display(directions.codex, 98.0), codex);
            assert_eq!(usage_percent_for_display(directions.claude, 98.0), claude);
            assert_eq!(
                usage_percent_for_display(directions.antigravity, 98.0),
                if mode == UsageDisplayMode::Used {
                    98.0
                } else {
                    2.0
                }
            );
        }
    }

    #[test]
    fn usage_display_settings_keep_existing_choices_and_default_new_installs_to_mixed() {
        // Brand-new installation (no settings file) starts Mixed.
        let dir =
            std::env::temp_dir().join(format!("codex-usage-mixed-default-{}", std::process::id()));
        let (fresh, source) =
            load_settings_from_paths(&dir.join("missing.json"), &dir.join("missing-legacy.json"));
        assert!(source == SettingsSource::Defaults);
        assert_eq!(normalize_settings(fresh).usage_display, "mixed");

        // Files written before the field existed keep Remaining.
        let legacy: SettingsFile = serde_json::from_str(r#"{"tray_offset":0}"#).unwrap();
        assert_eq!(normalize_settings(legacy).usage_display, "remaining");

        // Settings written by Reserve-era builds, including any stray Reserve
        // keys, still load with their Credits and display choices intact.
        let reserve_era: SettingsFile = serde_json::from_str(
            r#"{"usage_display":"mixed","credit_display":"when_needed","credit_position":"right",
                "credit_value_mode":"usd_estimate","show_luna_reserve":true,
                "luna_reserve":{"enabled":true},"reserve_display":"ring"}"#,
        )
        .unwrap();
        let reserve_era = normalize_settings(reserve_era);
        assert_eq!(reserve_era.usage_display, "mixed");
        assert_eq!(reserve_era.credit_display, "when_needed");
        assert_eq!(reserve_era.credit_position, "right");
        assert_eq!(reserve_era.credit_value_mode, "usd_estimate");
        let saved = serde_json::to_string(&reserve_era).unwrap();
        assert!(!saved.contains("reserve"));

        for (stored, expected) in [
            ("remaining", "remaining"),
            ("used", "used"),
            ("USED", "used"),
            ("mixed", "mixed"),
            ("Mixed", "mixed"),
            ("sideways", "remaining"),
        ] {
            let settings: SettingsFile =
                serde_json::from_str(&format!(r#"{{"usage_display":"{stored}"}}"#)).unwrap();
            let settings = normalize_settings(settings);
            assert_eq!(settings.usage_display, expected, "{stored}");
            let round_trip: SettingsFile =
                serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
            assert_eq!(
                UsageDisplayMode::from_setting(&round_trip.usage_display).as_setting(),
                expected
            );
        }
    }

    #[test]
    fn adaptive_polling_uses_remaining_quota_and_urgent_window() {
        assert_eq!(adaptive_poll_interval(None), POLL_5_MIN);
        assert_eq!(
            adaptive_poll_interval(Some(&test_codex_data(None, 82.0, 41.0))),
            POLL_5_MIN
        );
        assert_eq!(
            adaptive_poll_interval(Some(&test_codex_data(None, 30.0, 90.0))),
            POLL_1_MIN
        );
        assert_eq!(
            adaptive_poll_interval(Some(&test_codex_data(None, 10.0, 90.0))),
            POLL_30_SEC
        );

        let mut urgent_other_provider = test_codex_data(None, 50.0, 50.0);
        urgent_other_provider.claude_code = Some(crate::models::UsageData {
            session: crate::models::UsageSection {
                percentage: 96.0,
                available: true,
                ..Default::default()
            },
            ..Default::default()
        });
        assert_eq!(
            adaptive_poll_interval(Some(&urgent_other_provider)),
            POLL_30_SEC
        );

        let mut unavailable_is_ignored = test_codex_data(None, 50.0, 50.0);
        unavailable_is_ignored.claude_code = Some(crate::models::UsageData {
            session: crate::models::UsageSection {
                percentage: 99.0,
                available: false,
                ..Default::default()
            },
            ..Default::default()
        });
        assert_eq!(
            adaptive_poll_interval(Some(&unavailable_is_ignored)),
            POLL_5_MIN
        );
    }

    #[test]
    fn adaptive_polling_setting_is_backward_compatible_and_persistent() {
        let legacy = normalize_settings(SettingsFile::default());
        assert!(!legacy.adaptive_polling);

        let adaptive: SettingsFile =
            serde_json::from_str(r#"{"poll_interval_ms":3600000,"adaptive_polling":true}"#)
                .unwrap();
        let adaptive = normalize_settings(adaptive);
        assert!(adaptive.adaptive_polling);
        assert_eq!(adaptive.poll_interval_ms, POLL_1_HOUR);
    }

    #[test]
    fn codex_mcp_preference_defaults_off_and_persists_both_states() {
        let old: SettingsFile = serde_json::from_str(r#"{"show_codex":true}"#).unwrap();
        assert!(!normalize_settings(old).enable_codex_mcp);
        assert!(!SettingsFile::default().enable_codex_mcp);

        for expected in [false, true] {
            let settings = SettingsFile {
                enable_codex_mcp: expected,
                ..SettingsFile::default()
            };
            let serialized = serde_json::to_string(&settings).unwrap();
            let restored: SettingsFile = serde_json::from_str(&serialized).unwrap();
            assert_eq!(normalize_settings(restored).enable_codex_mcp, expected);
        }
    }

    #[test]
    fn mcp_startup_uses_the_loaded_current_preference_and_defaults_safely() {
        let base = std::env::temp_dir().join(format!(
            "codex-usage-mcp-settings-test-{}-{}",
            std::process::id(),
            now_unix_secs()
        ));
        let current = base.join("CodexUsage").join("settings.json");
        let legacy = base.join("ClaudeCodeUsageMonitor").join("settings.json");

        let (settings, source) = load_settings_from_paths(&current, &legacy);
        assert_eq!(source, SettingsSource::Defaults);
        let settings = normalize_settings(settings);
        assert!(!settings.enable_codex_mcp);
        let mut started = false;
        assert_eq!(
            start_mcp_if_enabled(settings.enable_codex_mcp, || {
                started = true;
                Ok(())
            })
            .unwrap(),
            false
        );
        assert!(!started);

        std::fs::create_dir_all(current.parent().unwrap()).unwrap();
        let enabled_json = serde_json::to_string(&SettingsFile {
            enable_codex_mcp: true,
            ..SettingsFile::default()
        })
        .unwrap();
        std::fs::write(&current, enabled_json).unwrap();
        let (settings, source) = load_settings_from_paths(&current, &legacy);
        assert_eq!(source, SettingsSource::Current);
        let settings = normalize_settings(settings);
        assert!(settings.enable_codex_mcp);
        assert!(start_mcp_if_enabled(settings.enable_codex_mcp, || {
            started = true;
            Ok(())
        })
        .unwrap());
        assert!(started);

        let disabled_json = serde_json::to_string(&SettingsFile {
            enable_codex_mcp: false,
            ..SettingsFile::default()
        })
        .unwrap();
        std::fs::write(&current, disabled_json).unwrap();
        let (settings, source) = load_settings_from_paths(&current, &legacy);
        assert_eq!(source, SettingsSource::Current);
        assert!(!normalize_settings(settings).enable_codex_mcp);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn last_resort_cooldown_setting_survives_settings_round_trip() {
        let settings = SettingsFile {
            last_codex_exec_refresh_unix: Some(1_800_000_000),
            ..SettingsFile::default()
        };
        let serialized = serde_json::to_string(&settings).unwrap();
        let restored: SettingsFile = serde_json::from_str(&serialized).unwrap();
        assert_eq!(restored.last_codex_exec_refresh_unix, Some(1_800_000_000));
        let old: SettingsFile = serde_json::from_str("{}").unwrap();
        assert_eq!(old.last_codex_exec_refresh_unix, None);
    }

    #[test]
    fn only_codex_auth_errors_enter_auth_recovery() {
        assert!(is_codex_auth_error(Some(poller::PollError::AuthRequired)));
        assert!(is_codex_auth_error(Some(poller::PollError::TokenExpired)));
        for error in [
            poller::PollError::NetworkUnavailable,
            poller::PollError::RateLimited,
            poller::PollError::ServerError,
            poller::PollError::RequestFailed,
        ] {
            assert!(!is_codex_auth_error(Some(error)));
        }
        assert!(!is_codex_auth_error(None));
    }

    #[test]
    fn codex_credential_changes_trigger_immediate_recovery_detection() {
        let before = vec!["codex-auth|present|10|123".to_string()];
        let unchanged = before.clone();
        let changed = vec!["codex-auth|present|11|456".to_string()];
        let newly_missing = vec!["codex-auth|missing".to_string()];

        assert!(!credential_snapshot_changed(&before, &unchanged));
        assert!(credential_snapshot_changed(&before, &changed));
        assert!(credential_snapshot_changed(&before, &newly_missing));
    }

    #[test]
    fn codex_auth_retries_are_passive_before_refresh_and_exec_is_gated() {
        assert_eq!(codex_auth_retry_delay_ms(1), 30_000);
        assert_eq!(codex_auth_retry_delay_ms(2), 60_000);
        assert_eq!(codex_auth_retry_delay_ms(3), 120_000);
        assert_eq!(codex_auth_retry_delay_ms(4), 300_000);
        assert!(!should_attempt_codex_model_free_refresh(2, false));
        assert!(should_attempt_codex_model_free_refresh(3, false));
        assert!(!should_attempt_codex_model_free_refresh(6, true));

        assert!(!should_attempt_codex_exec_fallback(
            1, false, false, false, true
        ));
        assert!(!should_attempt_codex_exec_fallback(
            6, true, false, false, false
        ));
        assert!(!should_attempt_codex_exec_fallback(
            6, true, false, false, true
        ));
        assert!(!should_attempt_codex_exec_fallback(
            7, true, false, true, true
        ));
        assert!(!should_attempt_codex_exec_fallback(
            7, true, true, false, true
        ));
    }

    #[test]
    fn codex_exec_cooldown_is_persistent_and_saturating() {
        assert!(codex_exec_cooldown_elapsed(None, 10));
        assert!(!codex_exec_cooldown_elapsed(
            Some(100),
            100 + CODEX_EXEC_REFRESH_COOLDOWN_SECS - 1
        ));
        assert!(codex_exec_cooldown_elapsed(
            Some(100),
            100 + CODEX_EXEC_REFRESH_COOLDOWN_SECS
        ));
        assert!(!codex_exec_cooldown_elapsed(Some(200), 100));
    }

    #[test]
    fn successful_codex_auth_recovery_resets_episode_state() {
        let mut episode = CodexAuthEpisode {
            active: true,
            passive_failures: 6,
            model_free_refresh_attempted: true,
            model_free_refresh_succeeded: false,
            exec_attempted: true,
            credential_snapshot: vec!["changed".to_string()],
        };
        assert!(finish_codex_auth_episode(&mut episode));
        assert_eq!(episode, CodexAuthEpisode::default());
        assert!(!finish_codex_auth_episode(&mut episode));
    }

    #[test]
    fn rejected_usage_poll_cannot_enable_model_consuming_recovery() {
        let mut episode = CodexAuthEpisode {
            active: true,
            passive_failures: 6,
            model_free_refresh_attempted: true,
            model_free_refresh_succeeded: true,
            ..CodexAuthEpisode::default()
        };

        mark_model_free_refresh_ineffective_after_auth_rejection(&mut episode);

        assert!(!should_attempt_codex_exec_fallback(
            episode.passive_failures,
            episode.model_free_refresh_attempted,
            episode.model_free_refresh_succeeded,
            episode.exec_attempted,
            true,
        ));
    }

    #[test]
    fn frequency_change_refreshes_idle_providers_and_preserves_each_recovery() {
        use provider_schedule::{Outcome, ProviderSchedule};
        let enabled = [true, true, false];
        let now = Instant::now();
        let mut schedule = ProviderSchedule::default();
        schedule.begin(
            now,
            poll_request(PollTrigger::Scheduled, enabled, [false; 3], false),
        );
        schedule.finish(
            CLAUDE,
            now,
            Outcome::Transient {
                server_delay_ms: None,
            },
            provider_schedule::CLAUDE_MIN_POLL_MS,
        );
        schedule.finish(CODEX, now, Outcome::Success, POLL_5_MIN);

        // Claude keeps its backoff; idle Codex refreshes immediately.
        let soon = now + Duration::from_secs(5);
        let request = poll_request(PollTrigger::Reconfigure, enabled, [false; 3], false);
        assert_eq!(schedule.begin(soon, request), [false, true, false]);

        // Codex auth recovery keeps its own retry ladder across a frequency change.
        let request = poll_request(PollTrigger::Reconfigure, enabled, [false; 3], true);
        assert!(!request.providers[CODEX]);
        assert!(poll_request(PollTrigger::Scheduled, enabled, [false; 3], true).providers[CODEX]);
        assert!(
            poll_request(
                PollTrigger::CodexCredentialChange,
                enabled,
                [false; 3],
                true
            )
            .providers[CODEX]
        );
    }

    #[test]
    fn low_quota_alerts_use_remaining_percentage() {
        let mut alerts = Vec::new();
        let mut notified = BTreeSet::new();
        let section = crate::models::UsageSection {
            percentage: 95.0,
            resets_at: None,
            available: true,
        };

        append_quota_alert(
            &mut alerts,
            &mut notified,
            5,
            LanguageId::English,
            tray_icon::TrayIconKind::Codex,
            "codex",
            "Codex",
            "session",
            "5-hour quota",
            &section,
        );

        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].message.contains("5% remaining"));
    }

    #[test]
    fn unavailable_claude_cli_is_disabled_without_disabling_codex() {
        let settings = SettingsFile {
            show_claude_code: true,
            show_codex: false,
            show_antigravity: false,
            ..SettingsFile::default()
        };

        let (settings, changed) = apply_claude_code_availability(settings, false);

        assert!(changed);
        assert!(!settings.show_claude_code);
        assert!(settings.show_codex);
    }

    #[test]
    fn formats_precise_local_reset_time() {
        let local = SYSTEMTIME {
            wYear: 2026,
            wMonth: 7,
            wDay: 17,
            wHour: 18,
            wMinute: 30,
            ..Default::default()
        };
        assert_eq!(format_local_system_time(local), "2026-07-17 18:30");
        assert_eq!(format_precise_reset_time(None), None);
    }

    #[test]
    fn low_quota_alert_is_deduplicated_until_reset_window_changes() {
        let mut alerts = Vec::new();
        let mut notified = BTreeSet::new();
        let first_reset = UNIX_EPOCH + Duration::from_secs(2_000_000_000);
        let first = crate::models::UsageSection {
            percentage: 85.0,
            resets_at: Some(first_reset),
            available: true,
        };

        append_quota_alert(
            &mut alerts,
            &mut notified,
            20,
            LanguageId::SimplifiedChinese,
            tray_icon::TrayIconKind::Codex,
            "codex",
            "Codex",
            "session",
            "5小时",
            &first,
        );
        append_quota_alert(
            &mut alerts,
            &mut notified,
            20,
            LanguageId::SimplifiedChinese,
            tray_icon::TrayIconKind::Codex,
            "codex",
            "Codex",
            "session",
            "5小时",
            &first,
        );
        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].message.contains("仅剩 15%"));

        let next = crate::models::UsageSection {
            percentage: 90.0,
            resets_at: Some(first_reset + Duration::from_secs(18_000)),
            available: true,
        };
        append_quota_alert(
            &mut alerts,
            &mut notified,
            20,
            LanguageId::SimplifiedChinese,
            tray_icon::TrayIconKind::Codex,
            "codex",
            "Codex",
            "session",
            "5小时",
            &next,
        );
        assert_eq!(alerts.len(), 2);
        assert_eq!(notified.len(), 1);
    }

    #[test]
    fn low_quota_alert_ignores_accumulating_reset_time_jitter() {
        let mut alerts = Vec::new();
        let mut notified = BTreeSet::new();
        let first_reset = UNIX_EPOCH + Duration::from_secs(2_000_000_000);

        for offset in [0, 4 * 60, 8 * 60, 12 * 60] {
            let section = crate::models::UsageSection {
                percentage: 96.0,
                resets_at: Some(first_reset + Duration::from_secs(offset)),
                available: true,
            };
            append_quota_alert(
                &mut alerts,
                &mut notified,
                5,
                LanguageId::English,
                tray_icon::TrayIconKind::Codex,
                "codex",
                "Codex",
                "session",
                "5-hour quota",
                &section,
            );
        }

        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].message.contains("4% remaining"));
        assert_eq!(notified.len(), 1);
    }

    #[test]
    fn low_quota_alert_fires_then_exhaustion_alert_fires() {
        let mut alerts = Vec::new();
        let mut notified = BTreeSet::new();

        append_test_codex_session_alert(&mut alerts, &mut notified, 5, 4.0, 0);
        append_test_codex_session_alert(&mut alerts, &mut notified, 5, 0.0, 0);

        assert_eq!(alerts.len(), 2);
        assert_eq!(alerts[0].title, "Codex quota alert");
        assert_eq!(alerts[1].title, "Codex quota exhausted");
        assert!(alerts[1].message.contains("5-hour quota has 0% remaining"));
        assert_eq!(notified.len(), 2);
    }

    #[test]
    fn quota_alerts_off_suppress_threshold_and_exhaustion_alerts() {
        let mut alerts = Vec::new();
        let mut notified = BTreeSet::new();

        append_test_codex_session_alert(&mut alerts, &mut notified, 0, 4.0, 0);
        append_test_codex_session_alert(&mut alerts, &mut notified, 0, 0.0, 0);

        assert!(alerts.is_empty());
        assert!(notified.is_empty());
    }

    #[test]
    fn direct_jump_to_zero_only_sends_exhaustion_alert() {
        let mut alerts = Vec::new();
        let mut notified = BTreeSet::new();

        append_test_codex_session_alert(&mut alerts, &mut notified, 5, 50.0, 0);
        append_test_codex_session_alert(&mut alerts, &mut notified, 5, 0.0, 0);

        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].title, "Codex quota exhausted");
        assert_eq!(notified.len(), 2);
    }

    #[test]
    fn repeated_zero_remaining_polls_do_not_repeat_exhaustion_alert() {
        let mut alerts = Vec::new();
        let mut notified = BTreeSet::new();

        append_test_codex_session_alert(&mut alerts, &mut notified, 5, 0.0, 0);
        append_test_codex_session_alert(&mut alerts, &mut notified, 5, 0.0, 0);

        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].title, "Codex quota exhausted");
    }

    #[test]
    fn persisted_exhaustion_state_does_not_repeat_alert_after_restart() {
        let mut alerts = Vec::new();
        let mut notified = BTreeSet::new();
        append_test_codex_session_alert(&mut alerts, &mut notified, 5, 0.0, 0);

        let persisted: BTreeSet<String> =
            serde_json::from_str(&serde_json::to_string(&notified).unwrap()).unwrap();
        let mut restarted_alerts = Vec::new();
        let mut restarted_notified = persisted;
        append_test_codex_session_alert(&mut restarted_alerts, &mut restarted_notified, 5, 0.0, 0);

        assert!(restarted_alerts.is_empty());
        assert_eq!(restarted_notified, notified);
    }

    #[test]
    fn exhaustion_alert_ignores_reset_time_jitter_and_rebases_state() {
        let mut alerts = Vec::new();
        let mut notified = BTreeSet::new();

        for offset in [0, 4 * 60, 8 * 60, 12 * 60] {
            append_test_codex_session_alert(&mut alerts, &mut notified, 5, 0.0, offset);
        }

        assert_eq!(alerts.len(), 1);
        assert_eq!(notified.len(), 2);
        assert!(notified.contains("codex:session:threshold:5:2000000720"));
        assert!(notified.contains("codex:session:exhausted:2000000720"));
    }

    #[test]
    fn exhaustion_alert_rearms_for_a_genuine_new_quota_window() {
        let mut alerts = Vec::new();
        let mut notified = BTreeSet::new();

        append_test_codex_session_alert(&mut alerts, &mut notified, 5, 0.0, 0);
        append_test_codex_session_alert(&mut alerts, &mut notified, 5, 0.0, 18_000);

        assert_eq!(alerts.len(), 2);
        assert!(alerts
            .iter()
            .all(|alert| alert.title == "Codex quota exhausted"));
        assert_eq!(notified.len(), 2);
        assert!(notified.contains("codex:session:exhausted:2000018000"));
    }

    #[test]
    fn weekly_low_quota_alert_fires_at_five_percent_remaining() {
        let mut alerts = Vec::new();
        let mut notified = BTreeSet::new();

        append_test_codex_weekly_alert(&mut alerts, &mut notified, 5, 5.0, 0);

        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].title, "Codex quota alert");
        assert!(alerts[0].message.contains("7d quota has 5% remaining"));
    }

    #[test]
    fn weekly_threshold_then_exhaustion_alerts_are_independent() {
        let mut alerts = Vec::new();
        let mut notified = BTreeSet::new();

        append_test_codex_weekly_alert(&mut alerts, &mut notified, 5, 4.0, 0);
        append_test_codex_weekly_alert(&mut alerts, &mut notified, 5, 0.0, 0);

        assert_eq!(alerts.len(), 2);
        assert_eq!(alerts[1].title, "Codex quota exhausted");
        assert_eq!(notified.len(), 2);
    }

    #[test]
    fn repeated_weekly_low_polls_do_not_duplicate_threshold_alert() {
        let mut alerts = Vec::new();
        let mut notified = BTreeSet::new();

        append_test_codex_weekly_alert(&mut alerts, &mut notified, 5, 4.0, 0);
        append_test_codex_weekly_alert(&mut alerts, &mut notified, 5, 3.0, 0);

        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].title, "Codex quota alert");
    }

    #[test]
    fn repeated_weekly_zero_polls_do_not_duplicate_exhaustion_alert() {
        let mut alerts = Vec::new();
        let mut notified = BTreeSet::new();

        append_test_codex_weekly_alert(&mut alerts, &mut notified, 5, 0.0, 0);
        append_test_codex_weekly_alert(&mut alerts, &mut notified, 5, 0.0, 0);

        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].title, "Codex quota exhausted");
    }

    #[test]
    fn persisted_weekly_alert_state_survives_restart() {
        let mut alerts = Vec::new();
        let mut notified = BTreeSet::new();
        append_test_codex_weekly_alert(&mut alerts, &mut notified, 5, 4.0, 0);
        append_test_codex_weekly_alert(&mut alerts, &mut notified, 5, 0.0, 0);

        let persisted: BTreeSet<String> =
            serde_json::from_str(&serde_json::to_string(&notified).unwrap()).unwrap();
        let mut restarted_alerts = Vec::new();
        let mut restarted_notified = persisted;
        append_test_codex_weekly_alert(&mut restarted_alerts, &mut restarted_notified, 5, 0.0, 0);

        assert!(restarted_alerts.is_empty());
        assert_eq!(restarted_notified, notified);
    }

    #[test]
    fn weekly_reset_time_jitter_does_not_rearm_alerts() {
        let mut alerts = Vec::new();
        let mut notified = BTreeSet::new();

        for offset in [0, 4 * 60, 8 * 60, 12 * 60] {
            append_test_codex_weekly_alert(&mut alerts, &mut notified, 5, 0.0, offset);
        }

        assert_eq!(alerts.len(), 1);
        assert!(notified.contains("codex:weekly:exhausted:2000000720"));
    }

    #[test]
    fn genuine_new_weekly_window_rearms_alerts() {
        let mut alerts = Vec::new();
        let mut notified = BTreeSet::new();

        append_test_codex_weekly_alert(&mut alerts, &mut notified, 5, 0.0, 0);
        append_test_codex_weekly_alert(&mut alerts, &mut notified, 5, 0.0, 18_000);

        assert_eq!(alerts.len(), 2);
        assert!(notified.contains("codex:weekly:exhausted:2000018000"));
    }

    #[test]
    fn multiple_thresholds_fire_once_each_when_crossed_gradually() {
        let thresholds = [2, 10, 30];
        let mut alerts = Vec::new();
        let mut notified = BTreeSet::new();

        append_test_codex_session_alerts(&mut alerts, &mut notified, &thresholds, 31.0, 0);
        append_test_codex_session_alerts(&mut alerts, &mut notified, &thresholds, 25.0, 0);
        append_test_codex_session_alerts(&mut alerts, &mut notified, &thresholds, 8.0, 0);
        append_test_codex_session_alerts(&mut alerts, &mut notified, &thresholds, 1.0, 0);
        append_test_codex_session_alerts(&mut alerts, &mut notified, &thresholds, 1.0, 0);

        assert_eq!(alerts.len(), 3);
        assert_eq!(notified.len(), 3);
        assert!(alerts
            .iter()
            .all(|alert| alert.title == "Codex quota alert"));
    }

    #[test]
    fn a_jump_across_thresholds_emits_only_the_deepest_threshold() {
        let thresholds = [10, 20, 30];
        let mut alerts = Vec::new();
        let mut notified = BTreeSet::new();

        append_test_codex_session_alerts(&mut alerts, &mut notified, &thresholds, 80.0, 0);
        append_test_codex_session_alerts(&mut alerts, &mut notified, &thresholds, 8.0, 0);
        append_test_codex_session_alerts(&mut alerts, &mut notified, &thresholds, 5.0, 0);

        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].message.contains("8% remaining"));
        assert_eq!(notified.len(), 3);
    }

    #[test]
    fn multi_threshold_notification_state_survives_restart() {
        let thresholds = [2, 5, 10];
        let mut alerts = Vec::new();
        let mut notified = BTreeSet::new();
        append_test_codex_session_alerts(&mut alerts, &mut notified, &thresholds, 4.0, 0);
        let persisted: BTreeSet<String> =
            serde_json::from_str(&serde_json::to_string(&notified).unwrap()).unwrap();

        let mut restarted_alerts = Vec::new();
        let mut restarted_notified = persisted;
        append_test_codex_session_alerts(
            &mut restarted_alerts,
            &mut restarted_notified,
            &thresholds,
            4.0,
            4 * 60,
        );

        assert!(restarted_alerts.is_empty());
        assert!(restarted_notified.contains("codex:session:threshold:5:2000000240"));
        assert!(restarted_notified.contains("codex:session:threshold:10:2000000240"));
    }

    #[test]
    fn direct_zero_marks_thresholds_without_emitting_threshold_notifications() {
        let thresholds = [2, 5, 10];
        let mut alerts = Vec::new();
        let mut notified = BTreeSet::new();

        append_test_codex_session_alerts(&mut alerts, &mut notified, &thresholds, 0.0, 0);
        append_test_codex_session_alerts(&mut alerts, &mut notified, &thresholds, 1.0, 0);

        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].title, "Codex quota exhausted");
        assert_eq!(notified.len(), 4);
    }

    #[test]
    fn two_percent_threshold_is_persisted_and_fires() {
        let settings: SettingsFile = serde_json::from_str(
            r#"{"alert_thresholds_percent":[30,2,2,99],"alert_threshold_percent":0}"#,
        )
        .unwrap();
        let settings = normalize_settings(settings);
        assert_eq!(settings.alert_thresholds_percent, Some(vec![2, 30]));

        let mut alerts = Vec::new();
        let mut notified = BTreeSet::new();
        append_test_codex_session_alerts(&mut alerts, &mut notified, &[2], 2.0, 0);
        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].message.contains("2% remaining"));
    }

    #[test]
    fn weekly_multi_threshold_state_rebases_and_rearms_only_on_new_window() {
        let thresholds = [5, 10];
        let mut alerts = Vec::new();
        let mut notified = BTreeSet::new();

        for offset in [0, 4 * 60, 8 * 60, 12 * 60] {
            append_test_codex_weekly_alerts(&mut alerts, &mut notified, &thresholds, 4.0, offset);
        }
        assert_eq!(alerts.len(), 1);
        append_test_codex_weekly_alerts(&mut alerts, &mut notified, &thresholds, 4.0, 18_000);
        assert_eq!(alerts.len(), 2);
        assert!(notified.contains("codex:weekly:threshold:5:2000018000"));
    }

    #[test]
    fn credit_balance_rounds_to_the_nearest_whole_credit() {
        assert_eq!(format_credit_balance(&CreditBalance::Amount(360.89)), "361");
        assert_eq!(format_credit_balance(&CreditBalance::Amount(360.01)), "360");
        assert_eq!(format_credit_balance(&CreditBalance::Amount(360.50)), "361");
        assert_eq!(format_credit_balance(&CreditBalance::Amount(360.0)), "360");
        assert_eq!(format_credit_balance(&CreditBalance::Amount(0.99)), "1");
        assert_eq!(format_credit_balance(&CreditBalance::Amount(0.0)), "0");
        assert_eq!(format_credit_balance(&CreditBalance::Unlimited), "∞");
    }

    #[test]
    fn usd_credit_value_uses_precise_balance_and_cent_rounding() {
        assert_eq!(
            format_credit_value(&CreditBalance::Amount(360.0), CreditValueMode::UsdEstimate),
            "~$14.40"
        );
        assert_eq!(
            format_credit_value(
                &CreditBalance::Amount(360.839693),
                CreditValueMode::UsdEstimate
            ),
            "~$14.43"
        );
        assert_eq!(
            format_credit_value(&CreditBalance::Amount(0.0), CreditValueMode::UsdEstimate),
            "~$0.00"
        );
        assert_eq!(
            format_credit_value(&CreditBalance::Unlimited, CreditValueMode::UsdEstimate),
            "∞"
        );
    }

    #[test]
    fn old_settings_default_credit_value_to_credits_and_value_mode_persists() {
        let old = normalize_settings(
            serde_json::from_str::<SettingsFile>(r#"{"show_codex":true}"#).unwrap(),
        );
        assert_eq!(old.credit_value_mode, "credits");

        let usd = normalize_settings(SettingsFile {
            credit_value_mode: "usd_estimate".to_string(),
            ..SettingsFile::default()
        });
        let serialized = serde_json::to_string(&usd).unwrap();
        let restored = normalize_settings(serde_json::from_str(&serialized).unwrap());
        assert_eq!(restored.credit_value_mode, "usd_estimate");
    }

    #[test]
    fn credit_panel_visibility_obeys_display_mode_and_quota_state() {
        let positive = test_codex_data(Some(CreditBalance::Amount(341.0)), 82.0, 41.0);
        let zero_session = test_codex_data(Some(CreditBalance::Amount(0.0)), 0.0, 41.0);
        let zero_weekly = test_codex_data(Some(CreditBalance::Unlimited), 82.0, 0.0);
        let unknown = test_codex_data(None, 0.0, 0.0);

        assert!(codex_credit_panel_visible(
            true,
            CreditDisplayMode::Always,
            Some(&positive)
        ));
        assert!(codex_credit_panel_visible(
            true,
            CreditDisplayMode::Always,
            Some(&zero_session)
        ));
        assert!(!codex_credit_panel_visible(
            true,
            CreditDisplayMode::Always,
            Some(&unknown)
        ));
        assert!(!codex_credit_panel_visible(
            true,
            CreditDisplayMode::WhenNeeded,
            Some(&positive)
        ));
        assert!(codex_credit_panel_visible(
            true,
            CreditDisplayMode::WhenNeeded,
            Some(&zero_session)
        ));
        assert!(codex_credit_panel_visible(
            true,
            CreditDisplayMode::WhenNeeded,
            Some(&zero_weekly)
        ));
        assert!(!codex_credit_panel_visible(
            true,
            CreditDisplayMode::WhenNeeded,
            Some(&unknown)
        ));
        assert!(!codex_credit_panel_visible(
            true,
            CreditDisplayMode::Off,
            Some(&zero_session)
        ));
        assert!(!codex_credit_panel_visible(
            false,
            CreditDisplayMode::Always,
            Some(&positive)
        ));
    }

    #[test]
    fn credit_panel_visibility_hides_after_exhaustion_recovers() {
        let exhausted = test_codex_data(Some(CreditBalance::Amount(12.0)), 0.0, 25.0);
        let usable = test_codex_data(Some(CreditBalance::Amount(12.0)), 12.0, 25.0);

        assert!(codex_credit_panel_visible(
            true,
            CreditDisplayMode::WhenNeeded,
            Some(&exhausted)
        ));
        assert!(!codex_credit_panel_visible(
            true,
            CreditDisplayMode::WhenNeeded,
            Some(&usable)
        ));
    }

    #[test]
    fn credit_panel_selection_follows_display_mode_and_exhaustion() {
        let mut credits = test_codex_data(Some(CreditBalance::Amount(12.0)), 0.0, 25.0);
        for (mode, visible) in [
            (CreditDisplayMode::Always, true),
            (CreditDisplayMode::WhenNeeded, true),
            (CreditDisplayMode::Off, false),
        ] {
            assert_eq!(
                codex_credit_panel_visible(true, mode, Some(&credits)),
                visible
            );
        }
        assert_eq!(
            credit_panel_reason(true, CreditDisplayMode::WhenNeeded, Some(&credits)),
            "finite_credits_available"
        );

        // A known zero balance is still shown, as the real exhaustion test did.
        credits.codex.as_mut().unwrap().credits = Some(CreditBalance::Amount(0.0));
        assert!(codex_credit_panel_visible(
            true,
            CreditDisplayMode::WhenNeeded,
            Some(&credits)
        ));
        assert_eq!(
            credit_panel_reason(true, CreditDisplayMode::WhenNeeded, Some(&credits)),
            "credits_exhausted"
        );

        // When needed hides Credits while included usage remains.
        credits.codex.as_mut().unwrap().session.percentage = 40.0;
        assert!(!codex_credit_panel_visible(
            true,
            CreditDisplayMode::WhenNeeded,
            Some(&credits)
        ));
        assert!(codex_credit_panel_visible(
            true,
            CreditDisplayMode::Always,
            Some(&credits)
        ));
        assert_eq!(
            credit_panel_reason(true, CreditDisplayMode::WhenNeeded, Some(&credits)),
            "included_usage_available"
        );

        // Unknown Credits never show a panel; hidden Codex never does.
        credits.codex.as_mut().unwrap().credits = None;
        assert!(!codex_credit_panel_visible(
            true,
            CreditDisplayMode::Always,
            Some(&credits)
        ));
        assert!(!codex_credit_panel_visible(
            false,
            CreditDisplayMode::Always,
            Some(&credits)
        ));
        assert_eq!(
            credit_panel_reason(false, CreditDisplayMode::Always, Some(&credits)),
            "disabled"
        );
    }

    #[test]
    fn transient_provider_failure_retains_only_that_provider_cache() {
        let mut previous = test_codex_data(Some(CreditBalance::Amount(9.0)), 74.0, 40.0);
        previous.claude_code = Some(crate::models::UsageData {
            session: crate::models::UsageSection {
                percentage: 20.0,
                available: true,
                ..Default::default()
            },
            ..Default::default()
        });
        // A same-credential Codex failure keeps Codex's cache and never touches Claude.
        let mut current = previous.clone();
        merge_provider_result(&mut current, CODEX, None, false);
        assert_eq!(current.codex.as_ref().unwrap().session.percentage, 26.0);
        assert_eq!(
            current.claude_code.as_ref().unwrap().session.percentage,
            20.0
        );
        assert!(current.antigravity.is_none());

        // A Claude success replaces only Claude.
        let mut claude = previous.claude_code.clone().unwrap();
        claude.session.percentage = 33.0;
        merge_provider_result(&mut current, CLAUDE, Some(claude), false);
        assert_eq!(
            current.claude_code.as_ref().unwrap().session.percentage,
            33.0
        );
        assert_eq!(current.codex.as_ref().unwrap().session.percentage, 26.0);

        let mut no_cache = AppUsageData::default();
        merge_provider_result(&mut no_cache, CODEX, None, false);
        assert!(no_cache.codex.is_none());
    }

    #[test]
    fn credit_panel_expands_widget_without_changing_quota_area_width() {
        let text_width = quota_text_width_for(LanguageId::English, &["--"]);
        let without_credits = total_widget_width_for(
            1,
            LanguageId::English,
            false,
            false,
            CreditValueMode::Credits,
            "",
            text_width,
        );
        let with_credits = total_widget_width_for(
            1,
            LanguageId::English,
            false,
            true,
            CreditValueMode::Credits,
            "351",
            text_width,
        );
        let (credit_header_width, _) = credit_layout_widths(LanguageId::English.strings(), "351");
        let credit_panel_width = credit_outer_width(credit_header_width);

        assert_eq!(with_credits - without_credits, sc(credit_panel_width));
        assert_eq!(RIGHT_MARGIN, 6);
    }

    #[test]
    fn quota_text_width_tracks_current_strings_without_fixed_dead_space() {
        let short = quota_text_width_for(LanguageId::English, &["40% 12m"]);
        let long = quota_text_width_for(LanguageId::English, &["100% 4h59m"]);

        assert!(long > short);
        assert_eq!(row_bar_segment_count(1), SEGMENT_COUNT);
        assert!(
            quota_area_width_for(1, LanguageId::English, long)
                > quota_area_width_for(1, LanguageId::English, short)
        );
    }

    #[test]
    fn quota_width_is_stable_within_text_shapes_and_expands_for_real_shape_changes() {
        let stable_pairs = [
            ("97% 3h58m", "97% 3h57m"),
            ("97% 3h58m", "97% 3h55m"),
            ("97% 58m", "97% 57m"),
            ("13% 6d", "13% 5d"),
            ("92% 59s", "92% 58s"),
        ];
        for (before, after) in stable_pairs {
            assert_eq!(
                quota_text_width_for(LanguageId::English, &[before]),
                quota_text_width_for(LanguageId::English, &[after]),
                "same display shape should keep a stable measured column: {before} -> {after}"
            );
        }

        let compact_session = quota_text_width_for(LanguageId::English, &["100% 5h"]);
        let detailed_session = quota_text_width_for(LanguageId::English, &["100% 4h58m"]);
        let one_digit_minute = quota_text_width_for(LanguageId::English, &["97% 9m"]);
        let two_digit_minute = quota_text_width_for(LanguageId::English, &["97% 10m"]);
        assert!(detailed_session > compact_session);
        assert!(two_digit_minute > one_digit_minute);
    }

    #[test]
    fn equivalent_countdown_layout_is_stable_for_credit_sides_grip_states_and_dpi() {
        let before_text = quota_text_width_for(LanguageId::English, &["97% 3h58m"]);
        let after_text = quota_text_width_for(LanguageId::English, &["97% 3h57m"]);
        assert_eq!(before_text, after_text);

        for dpi in [96, 120, 144, 192] {
            for show_drag_handle in [false, true] {
                for credit_position in [CreditPosition::Left, CreditPosition::Right] {
                    let before_layout = widget_content_positions_for(
                        1,
                        LanguageId::English,
                        show_drag_handle,
                        true,
                        credit_position,
                        CreditValueMode::Credits,
                        "361",
                        before_text,
                    );
                    let after_layout = widget_content_positions_for(
                        1,
                        LanguageId::English,
                        show_drag_handle,
                        true,
                        credit_position,
                        CreditValueMode::Credits,
                        "361",
                        after_text,
                    );
                    assert_eq!(before_layout, after_layout);

                    let before_width = total_widget_width_for(
                        1,
                        LanguageId::English,
                        show_drag_handle,
                        true,
                        CreditValueMode::Credits,
                        "361",
                        before_text,
                    );
                    let after_width = total_widget_width_for(
                        1,
                        LanguageId::English,
                        show_drag_handle,
                        true,
                        CreditValueMode::Credits,
                        "361",
                        after_text,
                    );
                    assert_eq!(
                        scale_logical_at_dpi(before_width, dpi),
                        scale_logical_at_dpi(after_width, dpi),
                        "widget bounds should remain stable at {dpi} DPI"
                    );
                    assert_eq!(
                        scale_logical_at_dpi(before_layout.0, dpi),
                        scale_logical_at_dpi(after_layout.0, dpi),
                        "quota anchor should remain stable at {dpi} DPI"
                    );
                    assert_eq!(
                        before_layout.1.map(|x| scale_logical_at_dpi(x, dpi)),
                        after_layout.1.map(|x| scale_logical_at_dpi(x, dpi)),
                        "Credits anchor should remain stable at {dpi} DPI"
                    );
                }
            }
        }

        let short_text = quota_text_width_for(LanguageId::English, &["100% 5h"]);
        let long_text = quota_text_width_for(LanguageId::English, &["100% 4h58m"]);
        assert!(long_text > short_text);
        assert!(
            quota_area_width_for(1, LanguageId::English, long_text)
                > quota_area_width_for(1, LanguageId::English, short_text)
        );
    }

    #[test]
    fn quota_text_measurement_covers_countdown_shapes_dpi_and_layout_variants() {
        let candidates = [
            "100% 5h",
            "9% 4h59m",
            "10% 47m",
            "82% 3h14m",
            "92% 59s",
            "40% 12m",
        ];
        let widest = quota_text_width_for(LanguageId::English, &candidates);
        assert!(candidates
            .iter()
            .all(|text| widest >= quota_text_width_for(LanguageId::English, &[*text])));

        for dpi in [96, 120, 144, 192] {
            let physical_width = (widest as f64 * dpi as f64 / 96.0).ceil() as i32;
            let logical = logical_width_from_physical(physical_width, dpi).unwrap();
            let scaled_back = (logical as f64 * dpi as f64 / 96.0).round() as i32;
            assert!(scaled_back >= physical_width - 1, "DPI {dpi}");
        }
        assert_eq!(logical_width_from_physical(0, 96), None);
        assert_eq!(logical_width_from_physical(10, 0), None);

        for model_count in 1..=3 {
            for show_drag_handle in [false, true] {
                for credit_position in [CreditPosition::Left, CreditPosition::Right] {
                    let (quota_x, credit_x) = widget_content_positions_for(
                        model_count,
                        LanguageId::English,
                        show_drag_handle,
                        true,
                        credit_position,
                        CreditValueMode::Credits,
                        "361",
                        widest,
                    );
                    assert!(quota_x >= drag_handle_reserved_width(show_drag_handle));
                    match credit_position {
                        CreditPosition::Left => assert!(credit_x.unwrap() < quota_x),
                        CreditPosition::Right => assert!(credit_x.unwrap() > quota_x),
                    }
                    assert_eq!(
                        row_bar_segment_count(model_count),
                        match model_count {
                            1 => SEGMENT_COUNT,
                            2 => 5,
                            _ => 4,
                        }
                    );
                    assert!(
                        total_widget_width_for(
                            model_count,
                            LanguageId::English,
                            show_drag_handle,
                            true,
                            CreditValueMode::Credits,
                            "361",
                            widest,
                        ) > quota_area_width_for(model_count, LanguageId::English, widest)
                    );
                }
            }
        }
    }

    #[test]
    fn usd_credit_value_overflows_only_outward_and_keeps_quota_gutters() {
        let credits = credit_layout_widths(LanguageId::English.strings(), "351");
        let usd = credit_layout_widths(LanguageId::English.strings(), "~$1234.56");
        assert_eq!(usd.0, credits.0);
        assert!(usd.1 > credits.1);
        let overflow = credit_outward_overflow(credits.0, usd.1);
        assert!(overflow > 0);

        let text_width = quota_text_width_for(LanguageId::English, &["--"]);
        let quota_width = quota_area_width_for(1, LanguageId::English, text_width);
        let quota_gutter = sc(HORIZONTAL_GUTTER);
        assert_eq!(credit_outer_width(credits.0), credit_outer_width(usd.0));
        for show_drag_handle in [false, true] {
            let drag_width = drag_handle_reserved_width(show_drag_handle);
            let base_x = drag_width + quota_gutter;
            for position in [CreditPosition::Left, CreditPosition::Right] {
                let old_total = total_widget_width_for(
                    1,
                    LanguageId::English,
                    show_drag_handle,
                    true,
                    CreditValueMode::Credits,
                    "351",
                    text_width,
                );
                let new_total = total_widget_width_for(
                    1,
                    LanguageId::English,
                    show_drag_handle,
                    true,
                    CreditValueMode::UsdEstimate,
                    "~$1234.56",
                    text_width,
                );
                assert_eq!(new_total - old_total, sc(overflow));

                let (old_quota_x, old_credit_x) = widget_content_positions_for(
                    1,
                    LanguageId::English,
                    show_drag_handle,
                    true,
                    position,
                    CreditValueMode::Credits,
                    "351",
                    text_width,
                );
                let (new_quota_x, new_credit_x) = widget_content_positions_for(
                    1,
                    LanguageId::English,
                    show_drag_handle,
                    true,
                    position,
                    CreditValueMode::UsdEstimate,
                    "~$1234.56",
                    text_width,
                );
                let old_credit_x = old_credit_x.unwrap();
                let new_credit_x = new_credit_x.unwrap();
                let value_x = credit_value_x(new_credit_x, usd.0, usd.1, position);

                match position {
                    CreditPosition::Left => {
                        assert_eq!(new_quota_x - old_quota_x, sc(overflow));
                        assert_eq!(new_credit_x - old_credit_x, sc(overflow));
                        // Under right-edge anchoring, adding the same outward room
                        // to width and local X leaves the quota/header screen X fixed.
                        assert_eq!(old_quota_x - old_total, new_quota_x - new_total);
                        assert_eq!(old_credit_x - old_total, new_credit_x - new_total);
                        assert!((value_x - base_x).abs() <= 1);
                        assert_eq!(new_quota_x - (value_x + sc(usd.1)), quota_gutter);
                        if show_drag_handle {
                            assert!(value_x >= drag_width + quota_gutter - 1);
                        }
                    }
                    CreditPosition::Right => {
                        assert_eq!(new_quota_x, old_quota_x);
                        assert_eq!(new_credit_x, old_credit_x);
                        assert_eq!(value_x, new_credit_x);
                        assert_eq!(new_total - (value_x + sc(usd.1)), sc(RIGHT_MARGIN));
                        assert_eq!(new_credit_x - new_quota_x, quota_width + quota_gutter);
                    }
                }
            }
        }
    }

    #[test]
    fn credit_panel_stack_is_centered_with_a_compact_vertical_gap() {
        let height = sc(WIDGET_HEIGHT);
        let (header_y, value_y) = credit_panel_y_positions(height);
        let segment_height = sc(SEGMENT_H);
        let gap = sc(CREDIT_VERTICAL_GAP);
        let stack_height = segment_height * 2 + gap;

        assert_eq!(value_y - header_y, segment_height + gap);
        assert_eq!(header_y, (height - stack_height) / 2);
    }

    #[test]
    fn credit_position_defaults_left_and_persists_independently() {
        let old: SettingsFile = serde_json::from_str(r#"{"show_codex":true}"#).unwrap();
        let old = normalize_settings(old);
        assert_eq!(old.credit_display, default_credit_display());
        assert_eq!(old.credit_position, default_credit_position());

        let settings = SettingsFile {
            credit_display: "off".to_string(),
            credit_position: "right".to_string(),
            ..SettingsFile::default()
        };
        let serialized = serde_json::to_string(&settings).unwrap();
        let restored: SettingsFile = serde_json::from_str(&serialized).unwrap();
        let restored = normalize_settings(restored);
        assert_eq!(restored.credit_display, "off");
        assert_eq!(restored.credit_position, "right");

        let switched_visibility = normalize_settings(SettingsFile {
            credit_display: "always".to_string(),
            credit_position: restored.credit_position,
            ..SettingsFile::default()
        });
        assert_eq!(switched_visibility.credit_position, "right");
    }

    #[test]
    fn credit_position_layout_keeps_width_and_drag_handle_for_both_sides() {
        let text_width = quota_text_width_for(LanguageId::English, &["--"]);
        let (left_quota_x, left_credit_x) = widget_content_positions_for(
            1,
            LanguageId::English,
            true,
            true,
            CreditPosition::Left,
            CreditValueMode::Credits,
            "351",
            text_width,
        );
        let (right_quota_x, right_credit_x) = widget_content_positions_for(
            1,
            LanguageId::English,
            true,
            true,
            CreditPosition::Right,
            CreditValueMode::Credits,
            "351",
            text_width,
        );

        assert!(left_credit_x.unwrap() < left_quota_x);
        assert!(right_quota_x < right_credit_x.unwrap());
        let base_content_x = sc(DRAG_HANDLE_HIT_W) + sc(HORIZONTAL_GUTTER);
        let (credit_header_width, _) = credit_layout_widths(LanguageId::English.strings(), "351");
        let credit_panel_width = credit_outer_width(credit_header_width);
        let quota_area_width = quota_area_width_for(1, LanguageId::English, text_width);
        let total_width = total_widget_width_for(
            1,
            LanguageId::English,
            true,
            true,
            CreditValueMode::Credits,
            "351",
            text_width,
        );
        assert_eq!(left_credit_x.unwrap(), base_content_x);
        assert_eq!(
            left_credit_x.unwrap()
                + sc(credit_header_width + HORIZONTAL_GUTTER)
                + quota_area_width
                + sc(RIGHT_MARGIN),
            total_width
        );
        assert_eq!(
            right_credit_x.unwrap() + sc(credit_panel_width - HORIZONTAL_GUTTER) + sc(RIGHT_MARGIN),
            total_width
        );
        assert!(is_drag_handle_point(true, 1, sc(WIDGET_HEIGHT) / 2));
        assert!(is_drag_handle_point(
            true,
            sc(DRAG_HANDLE_HIT_W - 1),
            sc(WIDGET_HEIGHT) / 2
        ));
        assert_eq!(row_bar_segment_count(1), SEGMENT_COUNT);
    }

    #[test]
    fn safe_taskbar_anchor_avoids_discoverable_right_side_widgets() {
        let taskbar = RECT {
            left: 0,
            top: 0,
            right: 1_000,
            bottom: 46,
        };
        let weather = RECT {
            left: 760,
            top: 0,
            right: 840,
            bottom: 46,
        };
        assert_eq!(
            taskbar_layout::safe_anchor_left(taskbar, 900, &[weather]),
            760
        );
        assert_eq!(taskbar_layout::safe_anchor_left(taskbar, 900, &[]), 900);
    }

    #[test]
    fn safe_taskbar_anchor_uses_nearest_of_multiple_occupied_regions() {
        let taskbar = RECT {
            left: 0,
            top: 0,
            right: 1_200,
            bottom: 46,
        };
        let widgets = [
            RECT {
                left: 900,
                top: 0,
                right: 950,
                bottom: 46,
            },
            RECT {
                left: 760,
                top: 0,
                right: 820,
                bottom: 46,
            },
        ];
        assert_eq!(
            taskbar_layout::safe_anchor_left(taskbar, 1_100, &widgets),
            760
        );
        let narrow = RECT {
            left: 0,
            top: 0,
            right: 320,
            bottom: 46,
        };
        assert_eq!(taskbar_layout::safe_anchor_left(narrow, 300, &[]), 300);
    }

    #[test]
    fn drag_handle_defaults_hidden_persists_and_controls_layout_and_hit_testing() {
        let old = normalize_settings(
            serde_json::from_str::<SettingsFile>(r#"{"show_codex":true}"#).unwrap(),
        );
        assert!(!old.show_drag_handle);

        for show_drag_handle in [false, true] {
            let settings = SettingsFile {
                show_drag_handle,
                tray_offset: 321,
                ..SettingsFile::default()
            };
            let restored = normalize_settings(
                serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap(),
            );
            assert_eq!(restored.show_drag_handle, show_drag_handle);
            assert_eq!(restored.tray_offset, 321);
        }

        let text_width = quota_text_width_for(LanguageId::English, &["--"]);
        let hidden_width = total_widget_width_for(
            1,
            LanguageId::English,
            false,
            false,
            CreditValueMode::Credits,
            "",
            text_width,
        );
        let shown_width = total_widget_width_for(
            1,
            LanguageId::English,
            true,
            false,
            CreditValueMode::Credits,
            "",
            text_width,
        );
        assert_eq!(shown_width - hidden_width, sc(DRAG_HANDLE_HIT_W));

        let (hidden_quota_x, _) = widget_content_positions_for(
            1,
            LanguageId::English,
            false,
            false,
            CreditPosition::Left,
            CreditValueMode::Credits,
            "",
            text_width,
        );
        let (shown_quota_x, _) = widget_content_positions_for(
            1,
            LanguageId::English,
            true,
            false,
            CreditPosition::Left,
            CreditValueMode::Credits,
            "",
            text_width,
        );
        assert_eq!(hidden_quota_x, sc(HORIZONTAL_GUTTER));
        assert_eq!(
            shown_quota_x - hidden_quota_x,
            drag_handle_reserved_width(true)
        );
        assert!(!is_drag_handle_point(false, 1, sc(WIDGET_HEIGHT) / 2));
        assert!(is_drag_handle_point(true, 1, sc(WIDGET_HEIGHT) / 2));
    }

    #[test]
    fn codex_tooltip_includes_known_credit_balance() {
        assert_eq!(
            service_tooltip_with_credit(
                "Codex",
                "82% 2h",
                "41% 3d",
                true,
                true,
                Some(("Credits", "341")),
            ),
            "Codex: 5h 82% 2h | 7d 41% 3d | Credits 341"
        );
    }
}

#[cfg(test)]
mod persisted_snapshot_tests {
    use super::*;
    use crate::models::{UsageData, UsageSection};
    use poller::PollError;

    fn state() -> AppState {
        AppState {
            hwnd: SendHwnd::from_hwnd(HWND::default()),
            taskbar_hwnd: None,
            tray_notify_hwnd: None,
            win_event_hook: None,
            is_dark: true,
            embedded: false,
            language_override: None,
            language: LanguageId::English,
            session_percent: 0.0,
            session_text: "--".into(),
            weekly_percent: 0.0,
            weekly_text: "--".into(),
            codex_session_percent: 0.0,
            codex_session_text: "--".into(),
            codex_weekly_percent: 0.0,
            codex_weekly_text: "--".into(),
            credit_display: CreditDisplayMode::from_setting("always"),
            credit_position: CreditPosition::from_setting("left"),
            codex_credit_text: String::new(),
            antigravity_session_percent: 0.0,
            antigravity_session_text: "--".into(),
            antigravity_weekly_percent: 0.0,
            antigravity_weekly_text: "--".into(),
            claude_code_available: true,
            show_claude_code: true,
            show_codex: true,
            show_antigravity: false,
            show_session_window: true,
            show_weekly_window: true,
            show_drag_handle: false,
            enable_codex_mcp: true,
            usage_display: UsageDisplayMode::Mixed,
            bar_color: None,
            credit_value_mode: CreditValueMode::Credits,
            alert_thresholds_percent: Vec::new(),
            notified_quota_windows: BTreeSet::new(),
            data: None,
            provider_health: Default::default(),
            provider_generation: 0,
            data_sources: Default::default(),
            persisted: Default::default(),
            claude_desktop_sign_in: true,
            poll_interval_ms: POLL_30_SEC,
            adaptive_polling: false,
            schedule: Default::default(),
            codex_auth: CodexAuthEpisode::default(),
            last_codex_exec_refresh_unix: None,
            force_notify_auth_error: false,
            last_poll_ok: false,
            update_status: UpdateStatus::Idle,
            last_update_check_unix: None,
            taskbar_index: 0,
            tray_offset: 0,
            manual_position: false,
            dragging: false,
            drag_start_mouse_x: 0,
            drag_start_client_x: 0,
            drag_start_offset: 0,
            widget_visible: true,
        }
    }

    fn usage(
        session: f64,
        session_reset: SystemTime,
        weekly: f64,
        weekly_reset: SystemTime,
    ) -> UsageData {
        UsageData {
            session: UsageSection {
                percentage: session,
                resets_at: Some(session_reset),
                available: true,
            },
            weekly: UsageSection {
                percentage: weekly,
                resets_at: Some(weekly_reset),
                available: true,
            },
            credits: None,
        }
    }

    /// Simulates restore_snapshots for one provider without touching disk.
    fn restore(state: &mut AppState, id: usize, data: UsageData, updated_at: SystemTime) {
        state.provider_health[id].restore("account".into(), updated_at);
        state.data_sources[id] = poller::DataSource::Restored;
        *provider_usage_mut(state.data.get_or_insert_with(Default::default), id) = Some(data);
        state.last_poll_ok = true;
    }

    fn apply(state: &mut AppState, id: usize, data: Option<UsageData>, error: Option<PollError>) {
        let invalidate =
            state.provider_health[id].record(Some("account".into()), error, SystemTime::now());
        merge_provider_result(
            state.data.get_or_insert_with(Default::default),
            id,
            data,
            invalidate,
        );
    }

    #[test]
    fn restart_during_claude_auth_failure_shows_last_values_stale_and_codex_live() {
        let now = SystemTime::now();
        let hours = |h: u64| Duration::from_secs(h * 3600);
        let mut state = state();
        let claude = usage(25.0, now + hours(2), 35.0, now + hours(30));
        let codex = usage(40.0, now + hours(3), 22.0, now + hours(90));
        restore(&mut state, CLAUDE, claude, now - hours(1));
        restore(&mut state, CODEX, codex, now - hours(1));
        refresh_usage_texts(&mut state);
        assert!(
            state.session_text.starts_with("!25%"),
            "{}",
            state.session_text
        );
        assert!(
            state.codex_session_text.starts_with('!'),
            "restored Codex is stale too"
        );

        // First polls: Claude's sign-in is expired, Codex succeeds.
        apply(&mut state, CLAUDE, None, Some(PollError::TokenExpired));
        let codex = usage(41.0, now + hours(3), 22.0, now + hours(90));
        apply(&mut state, CODEX, Some(codex), None);
        refresh_usage_texts(&mut state);
        assert!(
            state.session_text.starts_with("!25%"),
            "{}",
            state.session_text
        );
        assert!(
            state.weekly_text.starts_with("!35%"),
            "{}",
            state.weekly_text
        );
        assert_eq!(state.session_percent, 25.0);
        // Mixed shows Codex as Remaining: 59% left, fresh and unmarked.
        assert!(
            state.codex_session_text.starts_with("59%"),
            "{}",
            state.codex_session_text
        );

        // The CLI refreshes its own sign-in; the next poll recovers Claude.
        let claude = usage(26.0, now + hours(2), 35.0, now + hours(30));
        apply(&mut state, CLAUDE, Some(claude), None);
        refresh_usage_texts(&mut state);
        assert!(
            state.session_text.starts_with("26%"),
            "{}",
            state.session_text
        );
    }

    #[test]
    fn a_reset_that_passed_since_the_snapshot_is_unknown_not_the_old_value() {
        let now = SystemTime::now();
        let day = Duration::from_secs(86_400);
        let mut state = state();
        let before_reset = usage(80.0, now - Duration::from_secs(60), 35.0, now + day);
        restore(
            &mut state,
            CLAUDE,
            before_reset,
            now - Duration::from_secs(600),
        );
        refresh_usage_texts(&mut state);
        assert_eq!(state.session_text, "!--");
        assert!(
            state.session_percent.is_nan(),
            "empty bar, not a guessed value"
        );
        assert!(
            state.weekly_text.starts_with("!35%"),
            "{}",
            state.weekly_text
        );
        // A healthy provider past its reset asks for one prompt refresh...
        assert_eq!(reset_poll_pending(&state, now), [true, false, false]);
        // ...but a failing one keeps its own retry schedule (no 5 s loop).
        apply(&mut state, CLAUDE, None, Some(PollError::TokenExpired));
        assert_eq!(reset_poll_pending(&state, now), [false, false, false]);
        refresh_usage_texts(&mut state);
        assert_eq!(state.session_text, "!--");
        // A fresh observation after the reset shows real values again.
        let after_reset = usage(3.0, now + Duration::from_secs(18_000), 36.0, now + day);
        apply(&mut state, CLAUDE, Some(after_reset), None);
        refresh_usage_texts(&mut state);
        assert!(
            state.session_text.starts_with("3%"),
            "{}",
            state.session_text
        );
        assert_eq!(state.session_percent, 3.0);
    }

    #[test]
    fn a_different_account_never_shows_the_restored_snapshot() {
        let now = SystemTime::now();
        let mut state = state();
        let data = usage(25.0, now + Duration::from_secs(3600), 35.0, now);
        restore(&mut state, CLAUDE, data, now);
        let invalidate = state.provider_health[CLAUDE].record(
            Some("another-account".into()),
            Some(PollError::TokenExpired),
            now,
        );
        merge_provider_result(state.data.as_mut().unwrap(), CLAUDE, None, invalidate);
        assert!(invalidate);
        assert!(state.data.as_ref().unwrap().claude_code.is_none());
    }

    #[test]
    fn desktop_sign_in_fallback_defaults_on_and_round_trips() {
        let old: SettingsFile = serde_json::from_str("{}").unwrap();
        assert!(
            old.use_claude_desktop_sign_in,
            "existing settings get the fallback"
        );
        let off: SettingsFile =
            serde_json::from_str(r#"{"use_claude_desktop_sign_in":false}"#).unwrap();
        assert!(!off.use_claude_desktop_sign_in);
        let json = serde_json::to_string(&off).unwrap();
        assert!(json.contains("\"use_claude_desktop_sign_in\":false"));
        assert!(SettingsFile::default().use_claude_desktop_sign_in);
    }
}
