use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::ffi::c_void;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use std::os::windows::process::CommandExt;

use crate::diagnose;
use crate::localization::Strings;
use crate::models::{CreditBalance, UsageData, UsageSection};
use crate::native_interop;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const ANTIGRAVITY_CREDENTIAL_TARGET: &str = "gemini:antigravity";
const ANTIGRAVITY_ENDPOINTS: &[&str] = &[
    "https://daily-cloudcode-pa.googleapis.com",
    "https://daily-cloudcode-pa.sandbox.googleapis.com",
    "https://cloudcode-pa.googleapis.com",
];
const CREATE_NO_WINDOW: u32 = 0x08000000;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PollError {
    AuthRequired,
    NoCredentials,
    TokenExpired,
    NetworkUnavailable,
    RateLimited,
    ServerError,
    RequestFailed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsageWindowKind {
    Session,
    Weekly,
}

impl PollError {
    pub fn category(self) -> &'static str {
        match self {
            Self::AuthRequired => "auth_required",
            Self::NoCredentials => "no_credentials",
            Self::TokenExpired => "token_expired",
            Self::NetworkUnavailable => "network_unavailable",
            Self::RateLimited => "rate_limited",
            Self::ServerError => "server_error",
            Self::RequestFailed => "invalid_response",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialWatchMode {
    Codex,
}

pub type CredentialWatchSnapshot = Vec<String>;

/// Result of one provider request, produced on that provider's own worker.
#[derive(Debug, Default)]
pub struct ProviderPoll {
    pub data: Option<UsageData>,
    pub error: Option<PollError>,
    /// Server Retry-After deadline from a 429 response, in milliseconds.
    pub retry_after_ms: Option<u32>,
    /// Account scope after the request (stable across token refreshes);
    /// `None` when no credentials identify an account.
    pub scope: Option<String>,
    /// When the returned data was observed, if not "now" (a local cache).
    pub observed_at: Option<SystemTime>,
    pub source: DataSource,
    /// On failure: an account-matched official reading from Claude Code's
    /// local cache, for display as stale data when newer than ours.
    pub stale_fallback: Option<(UsageData, SystemTime)>,
}

/// Where a provider's displayed values came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DataSource {
    /// This monitor's own request.
    #[default]
    Live,
    /// Claude Code's own cached `/api/oauth/usage` reading (`~/.claude.json`).
    ClaudeCodeCache,
    /// This monitor's persisted snapshot, restored at startup.
    Restored,
}

impl DataSource {
    pub fn name(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::ClaudeCodeCache => "claude_code_cache",
            Self::Restored => "restored_snapshot",
        }
    }
}

/// Claude Code's cached reading is used instead of a request while it is
/// younger than Claude's own request spacing.
const CLAUDE_LOCAL_CACHE_FRESH: Duration =
    Duration::from_millis(crate::provider_schedule::CLAUDE_MIN_POLL_MS as u64);

static CLAUDE_DESKTOP_SIGN_IN: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

/// Whether an expired or rejected CLI sign-in may fall back to the Claude
/// Desktop app's own sign-in (read-only). On by default.
pub fn set_claude_desktop_sign_in(enabled: bool) {
    CLAUDE_DESKTOP_SIGN_IN.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

/// The Antigravity quota group the owner chose (`None` = Auto).
static ANTIGRAVITY_QUOTA_GROUP: Mutex<Option<String>> = Mutex::new(None);
/// Group names from the latest Antigravity quota summary, for the menu.
static ANTIGRAVITY_QUOTA_GROUPS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Which Antigravity quota group to show: a group name as Antigravity reports
/// it, or `None` for Auto (Gemini Models, else the first group).
pub fn set_antigravity_quota_group(group: Option<String>) {
    *ANTIGRAVITY_QUOTA_GROUP
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = group;
}

/// The quota groups Antigravity reported most recently.
pub fn antigravity_quota_groups() -> Vec<String> {
    ANTIGRAVITY_QUOTA_GROUPS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

#[derive(Default)]
struct ClaudeObservation {
    observed_at: Option<SystemTime>,
    source: DataSource,
    fallback: Option<(UsageData, SystemTime)>,
    /// Account used when it is not the CLI's (Desktop sign-in, no CLI account).
    account_override: Option<String>,
}

pub fn provider_name(id: usize) -> &'static str {
    match id {
        0 => "claude",
        1 => "codex",
        _ => "antigravity",
    }
}

/// `/api/oauth/usage` body; Claude Code caches the same shape locally.
#[derive(Deserialize)]
pub(crate) struct UsageResponse {
    five_hour: Option<UsageBucket>,
    seven_day: Option<UsageBucket>,
}

#[derive(Deserialize)]
struct UsageBucket {
    utilization: f64,
    resets_at: Option<String>,
}

#[derive(Deserialize)]
struct CodexAuthFile {
    tokens: Option<CodexTokenData>,
}

#[derive(Clone, Deserialize)]
struct CodexTokenData {
    access_token: String,
    account_id: Option<String>,
}

#[derive(Deserialize)]
struct CodexUsageResponse {
    rate_limit: Option<Option<Box<CodexRateLimitDetails>>>,
    credits: Option<CodexCredits>,
}

#[derive(Deserialize)]
struct CodexCredits {
    unlimited: Option<bool>,
    balance: Option<CodexCreditBalanceValue>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum CodexCreditBalanceValue {
    Text(String),
    Number(f64),
}

#[derive(Deserialize)]
struct CodexRateLimitDetails {
    primary_window: Option<Option<Box<CodexRateLimitWindow>>>,
    secondary_window: Option<Option<Box<CodexRateLimitWindow>>>,
}

#[derive(Deserialize)]
struct CodexRateLimitWindow {
    used_percent: f64,
    reset_at: i64,
    limit_window_seconds: Option<u64>,
}

const CODEX_SESSION_WINDOW_MAX_SECONDS: u64 = 24 * 60 * 60;

#[derive(Deserialize)]
struct AntigravityAuthFile {
    token: AntigravityTokenData,
}

#[derive(Deserialize)]
struct AntigravityTokenData {
    access_token: String,
    /// RFC 3339 expiry written by Antigravity with the token.
    #[serde(default)]
    expiry: Option<String>,
}

#[derive(Deserialize)]
struct AntigravityLoadResponse {
    #[serde(rename = "cloudaicompanionProject")]
    project: Option<String>,
}

#[derive(Deserialize)]
struct AntigravityModelsResponse {
    models: HashMap<String, AntigravityModelInfo>,
}

#[derive(Deserialize)]
struct AntigravityModelInfo {
    #[serde(rename = "quotaInfo")]
    quota_info: Option<AntigravityQuotaInfo>,
}

#[derive(Deserialize)]
struct AntigravityQuotaInfo {
    #[serde(rename = "remainingFraction")]
    remaining_fraction: Option<f64>,
    #[serde(rename = "resetTime")]
    reset_time: Option<String>,
}

#[derive(Deserialize)]
struct AntigravityQuotaSummaryResponse {
    groups: Option<Vec<AntigravityQuotaSummaryGroup>>,
}

#[derive(Deserialize)]
struct AntigravityQuotaSummaryGroup {
    #[serde(rename = "displayName")]
    display_name: Option<String>,
    description: Option<String>,
    buckets: Option<Vec<AntigravityQuotaSummaryBucket>>,
}

#[derive(Clone, Deserialize)]
struct AntigravityQuotaSummaryBucket {
    #[serde(rename = "bucketId")]
    bucket_id: Option<String>,
    #[serde(rename = "displayName")]
    display_name: Option<String>,
    window: Option<String>,
    #[serde(rename = "remainingFraction")]
    remaining_fraction: Option<f64>,
    #[serde(rename = "resetTime")]
    reset_time: Option<String>,
}

#[repr(C)]
struct CredentialW {
    flags: u32,
    type_: u32,
    target_name: *mut u16,
    comment: *mut u16,
    last_written: u64,
    credential_blob_size: u32,
    credential_blob: *mut u8,
    persist: u32,
    attribute_count: u32,
    attributes: *mut c_void,
    target_alias: *mut u16,
    user_name: *mut u16,
}

#[link(name = "Advapi32")]
extern "system" {
    fn CredReadW(
        target_name: *const u16,
        type_: u32,
        reserved_flags: u32,
        credential: *mut *mut CredentialW,
    ) -> i32;
    fn CredFree(buffer: *mut c_void);
}

/// Request one provider. Runs on that provider's own worker thread so a slow
/// or failing provider never blocks the others.
pub fn poll_provider(id: usize) -> ProviderPoll {
    let started = std::time::Instant::now();
    diagnose::log(format!(
        "provider request started provider={}",
        provider_name(id)
    ));
    let before = (cache_scope(id), account_scope(id));
    RETRY_AFTER_HINT.with(|cell| cell.set(None));
    CLAUDE_OBSERVATION.with(|cell| cell.take());
    let result = match id {
        0 => poll_claude_code(),
        1 => poll_codex(),
        _ => poll_antigravity(),
    };
    let retry_after_ms = RETRY_AFTER_HINT.with(|cell| cell.take());
    let observation = CLAUDE_OBSERVATION.with(|cell| cell.take());
    let after = (cache_scope(id), account_scope(id));
    let account = observation
        .account_override
        .as_deref()
        .map(|uuid| account_digest("claude", uuid))
        .or_else(|| after.1.clone());
    let mut poll = finish_provider_poll(id, result, before, after, retry_after_ms);
    poll.scope = account;
    if poll.data.is_some() {
        poll.observed_at = observation.observed_at;
        poll.source = observation.source;
    } else {
        poll.stale_fallback = observation.fallback;
    }
    diagnose::log(format!(
        "provider request completed provider={} elapsed_ms={} error={:?} retry_after_ms={:?}",
        provider_name(id),
        started.elapsed().as_millis(),
        poll.error,
        poll.retry_after_ms
    ));
    poll
}

fn finish_provider_poll<S: PartialEq>(
    id: usize,
    result: Result<UsageData, PollError>,
    before: (Option<String>, S),
    after: (Option<String>, S),
    retry_after_ms: Option<u32>,
) -> ProviderPoll {
    // Never publish a response from credentials that changed during the request.
    let result = if before != after {
        Err(PollError::AuthRequired)
    } else {
        result
    };
    let (data, error) = match result {
        Ok(data) => (Some(data), None),
        Err(error) => {
            diagnose::log(format!(
                "{} usage poll failed category={}",
                match id {
                    0 => "Claude Code",
                    1 => "Codex",
                    _ => "Antigravity",
                },
                error.category()
            ));
            (None, Some(error))
        }
    };
    if let Some(usage) = data.as_ref() {
        log_provider_success(id, usage);
    }
    ProviderPoll {
        data,
        error,
        retry_after_ms: retry_after_ms.filter(|_| error == Some(PollError::RateLimited)),
        scope: after.0,
        ..Default::default()
    }
}

fn log_provider_success(id: usize, usage: &UsageData) {
    // Reset times are public window boundaries, logged so rollovers can be
    // correlated with what the widget showed.
    let reset = |section: &UsageSection| {
        section
            .resets_at
            .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
            .map(|at| at.as_secs())
    };
    match id {
        0 => diagnose::log(format!(
            "Claude usage poll succeeded session_used={:.2}% session_reset_unix={:?} weekly_used={:.2}% weekly_reset_unix={:?}",
            usage.session.percentage,
            reset(&usage.session),
            usage.weekly.percentage,
            reset(&usage.weekly)
        )),
        1 => log_codex_success(usage),
        _ => diagnose::log(format!(
            "Antigravity usage poll succeeded group={} session_used={:.2}% session_reset_unix={:?} weekly_used={:.2}% weekly_reset_unix={:?}",
            usage.quota_group.as_deref().unwrap_or("unnamed"),
            usage.session.percentage,
            reset(&usage.session),
            usage.weekly.percentage,
            reset(&usage.weekly)
        )),
    }
}

// Conservative token-bound cache identity, adapted from upstream PR #30.
// Digests stay in memory and are never logged or exposed over MCP.
fn credential_scope(provider: &str, token: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    digest.update(provider.as_bytes());
    digest.update([0]);
    digest.update(token.as_bytes());
    format!("{:x}", digest.finalize())
}

pub fn cache_scope(id: usize) -> Option<String> {
    match id {
        0 => read_first_credentials().map(|c| credential_scope("claude", &c.access_token)),
        1 => read_codex_credentials().map(|c| {
            credential_scope(
                "codex",
                &format!("{}|{}", c.access_token, c.account_id.unwrap_or_default()),
            )
        }),
        2 => {
            read_antigravity_credentials().map(|c| credential_scope("antigravity", &c.access_token))
        }
        _ => None,
    }
}

/// Fingerprint of a provider account identifier. Unlike token digests it
/// survives token refreshes, so persisted snapshots stay attached to the
/// account; a different account never matches.
fn account_digest(provider: &str, account: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    digest.update(b"codex-usage-monitor\0account-v1\0");
    digest.update(provider.as_bytes());
    digest.update([0]);
    digest.update(account.as_bytes());
    format!("{:x}", digest.finalize())
}

/// Account scope for snapshots and cache invalidation: the account
/// identifier when the provider exposes one, otherwise (conservatively) the
/// current token. Never logged or exposed over MCP.
pub fn account_scope(id: usize) -> Option<String> {
    match id {
        0 => crate::claude_local::read()
            .account_uuid
            .map(|uuid| account_digest("claude", &uuid))
            .or_else(|| cache_scope(0)),
        1 => {
            let creds = read_codex_credentials()?;
            match creds.account_id.as_deref().filter(|id| !id.is_empty()) {
                Some(account) => Some(account_digest("codex", account)),
                None => cache_scope(1),
            }
        }
        _ => cache_scope(id),
    }
}

/// Whether Claude Code CLI credentials are available from a supported source.
/// This decides the default provider selection only; polling may also use
/// the Claude Desktop sign-in when the CLI sign-in has expired.
pub fn claude_code_credentials_available() -> bool {
    let _ = resolve_windows_claude_path();
    read_first_credentials().is_some()
}

fn log_codex_success(codex: &UsageData) {
    let reset = |value: Option<SystemTime>| {
        value
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs())
    };
    let credits_state = match codex.credits.as_ref() {
        Some(CreditBalance::Amount(_)) => "amount",
        Some(CreditBalance::Unlimited) => "unlimited",
        None => "unknown",
    };
    diagnose::log(format!(
        "Codex usage poll succeeded session_used={:.2}% session_remaining={:.2}% session_reset_unix={:?} weekly_used={:.2}% weekly_remaining={:.2}% weekly_reset_unix={:?} credits={credits_state}",
        codex.session.percentage,
        remaining_percentage(codex.session.percentage),
        reset(codex.session.resets_at),
        codex.weekly.percentage,
        remaining_percentage(codex.weekly.percentage),
        reset(codex.weekly.resets_at),
    ));
}

/// What a probe run should do after the read-only report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeRequest {
    None,
    /// One normal poll (CLI sign-in first, Desktop only as fallback).
    NormalPoll,
    /// One usage GET with the matched Claude Desktop sign-in only.
    DesktopOnly,
}

/// Whether a one-shot Desktop probe request may run now. It must not add to
/// a running monitor's Claude polling, the 120 s spacing, or a server
/// Retry-After deadline. Refuses rather than waits; the reason says why.
fn desktop_probe_gate(
    monitor_running: bool,
    claude_enabled_in_settings: bool,
    history: ClaudeRequestHistory,
    now_unix: u64,
) -> Result<(), String> {
    if monitor_running && claude_enabled_in_settings {
        return Err("monitor_running_with_claude_enabled".into());
    }
    if let Some(deadline) = history.retry_deadline_unix.filter(|d| *d > now_unix) {
        return Err(format!(
            "retry_after_active remaining_s={}",
            deadline - now_unix
        ));
    }
    let spacing = u64::from(crate::provider_schedule::CLAUDE_MIN_POLL_MS / 1000);
    if let Some(last) = history.last_request_unix {
        let next = last.saturating_add(spacing);
        if next > now_unix {
            return Err(format!("claude_spacing remaining_s={}", next - now_unix));
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ClaudeRequestHistory {
    last_request_unix: Option<u64>,
    retry_deadline_unix: Option<u64>,
}

/// Claude request times from the operational log (any process): request
/// starts and 429 Retry-After hints. Lines are `[unix] pid=N message`.
fn claude_request_history(log: &str) -> ClaudeRequestHistory {
    let mut history = ClaudeRequestHistory::default();
    for line in log.lines() {
        let Some(time) = line
            .strip_prefix('[')
            .and_then(|rest| rest.split_once(']'))
            .and_then(|(time, _)| time.parse::<u64>().ok())
        else {
            continue;
        };
        if line.contains("provider request started provider=claude") {
            history.last_request_unix = Some(time);
        } else if let Some(hint) = line
            .split_once("Claude usage endpoint failed http_status=429 retry_after_ms=Some(")
            .and_then(|(_, rest)| rest.split_once(')'))
            .and_then(|(ms, _)| ms.parse::<u64>().ok())
        {
            let deadline = time.saturating_add(hint.div_ceil(1000));
            history.retry_deadline_unix = Some(
                history
                    .retry_deadline_unix
                    .map_or(deadline, |d| d.max(deadline)),
            );
        }
    }
    history
}

/// The tail of the real operational log; requests older than this are
/// irrelevant to the 120 s spacing and any capped Retry-After.
fn recent_log_text() -> String {
    use std::io::{Read, Seek, SeekFrom};
    const TAIL_BYTES: u64 = 2 * 1024 * 1024;
    let Some(path) = diagnose::active_log_file_path() else {
        return String::new();
    };
    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = file.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES)));
    let mut bytes = Vec::new();
    let _ = file.read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}

/// A monitor instance holds the `Global\CodexUsage` single-instance mutex.
fn monitor_instance_running() -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE};
    let name = native_interop::wide_str("Global\\CodexUsage");
    unsafe {
        match OpenMutexW(
            SYNCHRONIZATION_SYNCHRONIZE,
            false,
            windows::core::PCWSTR::from_raw(name.as_ptr()),
        ) {
            Ok(handle) => {
                let _ = CloseHandle(handle);
                true
            }
            Err(_) => false,
        }
    }
}

/// `show_claude_code` from the monitor's settings; unknown counts as enabled.
fn claude_enabled_in_settings() -> bool {
    let Some(appdata) = std::env::var_os("APPDATA") else {
        return true;
    };
    std::fs::read(
        PathBuf::from(appdata)
            .join("CodexUsage")
            .join("settings.json"),
    )
    .ok()
    .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
    .and_then(|settings| settings.get("show_claude_code")?.as_bool())
    .unwrap_or(true)
}

fn file_stamp(path: Option<PathBuf>) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(path?).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// One usage GET with the matched Desktop sign-in only: no CLI selection, no
/// rejected-token guards, no snapshot writes, no retry, no other source.
fn probe_desktop_request(cli: &crate::claude_local::CliState) -> String {
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if let Err(reason) = desktop_probe_gate(
        monitor_instance_running(),
        claude_enabled_in_settings(),
        claude_request_history(&recent_log_text()),
        now_unix,
    ) {
        return format!("desktop_request=refused reason={reason}");
    }
    let token = match crate::claude_desktop::token_for_account(cli.desktop_expected()) {
        Ok(token) => token,
        Err(reason) => {
            return format!("desktop_request=not_sent reason={}", reason.category());
        }
    };
    if is_token_expired(token.expires_at_ms) {
        return "desktop_request=not_sent reason=desktop_token_expired_locally".into();
    }
    let cli_file = windows_credentials_path();
    let cli_before = file_stamp(cli_file.clone());
    diagnose::log("provider request started provider=claude source=desktop_probe");
    RETRY_AFTER_HINT.with(|cell| cell.set(None));
    LAST_USAGE_HTTP_STATUS.with(|cell| cell.set(None));
    let result = try_usage_endpoint(&token.access_token);
    drop(token);
    let status = LAST_USAGE_HTTP_STATUS.with(|cell| cell.take());
    let retry_after_s = RETRY_AFTER_HINT
        .with(|cell| cell.take())
        .map(|ms| ms.div_ceil(1000));
    diagnose::log(format!(
        "claude desktop probe request completed http_status={status:?} error={}",
        result
            .as_ref()
            .err()
            .map(|e| e.category())
            .unwrap_or("none")
    ));
    let cli_unchanged = file_stamp(cli_file) == cli_before;
    let section = |s: &UsageSection| {
        if s.available {
            format!(
                "{:.0}% reset={}",
                s.percentage,
                if s.resets_at.is_some() {
                    "present"
                } else {
                    "absent"
                }
            )
        } else {
            "absent".into()
        }
    };
    let mut line = format!(
        "desktop_request=sent http_status={} result={}",
        status.map_or_else(|| "none".into(), |s| s.to_string()),
        result.as_ref().err().map(|e| e.category()).unwrap_or("ok")
    );
    if let Ok(data) = &result {
        line.push_str(&format!(
            " five_hour={} weekly={}",
            section(&data.session),
            section(&data.weekly)
        ));
    }
    if let Some(seconds) = retry_after_s {
        line.push_str(&format!(" retry_after_s={seconds}"));
    }
    line.push_str(&format!(" cli_credentials_unchanged={cli_unchanged}"));
    line
}

/// Read-only report for `--probe-claude-sign-in`: which Claude sources are
/// usable, without tokens, emails or account identifiers. `NormalPoll`
/// performs one normal usage poll; `DesktopOnly` one gated usage GET with the
/// Claude Desktop sign-in.
pub fn probe_claude_sign_in(request: ProbeRequest) -> String {
    let now = SystemTime::now();
    let cli = crate::claude_local::read();
    let mut lines = vec![format!("cli_account_known={}", cli.account_uuid.is_some())];
    lines.push(match cli.cached_usage.as_ref() {
        Some((_, at)) => format!(
            "cli_usage_cache=account_matched age_s={}",
            now.duration_since(*at).unwrap_or_default().as_secs()
        ),
        None => "cli_usage_cache=none".into(),
    });
    lines.push(match read_first_credentials() {
        None => "cli_credentials=missing".into(),
        Some(creds) => format!(
            "cli_credentials=present expired={} expires_in_s={}",
            is_token_expired(creds.expires_at),
            creds
                .expires_at
                .map(|exp| {
                    let now_ms = now
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as i64;
                    ((exp - now_ms) / 1000).to_string()
                })
                .unwrap_or_else(|| "unknown".into())
        ),
    });
    lines.extend(crate::claude_desktop::probe_lines(cli.desktop_expected()));
    let desktop_stamp = || file_stamp(crate::claude_desktop::config_path());
    let desktop_before = desktop_stamp();
    if request == ProbeRequest::DesktopOnly {
        lines.push(probe_desktop_request(&cli));
        lines.push(format!(
            "desktop_store_unchanged={}",
            desktop_stamp() == desktop_before
        ));
    }
    if request == ProbeRequest::NormalPoll {
        let poll = poll_provider(0);
        lines.push(format!(
            "usage_poll error={} source={} session_used={} weekly_used={}",
            poll.error.map(PollError::category).unwrap_or("none"),
            poll.source.name(),
            poll.data
                .as_ref()
                .map(|d| format!("{:.0}%", d.session.percentage))
                .unwrap_or_else(|| "-".into()),
            poll.data
                .as_ref()
                .map(|d| format!("{:.0}%", d.weekly.percentage))
                .unwrap_or_else(|| "-".into()),
        ));
    }
    lines.join("\n") + "\n"
}

thread_local! {
    static CLAUDE_OBSERVATION: std::cell::RefCell<ClaudeObservation> =
        std::cell::RefCell::new(ClaudeObservation::default());
}

fn record_claude_observation(update: impl FnOnce(&mut ClaudeObservation)) {
    CLAUDE_OBSERVATION.with(|cell| update(&mut cell.borrow_mut()));
}

/// Log a state line only when it differs from the previous one for `slot`,
/// so a long sign-in wait does not repeat itself every tick.
fn log_claude_state_change(slot: usize, message: String) {
    static LAST: Mutex<[Option<String>; 3]> = Mutex::new([None, None, None]);
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if last[slot].as_deref() != Some(message.as_str()) {
        diagnose::log(&message);
        last[slot] = Some(message);
    }
}

/// One Claude usage observation, strictly model-free and without touching
/// any credential store:
/// 1. Claude Code's own cached reading, when younger than Claude's request
///    spacing (no request at all).
/// 2. The Claude Code CLI sign-in (`~/.claude/.credentials.json`).
/// 3. When the CLI sign-in is expired, missing or rejected, the Claude
///    Desktop app's own sign-in for the same account (read-only).
///
/// Nothing here refreshes or rotates a token: renewal is left to Claude Code
/// and Claude Desktop, whose new credentials are picked up on the next tick.
fn poll_claude_code() -> Result<UsageData, PollError> {
    static CLI_REJECTED: OnceLock<Mutex<crate::provider_health::RejectedCredential>> =
        OnceLock::new();
    static DESKTOP_REJECTED: OnceLock<Mutex<crate::provider_health::RejectedCredential>> =
        OnceLock::new();
    let lock = |guard: &'static OnceLock<Mutex<crate::provider_health::RejectedCredential>>| {
        guard
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    };
    let cli = crate::claude_local::read();
    let desktop = || {
        CLAUDE_DESKTOP_SIGN_IN
            .load(std::sync::atomic::Ordering::Relaxed)
            .then(|| crate::claude_desktop::token_for_account(cli.desktop_expected()))
    };
    let result = poll_claude_sources(
        &cli,
        SystemTime::now(),
        read_first_credentials(),
        desktop,
        &mut lock(&CLI_REJECTED),
        &mut lock(&DESKTOP_REJECTED),
        fetch_usage_with_fallback,
    );
    if result.is_ok() {
        log_claude_state_change(2, "Claude sign-in available".to_string());
    }
    result
}

fn poll_claude_sources(
    cli: &crate::claude_local::CliState,
    now: SystemTime,
    cli_credentials: Option<Credentials>,
    desktop: impl FnOnce() -> Option<
        Result<crate::claude_desktop::DesktopToken, crate::claude_desktop::Unavailable>,
    >,
    cli_rejected: &mut crate::provider_health::RejectedCredential,
    desktop_rejected: &mut crate::provider_health::RejectedCredential,
    mut fetch: impl FnMut(&str) -> Result<UsageData, PollError>,
) -> Result<UsageData, PollError> {
    if let Some((data, fetched_at)) = cli.cached_usage.as_ref() {
        let age = now.duration_since(*fetched_at).unwrap_or_default();
        // A reading fetched before a reset that has since passed describes the
        // old window: it must not stand in for the post-reset request.
        let predates_reset = [&data.session, &data.weekly].iter().any(|section| {
            crate::provider_health::window_reset_since(section.resets_at, Some(*fetched_at), now)
        });
        if predates_reset {
            diagnose::log(
                "Claude Code's local cache predates a passed reset; requesting fresh usage",
            );
        } else if age < CLAUDE_LOCAL_CACHE_FRESH {
            diagnose::log(format!(
                "Claude usage served from Claude Code's local cache age_s={}; no request",
                age.as_secs()
            ));
            record_claude_observation(|o| {
                o.observed_at = Some(*fetched_at);
                o.source = DataSource::ClaudeCodeCache;
            });
            return Ok(data.clone());
        }
    }
    record_claude_observation(|o| o.fallback = cli.cached_usage.clone());

    let cli_error = match cli_credentials {
        None => PollError::NoCredentials,
        Some(creds) => match fetch_claude_passively(creds, cli_rejected, &mut fetch) {
            Ok(data) => return Ok(data),
            // Only an unusable sign-in falls through; 429, network and server
            // errors are shared by both sign-ins and back off as one provider.
            Err(error @ (PollError::TokenExpired | PollError::AuthRequired)) => error,
            Err(error) => return Err(error),
        },
    };

    match desktop() {
        None => {
            log_claude_state_change(
                1,
                "Claude Desktop sign-in fallback disabled in settings".to_string(),
            );
        }
        Some(Err(reason)) => log_claude_state_change(
            1,
            format!(
                "Claude Desktop sign-in unavailable reason={}",
                reason.category()
            ),
        ),
        Some(Ok(token)) if is_token_expired(token.expires_at_ms) => {
            log_claude_state_change(1, "Claude Desktop sign-in also expired".to_string());
        }
        Some(Ok(mut token)) => {
            log_claude_state_change(
                1,
                format!(
                    "Claude using the Claude Desktop sign-in (read-only); CLI sign-in category={}",
                    cli_error.category()
                ),
            );
            if cli.account_uuid.is_none() {
                record_claude_observation(|o| o.account_override = token.account_uuid.clone());
            }
            let creds = Credentials {
                access_token: std::mem::take(&mut token.access_token),
                expires_at: token.expires_at_ms,
            };
            return fetch_claude_passively(creds, desktop_rejected, fetch);
        }
    }

    if cli_error == PollError::TokenExpired {
        log_claude_state_change(
            2,
            "Claude sign-in expired; no verified model-free renewal is enabled, waiting for Claude Code or Claude Desktop to refresh its own sign-in".to_string(),
        );
    }
    Err(cli_error)
}

fn fetch_claude_passively(
    creds: Credentials,
    rejected: &mut crate::provider_health::RejectedCredential,
    fetch: impl FnOnce(&str) -> Result<UsageData, PollError>,
) -> Result<UsageData, PollError> {
    let scope = credential_scope("claude", &creds.access_token);
    if !rejected.permits(&scope) {
        diagnose::log("Claude auth recovery waiting for credential change; no usage request");
        return Err(PollError::AuthRequired);
    }
    if rejected.is_parked() {
        diagnose::log("Claude credential change detected; resuming usage requests");
    }
    let result = refresh_or_fallback(creds).and_then(|creds| fetch(&creds.access_token));
    rejected.record(scope, result.as_ref().map(|_| ()).map_err(|error| *error));
    result
}

fn poll_codex() -> Result<UsageData, PollError> {
    let creds = match read_codex_credentials() {
        Some(creds) => creds,
        None => {
            diagnose::log("Codex usage poll failed: no Codex credentials found");
            return Err(PollError::NoCredentials);
        }
    };

    fetch_codex_usage(&creds.access_token, creds.account_id.as_deref())
}

fn poll_antigravity() -> Result<UsageData, PollError> {
    // Antigravity's own language server reports every quota group with both
    // windows, exactly as its panel shows them. Google's service is the
    // fallback when Antigravity is not running.
    if let Some(data) = fetch_antigravity_local_usage() {
        return Ok(data);
    }

    let creds = match read_antigravity_credentials() {
        Some(creds) => creds,
        None => {
            diagnose::log("Antigravity usage poll failed: no Antigravity credentials found");
            return Err(PollError::NoCredentials);
        }
    };

    // An access token past its stored expiry is rejected by every endpoint;
    // wait for Antigravity to sign in again instead of sending it.
    if antigravity_token_expired(creds.expiry.as_deref(), SystemTime::now()) {
        log_claude_state_change(
            0,
            "Antigravity sign-in expired; waiting for Antigravity to renew it, no usage request"
                .to_string(),
        );
        return Err(PollError::TokenExpired);
    }
    log_claude_state_change(0, "Antigravity sign-in present".to_string());
    fetch_antigravity_usage(&creds.access_token)
}

// Monitoring never launches inference for authentication. Claude Code refreshes
// its own credentials when the user signs in; we watch those changes.
fn refresh_or_fallback(creds: Credentials) -> Result<Credentials, PollError> {
    if is_token_expired(creds.expires_at) {
        Err(PollError::TokenExpired)
    } else {
        Ok(creds)
    }
}

/// Ask Codex's own auth manager to refresh OAuth without starting a model task.
/// This uses the app-server's documented account/read refreshToken operation.
pub fn refresh_codex_token_model_free() -> bool {
    let codex_path = resolve_windows_codex_path();
    diagnose::log("starting model-free Codex app-server auth refresh");
    let mut command = codex_command(&codex_path, &["app-server", "--stdio"]);
    command
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(error) => {
            diagnose::log_error(
                "unable to start Codex app-server for model-free refresh",
                error,
            );
            return false;
        }
    };
    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.kill();
        return false;
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        return false;
    };
    let (sender, receiver) = mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            match line {
                Ok(line) => {
                    if sender.send(line).is_err() {
                        break;
                    }
                }
                _ => break,
            }
        }
    });

    let initialized = write_app_server_request(&mut stdin, app_server_initialize_request())
        && receive_app_server_response(&receiver, 1).is_some_and(|response| {
            response.get("error").is_none() && response.get("result").is_some()
        });

    let refreshed = if initialized
        && write_app_server_request(&mut stdin, app_server_initialized_notification())
        && write_app_server_request(&mut stdin, app_server_account_refresh_request())
    {
        receive_app_server_response(&receiver, 2).is_some_and(|response| {
            response.get("error").is_none() && response.get("result").is_some()
        })
    } else {
        false
    };

    drop(stdin);
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < Duration::from_secs(2) => {
                std::thread::sleep(Duration::from_millis(50));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
        }
    }
    let _ = reader.join();
    diagnose::log(format!(
        "model-free Codex app-server auth refresh {}",
        if refreshed { "completed" } else { "failed" }
    ));
    refreshed
}

/// Last-resort legacy recovery. Callers gate this behind passive retries and a
/// persistent cooldown; unlike app-server refresh, this may consume model use.
pub fn run_codex_exec_last_resort() -> bool {
    diagnose::log("model-consuming Codex recovery disabled; sign in through the official CLI");
    false
}

fn codex_command(codex_path: &str, args: &[&str]) -> Command {
    if codex_path.to_ascii_lowercase().ends_with(".cmd") {
        let mut command = Command::new("cmd.exe");
        command.arg("/c").arg(codex_path).args(args);
        command
    } else if codex_path.to_ascii_lowercase().ends_with(".ps1") {
        let mut command = Command::new("powershell.exe");
        command
            .arg("-NoProfile")
            .arg("-ExecutionPolicy")
            .arg("Bypass")
            .arg("-File")
            .arg(codex_path)
            .args(args);
        command
    } else {
        let mut command = Command::new(codex_path);
        command.args(args);
        command
    }
}

fn write_app_server_request(
    stdin: &mut std::process::ChildStdin,
    value: serde_json::Value,
) -> bool {
    serde_json::to_writer(&mut *stdin, &value).is_ok()
        && stdin.write_all(b"\n").is_ok()
        && stdin.flush().is_ok()
}

fn app_server_initialize_request() -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "clientInfo": {
                "name": "codex_usage_monitor",
                "title": "Codex Usage Monitor",
                "version": crate::build_info::VERSION
            },
            "capabilities": {"experimentalApi": false}
        }
    })
}

fn app_server_initialized_notification() -> serde_json::Value {
    serde_json::json!({"jsonrpc":"2.0","method":"initialized","params":{}})
}

fn app_server_account_refresh_request() -> serde_json::Value {
    serde_json::json!({
        "jsonrpc":"2.0",
        "id":2,
        "method":"account/read",
        "params":{"refreshToken":true}
    })
}

fn receive_app_server_response(
    receiver: &mpsc::Receiver<String>,
    request_id: u64,
) -> Option<serde_json::Value> {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let line = receiver.recv_timeout(remaining).ok()?;
        let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if message.get("id").and_then(serde_json::Value::as_u64) == Some(request_id) {
            return Some(message);
        }
    }
}

/// Spawn a command and wait up to `timeout` for it to finish.
/// Returns None if the process fails to start or exceeds the deadline.
fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> Option<std::process::Output> {
    let mut child = cmd.spawn().ok()?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => return None,
        }
    }
}

/// Resolve the full path to the `claude` CLI executable.
fn resolve_windows_claude_path() -> String {
    for name in &["claude.cmd", "claude"] {
        if Command::new(name)
            .arg("--version")
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
        {
            diagnose::log(format!("Claude CLI resolved via PATH command={name}"));
            return name.to_string();
        }
    }

    for name in &["claude.cmd", "claude"] {
        if let Ok(output) = Command::new("where.exe")
            .arg(name)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if let Some(first_line) = stdout.lines().next() {
                    let path = first_line.trim().to_string();
                    if !path.is_empty() {
                        diagnose::log("Claude CLI resolved via where.exe");
                        return path;
                    }
                }
            }
        }
    }

    // Common native Claude Code installs are sometimes not added to PATH.
    // PATH/where.exe remain preferred; these candidates are deliberately
    // derived from the current user's profile rather than hard-coded.
    if let Some(home) = dirs::home_dir() {
        for candidate in claude_user_install_candidates(&home) {
            if candidate.is_file() {
                diagnose::log("Claude CLI resolved from the user-local install directory");
                return candidate.to_string_lossy().to_string();
            }
        }
    }

    diagnose::log(
        "Claude CLI executable was not found on PATH or in the user-local install directory",
    );
    "claude.cmd".to_string()
}

fn claude_user_install_candidates(home: &std::path::Path) -> Vec<PathBuf> {
    let bin = home.join(".local").join("bin");
    vec![
        bin.join("claude.cmd"),
        bin.join("claude.exe"),
        bin.join("claude"),
    ]
}

fn resolve_windows_codex_path() -> String {
    for name in &["codex.cmd", "codex.ps1", "codex.exe", "codex"] {
        if Command::new(name)
            .arg("--version")
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
        {
            return name.to_string();
        }
    }

    for name in &["codex.cmd", "codex.ps1", "codex.exe", "codex"] {
        if let Ok(output) = Command::new("where.exe")
            .arg(name)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if let Some(first_line) = stdout.lines().next() {
                    let path = first_line.trim().to_string();
                    if !path.is_empty() {
                        return path;
                    }
                }
            }
        }
    }

    "codex.cmd".to_string()
}

fn build_agent() -> Result<ureq::Agent, PollError> {
    let tls = native_tls::TlsConnector::new().map_err(|_| PollError::RequestFailed)?;
    Ok(ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .tls_connector(std::sync::Arc::new(tls))
        .build())
}

fn classify_http_status(status: u16) -> PollError {
    match status {
        401 | 403 => PollError::AuthRequired,
        429 => PollError::RateLimited,
        500..=599 => PollError::ServerError,
        _ => PollError::RequestFailed,
    }
}

fn classify_ureq_error(error: &ureq::Error) -> PollError {
    match error {
        ureq::Error::Status(status, response) => {
            if *status == 429 {
                record_retry_after(response);
            }
            classify_http_status(*status)
        }
        ureq::Error::Transport(_) => PollError::NetworkUnavailable,
    }
}

thread_local! {
    // Each provider request runs on its own worker thread, so the hint from a
    // 429 response reaches poll_provider without widening the Copy PollError.
    static RETRY_AFTER_HINT: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
    // HTTP status of this thread's last usage request, for the probe only.
    static LAST_USAGE_HTTP_STATUS: std::cell::Cell<Option<u16>> = const { std::cell::Cell::new(None) };
}

fn record_retry_after(response: &ureq::Response) {
    let hint = response
        .header("Retry-After")
        .and_then(|value| crate::retry_after::delay_ms(value, SystemTime::now()));
    RETRY_AFTER_HINT.with(|cell| cell.set(hint));
}

pub fn credential_watch_snapshot(mode: CredentialWatchMode) -> CredentialWatchSnapshot {
    match mode {
        CredentialWatchMode::Codex => vec![codex_credential_watch_signature()],
    }
}

fn windows_credentials_path() -> Option<PathBuf> {
    windows_credentials_path_from(
        std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from),
        dirs::home_dir(),
    )
}

fn windows_credentials_path_from(
    config_dir: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    let config_dir = config_dir.filter(|path| !path.as_os_str().is_empty());
    let directory = config_dir.or_else(|| home.map(|path| path.join(".claude")))?;
    Some(directory.join(".credentials.json"))
}

fn fetch_usage_with_fallback(token: &str) -> Result<UsageData, PollError> {
    // The dedicated usage endpoint is a passive read. Do not fall back to a
    // Messages request: that request is a real model invocation and can
    // consume the user's Claude allowance.
    try_usage_endpoint(token)
}

fn try_usage_endpoint(token: &str) -> Result<UsageData, PollError> {
    let agent = build_agent()?;

    let result = agent
        .get(USAGE_URL)
        .set("Authorization", &format!("Bearer {token}"))
        .set("anthropic-beta", "oauth-2025-04-20")
        .call();
    LAST_USAGE_HTTP_STATUS.with(|cell| {
        cell.set(match &result {
            Ok(resp) => Some(resp.status()),
            Err(ureq::Error::Status(code, _)) => Some(*code),
            Err(_) => None,
        })
    });
    let resp = match result {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, _)) if code == 401 || code == 403 => {
            diagnose::log(format!(
                "Claude Code usage endpoint returned auth error status {code}; CLI credentials are stale or invalid"
            ));
            return Err(PollError::AuthRequired);
        }
        Err(ureq::Error::Status(code, response)) => {
            if code == 429 {
                record_retry_after(&response);
            }
            diagnose::log(format!(
                "Claude usage endpoint failed http_status={code} retry_after_ms={:?}",
                RETRY_AFTER_HINT.with(|cell| cell.get())
            ));
            return Err(classify_http_status(code));
        }
        Err(error) => return Err(classify_ureq_error(&error)),
    };

    let response: UsageResponse = match resp.into_json() {
        Ok(response) => response,
        Err(_) => return Err(PollError::RequestFailed),
    };
    claude_usage_from_response(&response).ok_or(PollError::RequestFailed)
}

/// Normalize a Claude usage body (live or Claude Code's cached copy).
pub(crate) fn claude_usage_from_response(response: &UsageResponse) -> Option<UsageData> {
    let mut data = UsageData::default();

    if let Some(bucket) = &response.five_hour {
        data.session.percentage = bucket.utilization;
        data.session.resets_at = parse_iso8601(bucket.resets_at.as_deref());
        data.session.available = true;
    }

    if let Some(bucket) = &response.seven_day {
        data.weekly.percentage = bucket.utilization;
        data.weekly.resets_at = parse_iso8601(bucket.resets_at.as_deref());
        data.weekly.available = true;
    }

    if !data.session.available && !data.weekly.available {
        return None;
    }
    Some(data)
}

fn fetch_codex_usage(token: &str, account_id: Option<&str>) -> Result<UsageData, PollError> {
    let agent = build_agent()?;
    diagnose::log_verbose("sending Codex usage request; authorization header omitted");
    let mut request = agent
        .get(CODEX_USAGE_URL)
        .set("Authorization", &format!("Bearer {token}"))
        .set("User-Agent", "codex-cli");

    if let Some(account_id) = account_id.filter(|value| !value.is_empty()) {
        request = request.set("ChatGPT-Account-Id", account_id);
    }

    let resp = match request.call() {
        Ok(resp) => resp,
        Err(error) => {
            let classified = classify_ureq_error(&error);
            let status = match error {
                ureq::Error::Status(code, _) => Some(code),
                _ => None,
            };
            diagnose::log(format!(
                "Codex usage endpoint failed category={} http_status={status:?}",
                classified.category()
            ));
            return Err(classified);
        }
    };

    let response: CodexUsageResponse = match resp.into_json() {
        Ok(response) => response,
        Err(error) => {
            diagnose::log_error("unable to parse Codex usage response", error);
            return Err(PollError::RequestFailed);
        }
    };

    let data = codex_usage_from_response(response).ok_or(PollError::RequestFailed)?;
    diagnose::log_verbose(format!(
        "Codex response normalized session_window={} weekly_window={} credits_state={}",
        data.session.available,
        data.weekly.available,
        match data.credits.as_ref() {
            Some(CreditBalance::Amount(_)) => "amount",
            Some(CreditBalance::Unlimited) => "unlimited",
            None => "unknown",
        }
    ));
    Ok(data)
}

fn codex_usage_from_response(response: CodexUsageResponse) -> Option<UsageData> {
    let mut data = UsageData::default();
    data.credits = parse_codex_credits(response.credits);

    let Some(details) = response.rate_limit.flatten().map(|details| *details) else {
        return data.credits.is_some().then_some(data);
    };
    let mut has_session = false;
    let mut has_weekly = false;

    let primary = details.primary_window.flatten();
    let secondary = details.secondary_window.flatten();

    for window in [primary.as_deref(), secondary.as_deref()]
        .into_iter()
        .flatten()
    {
        match codex_window_kind(window) {
            Some(UsageWindowKind::Session) if !has_session => {
                data.session = codex_section_from_window(window);
                has_session = true;
            }
            Some(UsageWindowKind::Weekly) if !has_weekly => {
                data.weekly = codex_section_from_window(window);
                has_weekly = true;
            }
            _ => {}
        }
    }

    // Older responses did not include a window duration. Preserve their
    // positional meaning while avoiding overwriting a duration-classified
    // window from the newer response shape.
    if let Some(window) = primary
        .as_deref()
        .filter(|window| window.limit_window_seconds.is_none() && !has_session)
    {
        data.session = codex_section_from_window(window);
    }
    if let Some(window) = secondary
        .as_deref()
        .filter(|window| window.limit_window_seconds.is_none() && !has_weekly)
    {
        data.weekly = codex_section_from_window(window);
    }

    Some(data)
}

fn parse_codex_credits(credits: Option<CodexCredits>) -> Option<CreditBalance> {
    let credits = credits?;
    if credits.unlimited == Some(true) {
        return Some(CreditBalance::Unlimited);
    }

    let balance = credits.balance?;
    let amount = match balance {
        CodexCreditBalanceValue::Text(value) => value.parse::<f64>().ok()?,
        CodexCreditBalanceValue::Number(value) => value,
    };
    if !amount.is_finite() || amount < 0.0 {
        // A malformed negative balance should remain unknown rather than
        // being turned into a fabricated zero balance.
        return None;
    }
    Some(CreditBalance::Amount(amount))
}

fn codex_window_kind(window: &CodexRateLimitWindow) -> Option<UsageWindowKind> {
    window.limit_window_seconds.map(|seconds| {
        if seconds <= CODEX_SESSION_WINDOW_MAX_SECONDS {
            UsageWindowKind::Session
        } else {
            UsageWindowKind::Weekly
        }
    })
}

fn codex_section_from_window(window: &CodexRateLimitWindow) -> UsageSection {
    UsageSection {
        percentage: window.used_percent,
        resets_at: unix_to_system_time(Some(window.reset_at)),
        available: true,
    }
}

fn fetch_antigravity_usage(token: &str) -> Result<UsageData, PollError> {
    let mut auth_error = false;
    let mut last_error = PollError::RequestFailed;

    for base_url in ANTIGRAVITY_ENDPOINTS {
        match fetch_antigravity_usage_from_endpoint(base_url, token) {
            Ok(data) => return Ok(data),
            Err(PollError::AuthRequired) => auth_error = true,
            Err(error) => last_error = error,
        }
    }

    if auth_error {
        Err(PollError::AuthRequired)
    } else {
        Err(last_error)
    }
}

fn fetch_antigravity_usage_from_endpoint(
    base_url: &str,
    token: &str,
) -> Result<UsageData, PollError> {
    let project = fetch_antigravity_project(base_url, token)?;
    if let Some(project) = project.as_deref() {
        match fetch_antigravity_quota_summary(base_url, token, project) {
            Ok(data) => return Ok(data),
            Err(PollError::AuthRequired) => return Err(PollError::AuthRequired),
            Err(error) => diagnose::log(format!(
                "Antigravity retrieveUserQuotaSummary failed, falling back to model quota: {error:?}"
            )),
        }
    }

    let (session, quota_group) =
        fetch_antigravity_model_quota(base_url, token, project.as_deref())?;

    Ok(UsageData {
        session,
        quota_group,
        ..UsageData::default()
    })
}

fn fetch_antigravity_project(base_url: &str, token: &str) -> Result<Option<String>, PollError> {
    let agent = build_agent()?;
    let body = serde_json::json!({
        "metadata": {
            "ideType": "ANTIGRAVITY"
        }
    });

    let resp = match agent
        .post(&format!("{base_url}/v1internal:loadCodeAssist"))
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .set("User-Agent", "antigravity")
        .send_json(&body)
    {
        Ok(resp) => resp,
        Err(error) => {
            let classified = classify_ureq_error(&error);
            diagnose::log_error("Antigravity loadCodeAssist request failed", error);
            return Err(classified);
        }
    };

    let response: AntigravityLoadResponse = match resp.into_json() {
        Ok(response) => response,
        Err(error) => {
            diagnose::log_error("unable to parse Antigravity loadCodeAssist response", error);
            return Err(PollError::RequestFailed);
        }
    };

    Ok(response.project.filter(|project| !project.is_empty()))
}

fn fetch_antigravity_model_quota(
    base_url: &str,
    token: &str,
    project: Option<&str>,
) -> Result<(UsageSection, Option<String>), PollError> {
    let agent = build_agent()?;
    let body = match project {
        Some(project) => serde_json::json!({ "project": project }),
        None => serde_json::json!({}),
    };

    let resp = match agent
        .post(&format!("{base_url}/v1internal:fetchAvailableModels"))
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .set("User-Agent", "antigravity")
        .send_json(&body)
    {
        Ok(resp) => resp,
        Err(error) => {
            let classified = classify_ureq_error(&error);
            diagnose::log_error("Antigravity fetchAvailableModels request failed", error);
            return Err(classified);
        }
    };

    let response: AntigravityModelsResponse = match resp.into_json() {
        Ok(response) => response,
        Err(error) => {
            diagnose::log_error(
                "unable to parse Antigravity fetchAvailableModels response",
                error,
            );
            return Err(PollError::RequestFailed);
        }
    };

    // Gemini models are the primary allowance; other model groups have their
    // own quota and are used only when no Gemini model reports one.
    let models: Vec<(String, UsageSection)> = response
        .models
        .into_iter()
        .filter(|(model, _)| is_antigravity_display_model(model))
        .filter_map(|(model, info)| {
            Some((model, antigravity_section_from_quota(info.quota_info?)?))
        })
        .collect();
    let has_gemini = models.iter().any(|(model, _)| model.starts_with("gemini"));
    let section = best_antigravity_section(
        models
            .into_iter()
            .filter(|(model, _)| !has_gemini || model.starts_with("gemini"))
            .map(|(_, section)| section),
    )
    .ok_or(PollError::RequestFailed)?;
    Ok((section, has_gemini.then(|| "Gemini Models".to_string())))
}

fn fetch_antigravity_quota_summary(
    base_url: &str,
    token: &str,
    project: &str,
) -> Result<UsageData, PollError> {
    let agent = build_agent()?;
    let body = serde_json::json!({ "project": project });

    let resp = match agent
        .post(&format!("{base_url}/v1internal:retrieveUserQuotaSummary"))
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .set("User-Agent", "antigravity")
        .send_json(&body)
    {
        Ok(resp) => resp,
        Err(error) => {
            let classified = classify_ureq_error(&error);
            diagnose::log_error("Antigravity retrieveUserQuotaSummary request failed", error);
            return Err(classified);
        }
    };

    let response: AntigravityQuotaSummaryResponse = match resp.into_json() {
        Ok(response) => response,
        Err(error) => {
            diagnose::log_error(
                "unable to parse Antigravity retrieveUserQuotaSummary response",
                error,
            );
            return Err(PollError::RequestFailed);
        }
    };

    antigravity_usage_from_summary(response).ok_or(PollError::RequestFailed)
}

fn antigravity_section_from_quota(quota: AntigravityQuotaInfo) -> Option<UsageSection> {
    let remaining = quota.remaining_fraction?.clamp(0.0, 1.0);
    Some(UsageSection {
        percentage: (1.0 - remaining) * 100.0,
        resets_at: parse_iso8601(quota.reset_time.as_deref()),
        available: true,
    })
}

fn antigravity_section_from_summary_bucket(
    bucket: &AntigravityQuotaSummaryBucket,
) -> Option<UsageSection> {
    let remaining = bucket.remaining_fraction?.clamp(0.0, 1.0);
    Some(UsageSection {
        percentage: (1.0 - remaining) * 100.0,
        resets_at: parse_iso8601(bucket.reset_time.as_deref()),
        available: true,
    })
}

/// Gemini Models is Antigravity's primary allowance. Another group (for
/// example "Claude and GPT models") is shown only when the response names no
/// Gemini group, and then under its own name. Allowances of different groups
/// are never combined, and a Gemini group without a usable window shows that
/// window as unavailable (`--`) rather than another group's values.
///
/// A group the owner chose (Antigravity quota menu) is shown whenever the
/// response contains it; otherwise the Auto choice applies.
fn antigravity_usage_from_summary(response: AntigravityQuotaSummaryResponse) -> Option<UsageData> {
    let groups = response.groups.unwrap_or_default();
    log_antigravity_summary_shape(&groups);
    remember_antigravity_groups(&groups);
    let preferred = ANTIGRAVITY_QUOTA_GROUP
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    select_antigravity_group(&groups, preferred.as_deref())
}

/// Antigravity's local language server answers `{"response": <summary>}`.
#[derive(Deserialize)]
struct AntigravityLocalSummary {
    response: Option<AntigravityQuotaSummaryResponse>,
}

fn fetch_antigravity_local_usage() -> Option<UsageData> {
    let body = crate::antigravity_local::retrieve_user_quota_summary()?;
    let summary = match serde_json::from_str::<AntigravityLocalSummary>(&body) {
        Ok(summary) => summary.response?,
        Err(error) => {
            diagnose::log_error(
                "unable to parse the Antigravity language server quota summary",
                error,
            );
            return None;
        }
    };
    antigravity_usage_from_summary(summary)
}

/// Keeps every group name seen during this run (first-seen order) so the menu
/// can offer a group even while only Google's partial summary is available.
fn remember_antigravity_groups(groups: &[AntigravityQuotaSummaryGroup]) {
    const MAX_GROUPS: usize = 8;
    let mut known = ANTIGRAVITY_QUOTA_GROUPS
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for label in groups.iter().filter_map(antigravity_group_label) {
        if known.len() < MAX_GROUPS && !known.iter().any(|k| k.eq_ignore_ascii_case(&label)) {
            known.push(label);
        }
    }
}

fn select_antigravity_group(
    groups: &[AntigravityQuotaSummaryGroup],
    preferred: Option<&str>,
) -> Option<UsageData> {
    if let Some(preferred) = preferred {
        let chosen = groups.iter().find(|group| {
            antigravity_group_label(group)
                .is_some_and(|label| label.eq_ignore_ascii_case(preferred))
        });
        if let Some(group) = chosen {
            // A window the group does not have (Gemini 4 Argon has no 5h
            // limit) shows as unavailable.
            let mut data = antigravity_usage_from_summary_group(group).unwrap_or_default();
            data.quota_group = antigravity_group_label(group);
            return Some(data);
        }
    }

    let gemini = groups
        .iter()
        .filter_map(|group| antigravity_gemini_match(group).map(|rank| (rank, group)))
        .min_by_key(|(rank, _)| *rank)
        .map(|(_, group)| group);
    if let Some(group) = gemini {
        let mut data = antigravity_usage_from_summary_group(group).unwrap_or_default();
        data.quota_group =
            Some(antigravity_group_label(group).unwrap_or_else(|| "Gemini Models".to_string()));
        return Some(data);
    }
    groups.iter().find_map(|group| {
        let mut data = antigravity_usage_from_summary_group(group)?;
        data.quota_group = antigravity_group_label(group);
        Some(data)
    })
}

fn antigravity_usage_from_summary_group(group: &AntigravityQuotaSummaryGroup) -> Option<UsageData> {
    let mut data = UsageData::default();
    let mut has_quota = false;

    for bucket in group.buckets.as_deref().unwrap_or_default() {
        let Some(section) = antigravity_section_from_summary_bucket(bucket) else {
            continue;
        };
        match antigravity_bucket_window(bucket) {
            Some(UsageWindowKind::Session) if !data.session.available => {
                data.session = section;
                has_quota = true;
            }
            Some(UsageWindowKind::Weekly) if !data.weekly.available => {
                data.weekly = section;
                has_quota = true;
            }
            _ => {}
        }
    }

    has_quota.then_some(data)
}

/// The bucket's window from its `window` field, or else from its identifier
/// or name ("gemini-5h", "Five Hour Limit", "Weekly Limit").
fn antigravity_bucket_window(bucket: &AntigravityQuotaSummaryBucket) -> Option<UsageWindowKind> {
    fn classify(text: &str) -> Option<UsageWindowKind> {
        let key: String = text
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_ascii_lowercase())
            .collect();
        if key.contains("5h") || key.contains("fivehour") || key.contains("5hour") {
            Some(UsageWindowKind::Session)
        } else if key.contains("week") || key.contains("7d") || key.contains("sevenday") {
            Some(UsageWindowKind::Weekly)
        } else {
            None
        }
    }
    [
        bucket.window.as_deref(),
        bucket.bucket_id.as_deref(),
        bucket.display_name.as_deref(),
    ]
    .into_iter()
    .flatten()
    .find_map(classify)
}

/// How confidently a group is the Gemini group: its name (0), its
/// description (1) or its buckets (2). `None` for other groups.
fn antigravity_gemini_match(group: &AntigravityQuotaSummaryGroup) -> Option<u8> {
    let gemini =
        |text: Option<&str>| text.is_some_and(|t| t.to_ascii_lowercase().contains("gemini"));
    if gemini(group.display_name.as_deref()) {
        return Some(0);
    }
    if group.display_name.is_none() && gemini(group.description.as_deref()) {
        return Some(1);
    }
    let buckets = group.buckets.as_deref().unwrap_or_default();
    (group.display_name.is_none()
        && !buckets.is_empty()
        && buckets.iter().all(|bucket| {
            bucket
                .bucket_id
                .as_deref()
                .is_some_and(|id| id.to_ascii_lowercase().starts_with("gemini"))
                || gemini(bucket.display_name.as_deref())
        }))
    .then_some(2)
}

/// A provider-supplied group name, trimmed to printable text for display.
fn antigravity_group_label(group: &AntigravityQuotaSummaryGroup) -> Option<String> {
    let name: String = group
        .display_name
        .as_deref()?
        .chars()
        .filter(|c| !c.is_control())
        .take(40)
        .collect();
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// Logs the response's group and bucket layout when it changes: names,
/// windows and which fields are present. No project, account or token data.
fn log_antigravity_summary_shape(groups: &[AntigravityQuotaSummaryGroup]) {
    static LAST: Mutex<Option<String>> = Mutex::new(None);
    let shape = groups
        .iter()
        .map(|group| {
            let buckets = group
                .buckets
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|bucket| {
                    format!(
                        "{}/{}(fraction={},reset={})",
                        bucket.bucket_id.as_deref().unwrap_or("-"),
                        bucket.window.as_deref().unwrap_or("-"),
                        bucket.remaining_fraction.is_some(),
                        bucket.reset_time.is_some()
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            format!(
                "'{}'[{buckets}]",
                antigravity_group_label(group).unwrap_or_else(|| "-".to_string())
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if last.as_deref() != Some(shape.as_str()) {
        diagnose::log(format!("Antigravity quota summary groups: {shape}"));
        *last = Some(shape);
    }
}

fn best_antigravity_section<I>(sections: I) -> Option<UsageSection>
where
    I: IntoIterator<Item = UsageSection>,
{
    sections.into_iter().max_by(|a, b| {
        a.percentage
            .partial_cmp(&b.percentage)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.resets_at.cmp(&b.resets_at))
    })
}

fn is_antigravity_display_model(model: &str) -> bool {
    model.starts_with("gemini")
        || model.starts_with("claude")
        || model.starts_with("gpt")
        || model.starts_with("image")
        || model.starts_with("imagen")
}

fn unix_to_system_time(unix_secs: Option<i64>) -> Option<SystemTime> {
    let secs = unix_secs?;
    if secs < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_secs(secs as u64))
}

struct Credentials {
    access_token: String,
    expires_at: Option<i64>,
}

impl Drop for Credentials {
    fn drop(&mut self) {
        crate::claude_desktop::wipe_string(&mut self.access_token);
    }
}

fn read_first_credentials() -> Option<Credentials> {
    if let Some(creds) = read_windows_credentials() {
        return Some(creds);
    }

    for distro in list_wsl_distros() {
        if let Some(creds) = read_wsl_credentials(&distro) {
            return Some(creds);
        }
    }

    None
}

fn read_windows_credentials() -> Option<Credentials> {
    let cred_path = windows_credentials_path()?;
    let content = match std::fs::read_to_string(&cred_path) {
        Ok(content) => content,
        Err(error) => {
            if diagnose::is_enabled() {
                diagnose::log_error(
                    &format!(
                        "unable to read Windows credentials at {}",
                        cred_path.display()
                    ),
                    error,
                );
            }
            return None;
        }
    };
    parse_credentials(&content)
}

fn codex_auth_path() -> Option<PathBuf> {
    if let Some(codex_home) = std::env::var_os("CODEX_HOME").map(PathBuf::from) {
        return Some(codex_home.join("auth.json"));
    }

    Some(dirs::home_dir()?.join(".codex").join("auth.json"))
}

fn codex_credential_watch_signature() -> String {
    let Some(path) = codex_auth_path() else {
        return "codex-auth|path-unavailable".to_string();
    };
    match std::fs::read(path) {
        Ok(content) => {
            let mut hasher = DefaultHasher::new();
            content.hash(&mut hasher);
            format!("codex-auth|present|{}|{}", content.len(), hasher.finish())
        }
        Err(_) => "codex-auth|missing".to_string(),
    }
}

fn read_codex_credentials() -> Option<CodexTokenData> {
    let auth_path = codex_auth_path()?;
    let content = match std::fs::read_to_string(&auth_path) {
        Ok(content) => content,
        Err(error) => {
            diagnose::log_error(
                &format!(
                    "unable to read Codex credentials at {}",
                    auth_path.display()
                ),
                error,
            );
            return None;
        }
    };

    let auth: CodexAuthFile = match serde_json::from_str(&content) {
        Ok(auth) => auth,
        Err(_) => {
            // Serde errors can quote unexpected values from the credential file.
            diagnose::log("unable to parse Codex auth metadata; contents omitted");
            return None;
        }
    };
    let tokens = auth
        .tokens
        .filter(|tokens| !tokens.access_token.is_empty())?;
    diagnose::log_verbose(
        "Codex credential metadata loaded; access-token and account fields omitted",
    );
    if let Some(expiration) = jwt_expiration_unix(&tokens.access_token) {
        diagnose_codex_token_expiry(expiration);
    }
    Some(tokens)
}

fn jwt_expiration_unix(token: &str) -> Option<u64> {
    let payload = token.split('.').nth(1)?;
    let decoded = decode_base64_url(payload)?;
    serde_json::from_slice::<serde_json::Value>(&decoded)
        .ok()?
        .get("exp")?
        .as_u64()
}

fn decode_base64_url(input: &str) -> Option<Vec<u8>> {
    if input.len() % 4 == 1 {
        return None;
    }
    let mut output = Vec::with_capacity(input.len() * 3 / 4);
    let mut accumulator = 0u32;
    let mut bits = 0u8;
    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        } as u32;
        accumulator = (accumulator << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((accumulator >> bits) as u8);
            accumulator &= (1u32 << bits).saturating_sub(1);
        }
    }
    Some(output)
}

fn diagnose_codex_token_expiry(expiration_unix: u64) {
    static LAST_LOGGED_EXPIRATION: std::sync::OnceLock<std::sync::Mutex<Option<u64>>> =
        std::sync::OnceLock::new();
    let last_logged = LAST_LOGGED_EXPIRATION.get_or_init(|| std::sync::Mutex::new(None));
    let Ok(mut last_logged) = last_logged.lock() else {
        return;
    };
    if *last_logged == Some(expiration_unix) {
        return;
    }
    *last_logged = Some(expiration_unix);

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    if expiration_unix >= now {
        let remaining = expiration_unix - now;
        diagnose::log(format!(
            "Codex access token expires in {}h{:02}m",
            remaining / 3600,
            (remaining % 3600) / 60
        ));
    } else {
        let elapsed = now - expiration_unix;
        diagnose::log(format!(
            "Codex access token expired {}h{:02}m ago",
            elapsed / 3600,
            (elapsed % 3600) / 60
        ));
    }
}

fn antigravity_token_expired(expiry: Option<&str>, now: SystemTime) -> bool {
    parse_iso8601(expiry).is_some_and(|expiry| expiry <= now)
}

fn read_antigravity_credentials() -> Option<AntigravityTokenData> {
    let content = read_windows_generic_credential(ANTIGRAVITY_CREDENTIAL_TARGET)?;
    let auth: AntigravityAuthFile = serde_json::from_str(&content).ok()?;
    if auth.token.access_token.is_empty() {
        None
    } else {
        Some(auth.token)
    }
}

fn read_windows_generic_credential(target: &str) -> Option<String> {
    const CRED_TYPE_GENERIC: u32 = 1;

    let mut target_wide: Vec<u16> = target.encode_utf16().chain(std::iter::once(0)).collect();
    let mut credential: *mut CredentialW = std::ptr::null_mut();

    let ok = unsafe {
        CredReadW(
            target_wide.as_mut_ptr(),
            CRED_TYPE_GENERIC,
            0,
            &mut credential,
        )
    };

    if ok == 0 || credential.is_null() {
        diagnose::log(format!(
            "unable to read Windows generic credential target {target}"
        ));
        return None;
    }

    let result = unsafe {
        let cred = &*credential;
        if cred.credential_blob_size == 0 || cred.credential_blob.is_null() {
            CredFree(credential as *mut c_void);
            return None;
        }
        let bytes =
            std::slice::from_raw_parts(cred.credential_blob, cred.credential_blob_size as usize);
        let text = String::from_utf8(bytes.to_vec()).ok();
        CredFree(credential as *mut c_void);
        text
    };

    result
}

fn read_wsl_credentials(distro: &str) -> Option<Credentials> {
    let output = run_with_timeout(
        Command::new("wsl.exe")
            .arg("-d")
            .arg(distro)
            .arg("--")
            .arg("sh")
            .arg("-lc")
            .arg("cat ~/.claude/.credentials.json")
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()),
        Duration::from_secs(5),
    )?;

    if !output.status.success() {
        diagnose::log(format!(
            "WSL credentials probe failed for distro {distro} with status {}",
            output.status
        ));
        return None;
    }

    let content = String::from_utf8(output.stdout).ok()?;
    parse_credentials(&content)
}

fn parse_credentials(content: &str) -> Option<Credentials> {
    let json: serde_json::Value = serde_json::from_str(content).ok()?;

    let oauth = json.get("claudeAiOauth")?;
    let access_token = oauth
        .get("accessToken")
        .and_then(|v| v.as_str())?
        .to_string();
    let expires_at = oauth.get("expiresAt").and_then(|v| v.as_i64());

    Some(Credentials {
        access_token,
        expires_at,
    })
}

fn list_wsl_distros() -> Vec<String> {
    let output = match run_with_timeout(
        Command::new("wsl.exe")
            .args(["-l", "-q"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()),
        Duration::from_secs(5),
    ) {
        Some(output) if output.status.success() => output,
        _ => {
            diagnose::log("unable to enumerate WSL distros");
            return Vec::new();
        }
    };

    let stdout = decode_wsl_text(&output.stdout);
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn decode_wsl_text(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }

    if let Some(decoded) = decode_utf16le(bytes) {
        return decoded;
    }

    String::from_utf8_lossy(bytes).into_owned()
}

fn decode_utf16le(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 2 || bytes.len() % 2 != 0 {
        return None;
    }

    let body = if bytes.starts_with(&[0xFF, 0xFE]) {
        &bytes[2..]
    } else if looks_like_utf16le(bytes) {
        bytes
    } else {
        return None;
    };

    let units: Vec<u16> = body
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect();

    Some(String::from_utf16_lossy(&units))
}

fn looks_like_utf16le(bytes: &[u8]) -> bool {
    let sample_len = bytes.len().min(128);
    let units = sample_len / 2;
    if units == 0 {
        return false;
    }

    let nul_high_bytes = bytes[..sample_len]
        .chunks_exact(2)
        .filter(|chunk| chunk[1] == 0)
        .count();

    nul_high_bytes * 2 >= units
}

fn is_token_expired(expires_at: Option<i64>) -> bool {
    let Some(exp) = expires_at else { return false };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    now >= exp
}

/// Parse an ISO 8601 timestamp string into a SystemTime.
fn parse_iso8601(s: Option<&str>) -> Option<SystemTime> {
    let s = s?.trim();
    // Formats seen: "2026-03-05T08:00:00.321598+00:00", "...Z", and local
    // offsets such as "2026-11-09T10:30:08.9951974-06:00". The offset is
    // applied, not dropped.
    let (date, time) = s.split_once('T')?;
    let (time, offset_secs) = if let Some(time) = time.strip_suffix(['Z', 'z']) {
        (time, 0i64)
    } else if let Some(index) = time.rfind(['+', '-']) {
        let (clock, offset) = time.split_at(index);
        let sign = if offset.starts_with('-') { -1 } else { 1 };
        let (hours, minutes) = offset[1..].split_once(':').unwrap_or((&offset[1..], "0"));
        let hours: i64 = hours.parse().ok()?;
        let minutes: i64 = minutes.parse().ok()?;
        if hours > 23 || minutes > 59 {
            return None;
        }
        (clock, sign * (hours * 3600 + minutes * 60))
    } else {
        (time, 0)
    };
    let local = parse_datetime_to_unix(&format!("{date}T{time}"), "%Y-%m-%dT%H:%M:%S%.f").ok()?;
    let utc = i64::try_from(local).ok()?.checked_sub(offset_secs)?;
    u64::try_from(utc)
        .ok()
        .map(|secs| UNIX_EPOCH + Duration::from_secs(secs))
}

/// Minimal datetime parser — avoids pulling in chrono/time crates.
fn parse_datetime_to_unix(s: &str, _fmt: &str) -> Result<u64, ()> {
    // Extract date and time parts from "YYYY-MM-DDTHH:MM:SS[.frac]"
    let (date_str, time_str) = s.split_once('T').ok_or(())?;
    let date_parts: Vec<&str> = date_str.split('-').collect();
    if date_parts.len() != 3 {
        return Err(());
    }

    let year: u64 = date_parts[0].parse().map_err(|_| ())?;
    let month: u64 = date_parts[1].parse().map_err(|_| ())?;
    let day: u64 = date_parts[2].parse().map_err(|_| ())?;

    // Strip fractional seconds
    let time_base = time_str.split('.').next().unwrap_or(time_str);
    let time_parts: Vec<&str> = time_base.split(':').collect();
    if time_parts.len() != 3 {
        return Err(());
    }

    let hour: u64 = time_parts[0].parse().map_err(|_| ())?;
    let min: u64 = time_parts[1].parse().map_err(|_| ())?;
    let sec: u64 = time_parts[2].parse().map_err(|_| ())?;

    // Days from year (using a simplified calculation for dates after 1970)
    let mut days: u64 = 0;
    for y in 1970..year {
        days += if is_leap(y) { 366 } else { 365 };
    }

    let month_days = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    for m in 1..month {
        days += month_days[m as usize];
        if m == 2 && is_leap(year) {
            days += 1;
        }
    }
    days += day - 1;

    Ok(days * 86400 + hour * 3600 + min * 60 + sec)
}

fn is_leap(y: u64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// Format a usage section for the compact taskbar display.
pub fn format_line(
    section: &UsageSection,
    strings: Strings,
    is_simplified_chinese: bool,
    display_remaining: bool,
    window: UsageWindowKind,
) -> String {
    if is_simplified_chinese {
        return format_simplified_chinese_line(section, display_remaining, window);
    }

    let percentage = if display_remaining {
        remaining_percentage(section.percentage)
    } else {
        section.percentage.clamp(0.0, 100.0)
    };
    let pct = format_percent(percentage);
    let cd = format_countdown(section.resets_at, strings);
    if cd.is_empty() {
        pct
    } else {
        format!("{pct} {cd}")
    }
}

/// Format Codex using the reset timestamp, with a compact label for the
/// initial five-hour session countdown.
pub fn format_codex_line(
    section: &UsageSection,
    strings: Strings,
    is_simplified_chinese: bool,
    display_remaining: bool,
    window: UsageWindowKind,
) -> String {
    // Simplified Chinese displays the localized wall-clock reset time rather
    // than a duration, so keep its existing formatter unchanged.
    if is_simplified_chinese {
        return format_line(
            section,
            strings,
            is_simplified_chinese,
            display_remaining,
            window,
        );
    }

    let percentage = if display_remaining {
        remaining_percentage(section.percentage)
    } else {
        section.percentage.clamp(0.0, 100.0)
    };
    let percentage = format_percent(percentage);
    let countdown = section
        .resets_at
        .map(|reset| match reset.duration_since(SystemTime::now()) {
            Ok(remaining) => format_codex_countdown_from_secs(remaining.as_secs(), strings, window),
            Err(_) => strings.now.to_string(),
        })
        .unwrap_or_default();
    if countdown.is_empty() {
        percentage
    } else {
        format!("{percentage} {countdown}")
    }
}

fn format_simplified_chinese_line(
    section: &UsageSection,
    display_remaining: bool,
    window: UsageWindowKind,
) -> String {
    let percentage = if display_remaining {
        remaining_percentage(section.percentage)
    } else {
        section.percentage.clamp(0.0, 100.0)
    };
    let reset = section
        .resets_at
        .and_then(native_interop::system_time_to_local);
    format_simplified_chinese_values_with_label(
        percentage,
        reset,
        window,
        if display_remaining {
            "剩余"
        } else {
            "已用"
        },
    )
}

#[cfg(test)]
fn format_simplified_chinese_values(
    remaining: f64,
    reset: Option<windows::Win32::Foundation::SYSTEMTIME>,
    window: UsageWindowKind,
) -> String {
    format_simplified_chinese_values_with_label(remaining, reset, window, "剩余")
}

fn format_simplified_chinese_values_with_label(
    percentage: f64,
    reset: Option<windows::Win32::Foundation::SYSTEMTIME>,
    window: UsageWindowKind,
    label: &str,
) -> String {
    let Some(reset) = reset else {
        return format!("{label}{}", format_percent(percentage));
    };
    match window {
        UsageWindowKind::Session => {
            format!(
                "{label}{} {:02}:{:02}重置",
                format_percent(percentage),
                reset.wHour,
                reset.wMinute
            )
        }
        UsageWindowKind::Weekly => {
            format!(
                "{label}{} {:02}/{:02}重置",
                format_percent(percentage),
                reset.wMonth,
                reset.wDay
            )
        }
    }
}

/// Whole percent for display, without rounding away the ends of the scale: a
/// positive value below 1% is `<1%` and a value just short of 100% is `99%`,
/// so `0%` and `100%` only ever mean exactly empty or exactly full.
pub fn format_percent(value: f64) -> String {
    let value = if value.is_finite() {
        value.clamp(0.0, 100.0)
    } else {
        0.0
    };
    if value > 0.0 && value < 1.0 {
        "<1%".to_string()
    } else if value > 99.0 && value < 100.0 {
        "99%".to_string()
    } else {
        format!("{:.0}%", value.round())
    }
}

/// Whether a window has no quota left. Exhaustion is exact: a positive
/// remainder that displays as `<1%` is not exhausted.
pub fn window_exhausted(used_percentage: f64) -> bool {
    remaining_percentage(used_percentage) <= 0.0
}

pub fn remaining_percentage(used_percentage: f64) -> f64 {
    (100.0 - used_percentage).clamp(0.0, 100.0)
}

fn format_countdown(resets_at: Option<SystemTime>, strings: Strings) -> String {
    let reset = match resets_at {
        Some(t) => t,
        None => return String::new(),
    };

    let remaining = match reset.duration_since(SystemTime::now()) {
        Ok(d) => d,
        Err(_) => return strings.now.to_string(),
    };

    format_countdown_from_secs(remaining.as_secs(), strings)
}

/// Calculate how long until the display text would change
pub fn time_until_display_change(resets_at: Option<SystemTime>) -> Option<Duration> {
    let reset = resets_at?;
    let remaining = reset.duration_since(SystemTime::now()).ok()?;
    Some(time_until_display_change_from_secs(remaining.as_secs()))
}

fn format_countdown_from_secs(total_secs: u64, strings: Strings) -> String {
    let total_mins = total_secs / 60;
    let total_hours = total_secs / 3600;
    let total_days = total_secs / 86400;

    if total_days >= 1 {
        let remaining_hours = (total_secs / 3600) % 24;
        if remaining_hours == 0 {
            format!("{total_days}{}", strings.day_suffix)
        } else {
            format!(
                "{total_days}{}{}{}",
                strings.day_suffix, remaining_hours, strings.hour_suffix
            )
        }
    } else if total_hours >= 1 {
        format!(
            "{total_hours}{}{:02}{}",
            strings.hour_suffix,
            total_mins % 60,
            strings.minute_suffix
        )
    } else if total_mins >= 1 {
        format!("{total_mins}{}", strings.minute_suffix)
    } else {
        format!("{total_secs}{}", strings.second_suffix)
    }
}

fn format_codex_countdown_from_secs(
    total_secs: u64,
    strings: Strings,
    window: UsageWindowKind,
) -> String {
    let normal = format_countdown_from_secs(total_secs, strings);
    let four_hours_fifty_nine = format_countdown_from_secs(4 * 3600 + 59 * 60, strings);
    if window == UsageWindowKind::Session && normal == four_hours_fifty_nine {
        format!("5{}", strings.hour_suffix)
    } else {
        normal
    }
}

fn time_until_display_change_from_secs(total_secs: u64) -> Duration {
    let total_mins = total_secs / 60;
    let total_days = total_secs / 86400;

    let current_bucket_start = if total_days >= 1 {
        // Day displays now include whole hours, so the next visible change is
        // the next hour boundary rather than the next day boundary.
        total_secs - (total_secs % 3600)
    } else if total_mins >= 1 {
        total_mins * 60
    } else {
        total_secs
    };

    Duration::from_secs(total_secs.saturating_sub(current_bucket_start) + 1)
}

/// Returns true if either section has reached "now" (reset time has passed).
#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_credentials(token: &str, expires_at: Option<i64>) -> Credentials {
        Credentials {
            access_token: token.into(),
            expires_at,
        }
    }

    mod claude_sources {
        use super::*;
        use crate::claude_desktop::{DesktopToken, Unavailable};
        use crate::claude_local::CliState;
        use crate::provider_health::RejectedCredential;

        const ACCOUNT: &str = "00000000-0000-4000-8000-000000000001";
        const FUTURE: i64 = i64::MAX;

        fn now() -> SystemTime {
            UNIX_EPOCH + Duration::from_secs(1_800_000_000)
        }

        fn cli_state(cache_age: Option<u64>) -> CliState {
            CliState {
                account_uuid: Some(ACCOUNT.into()),
                organization_uuid: None,
                cached_usage: cache_age.map(|age| {
                    (
                        usage_with_session_percent(33.0),
                        now() - Duration::from_secs(age),
                    )
                }),
            }
        }

        fn desktop_token(token: &str, expires: i64) -> DesktopToken {
            DesktopToken {
                access_token: token.into(),
                expires_at_ms: Some(expires),
                account_uuid: Some(ACCOUNT.into()),
                org_uuid: None,
                profile_scope: true,
            }
        }

        struct Run {
            result: Result<UsageData, PollError>,
            tokens: Vec<String>,
            desktop_consulted: bool,
            observation: ClaudeObservation,
        }

        fn run(
            cli: &CliState,
            cli_creds: Option<Credentials>,
            desktop: Option<Result<DesktopToken, Unavailable>>,
            guards: &mut (RejectedCredential, RejectedCredential),
            response: impl Fn(&str) -> Result<UsageData, PollError>,
        ) -> Run {
            CLAUDE_OBSERVATION.with(|cell| cell.take());
            let mut tokens = Vec::new();
            let mut desktop_consulted = false;
            let result = poll_claude_sources(
                cli,
                now(),
                cli_creds,
                || {
                    desktop_consulted = true;
                    desktop
                },
                &mut guards.0,
                &mut guards.1,
                |token| {
                    tokens.push(token.to_string());
                    response(token)
                },
            );
            Run {
                result,
                tokens,
                desktop_consulted,
                observation: CLAUDE_OBSERVATION.with(|cell| cell.take()),
            }
        }

        #[test]
        fn a_cache_fetched_before_a_passed_reset_never_replaces_the_request() {
            // Claude Code cached 16% 30 s ago; the 5h window reset 10 s ago.
            let mut cli = cli_state(Some(30));
            if let Some((data, _)) = cli.cached_usage.as_mut() {
                data.session.percentage = 16.0;
                data.session.resets_at = Some(now() - Duration::from_secs(10));
            }
            let mut guards = Default::default();
            let run = run(
                &cli,
                Some(synthetic_credentials("cli", Some(FUTURE))),
                None,
                &mut guards,
                |_| Ok(usage_with_session_percent(0.0)),
            );
            assert_eq!(run.tokens, vec!["cli".to_string()]);
            assert_eq!(run.result.unwrap().session.percentage, 0.0);
            assert_eq!(run.observation.source, DataSource::Live);
        }

        #[test]
        fn a_fresh_claude_code_cache_answers_without_any_request() {
            let mut guards = Default::default();
            let run = run(
                &cli_state(Some(30)),
                Some(synthetic_credentials("cli", Some(FUTURE))),
                None,
                &mut guards,
                |_| panic!("no request expected"),
            );
            assert_eq!(run.result.unwrap().session.percentage, 33.0);
            assert!(run.tokens.is_empty() && !run.desktop_consulted);
            assert_eq!(run.observation.source, DataSource::ClaudeCodeCache);
            assert_eq!(
                run.observation.observed_at,
                Some(now() - Duration::from_secs(30))
            );
        }

        #[test]
        fn an_older_cache_does_not_replace_the_live_request_but_is_kept_as_fallback() {
            let mut guards = Default::default();
            let run = run(
                &cli_state(Some(600)),
                Some(synthetic_credentials("cli", Some(FUTURE))),
                None,
                &mut guards,
                |_| Ok(usage_with_session_percent(40.0)),
            );
            assert_eq!(run.result.unwrap().session.percentage, 40.0);
            assert_eq!(run.tokens, ["cli"]);
            assert!(!run.desktop_consulted);
            assert_eq!(run.observation.source, DataSource::Live);
            assert!(run.observation.fallback.is_some());
        }

        #[test]
        fn an_expired_cli_sign_in_uses_the_matching_desktop_sign_in_read_only() {
            let mut guards = Default::default();
            let run = run(
                &cli_state(None),
                Some(synthetic_credentials("cli-expired", Some(0))),
                Some(Ok(desktop_token("desktop", FUTURE))),
                &mut guards,
                |_| Ok(usage_with_session_percent(12.0)),
            );
            assert_eq!(run.result.unwrap().session.percentage, 12.0);
            assert_eq!(
                run.tokens,
                ["desktop"],
                "the expired CLI token is never sent"
            );
            assert!(
                run.observation.account_override.is_none(),
                "CLI account known"
            );
        }

        #[test]
        fn without_a_usable_desktop_sign_in_expiry_waits_with_no_request() {
            for desktop in [
                None,
                Some(Err(Unavailable::NotInstalled)),
                Some(Err(Unavailable::DecryptFailed)),
                Some(Err(Unavailable::AccountMismatch)),
                Some(Err(Unavailable::AccountUnparsed)),
                Some(Ok(desktop_token("desktop-expired", 0))),
            ] {
                let mut guards = Default::default();
                let run = run(
                    &cli_state(Some(3_600)),
                    Some(synthetic_credentials("cli-expired", Some(0))),
                    desktop,
                    &mut guards,
                    |_| panic!("no request expected"),
                );
                assert_eq!(run.result.unwrap_err(), PollError::TokenExpired);
                assert!(run.tokens.is_empty());
                assert!(run.observation.fallback.is_some(), "stale cache offered");
            }
            let mut guards = Default::default();
            let missing = run(&cli_state(None), None, None, &mut guards, |_| {
                panic!("no request expected")
            });
            assert_eq!(missing.result.unwrap_err(), PollError::NoCredentials);
        }

        #[test]
        fn rate_limits_and_transport_errors_never_retry_with_the_other_sign_in() {
            for error in [
                PollError::RateLimited,
                PollError::NetworkUnavailable,
                PollError::ServerError,
            ] {
                let mut guards = Default::default();
                let run = run(
                    &cli_state(None),
                    Some(synthetic_credentials("cli", Some(FUTURE))),
                    Some(Ok(desktop_token("desktop", FUTURE))),
                    &mut guards,
                    move |_| Err(error),
                );
                assert_eq!(run.result.unwrap_err(), error);
                assert_eq!(run.tokens, ["cli"], "one request per tick");
                assert!(!run.desktop_consulted);
            }
        }

        #[test]
        fn rejected_tokens_are_never_resent_and_do_not_alternate_between_sign_ins() {
            let mut guards = Default::default();
            let cli = cli_state(None);
            let creds = || Some(synthetic_credentials("cli-rejected", Some(FUTURE)));
            let desktop = || Some(Ok(desktop_token("desktop", FUTURE)));
            let respond = |token: &str| {
                if token == "cli-rejected" {
                    Err(PollError::AuthRequired)
                } else {
                    Ok(usage_with_session_percent(5.0))
                }
            };
            let first = run(&cli, creds(), desktop(), &mut guards, respond);
            assert!(first.result.is_ok());
            assert_eq!(first.tokens, ["cli-rejected", "desktop"]);
            for _ in 0..3 {
                let next = run(&cli, creds(), desktop(), &mut guards, respond);
                assert!(next.result.is_ok());
                assert_eq!(next.tokens, ["desktop"], "rejected CLI token stays parked");
            }
            // Both rejected: no request at all until either credential changes.
            let both = |_: &str| Err(PollError::AuthRequired);
            let rejected = run(
                &cli,
                creds(),
                Some(Ok(desktop_token("desktop-rejected", FUTURE))),
                &mut guards,
                both,
            );
            assert_eq!(rejected.tokens, ["desktop-rejected"]);
            let parked = run(
                &cli,
                creds(),
                Some(Ok(desktop_token("desktop-rejected", FUTURE))),
                &mut guards,
                both,
            );
            assert!(parked.tokens.is_empty());
            assert_eq!(parked.result.unwrap_err(), PollError::AuthRequired);
            let refreshed = run(
                &cli,
                Some(synthetic_credentials("cli-refreshed", Some(FUTURE))),
                None,
                &mut guards,
                |_| Ok(usage_with_session_percent(6.0)),
            );
            assert_eq!(refreshed.tokens, ["cli-refreshed"]);
            assert!(refreshed.result.is_ok());
        }

        #[test]
        fn a_desktop_only_account_is_reported_for_scoping() {
            let mut guards = Default::default();
            let run = run(
                &CliState::default(),
                None,
                Some(Ok(desktop_token("desktop", FUTURE))),
                &mut guards,
                |_| Ok(usage_with_session_percent(1.0)),
            );
            assert!(run.result.is_ok());
            assert_eq!(run.observation.account_override.as_deref(), Some(ACCOUNT));
        }

        #[test]
        fn account_scopes_are_stable_opaque_and_account_specific() {
            let a = account_digest("claude", ACCOUNT);
            assert_eq!(a, account_digest("claude", ACCOUNT));
            assert_ne!(a, account_digest("codex", ACCOUNT));
            assert_ne!(
                a,
                account_digest("claude", "00000000-0000-4000-8000-000000000002")
            );
            assert_eq!(a.len(), 64);
            assert!(!a.contains(ACCOUNT));
        }

        #[test]
        fn no_source_file_contains_a_model_or_token_endpoint() {
            // Monitoring must stay model-free and never rotate OAuth tokens.
            for (name, source) in [
                ("poller.rs", include_str!("poller.rs")),
                ("window.rs", include_str!("window.rs")),
                ("claude_desktop.rs", include_str!("claude_desktop.rs")),
                ("claude_local.rs", include_str!("claude_local.rs")),
                ("codex_mcp.rs", include_str!("codex_mcp.rs")),
            ] {
                for forbidden in [
                    concat!("/v1/", "messages"),
                    concat!("oauth/", "token"),
                    concat!("grant_type", "="),
                    concat!("\"-p\"", ","),
                ] {
                    assert!(!source.contains(forbidden), "{name} contains {forbidden}");
                }
            }
        }
    }

    #[test]
    fn probe_history_reads_claude_request_starts_and_retry_after_deadlines() {
        let log = "\
[1000] pid=1 provider request started provider=claude
[1010] pid=1 provider request started provider=codex
[1050] pid=1 Claude usage endpoint failed http_status=429 retry_after_ms=Some(300500)
[1060] pid=1 Claude usage endpoint failed http_status=429 retry_after_ms=Some(1000)
[1070] pid=1 Claude usage endpoint failed http_status=500 retry_after_ms=None
garbage line provider request started provider=claude
[bad] pid=1 provider request started provider=claude
[1100] pid=2 provider request started provider=claude source=desktop_probe
";
        let history = claude_request_history(log);
        assert_eq!(history.last_request_unix, Some(1100));
        assert_eq!(
            history.retry_deadline_unix,
            Some(1050 + 301),
            "latest deadline wins"
        );
        assert_eq!(claude_request_history(""), ClaudeRequestHistory::default());
    }

    #[test]
    fn desktop_probe_refuses_to_add_to_a_monitor_spacing_or_retry_window() {
        let history = |last: Option<u64>, deadline: Option<u64>| ClaudeRequestHistory {
            last_request_unix: last,
            retry_deadline_unix: deadline,
        };
        // A running monitor still polling Claude would collide with its schedule.
        assert_eq!(
            desktop_probe_gate(true, true, history(None, None), 5_000).unwrap_err(),
            "monitor_running_with_claude_enabled"
        );
        // With Claude disabled (or the monitor exited) only spacing matters.
        assert_eq!(
            desktop_probe_gate(true, false, history(Some(4_950), None), 5_000).unwrap_err(),
            "claude_spacing remaining_s=70"
        );
        assert!(desktop_probe_gate(true, false, history(Some(4_880), None), 5_000).is_ok());
        assert!(desktop_probe_gate(false, true, history(Some(4_000), None), 5_000).is_ok());
        assert!(desktop_probe_gate(false, true, history(None, None), 5_000).is_ok());
        // A server Retry-After deadline is always honoured.
        assert_eq!(
            desktop_probe_gate(false, true, history(Some(1_000), Some(5_600)), 5_000).unwrap_err(),
            "retry_after_active remaining_s=600"
        );
        assert!(desktop_probe_gate(false, true, history(Some(1_000), Some(4_999)), 5_000).is_ok());
    }

    #[test]
    fn provider_poll_rejects_responses_when_the_account_changes_mid_request() {
        let poll = finish_provider_poll(
            0,
            Ok(usage_with_session_percent(64.0)),
            (Some("token".into()), Some("account-a".to_string())),
            (Some("token".into()), Some("account-b".to_string())),
            None,
        );
        assert!(poll.data.is_none());
        assert_eq!(poll.error, Some(PollError::AuthRequired));
    }

    #[test]
    fn claude_rejected_future_token_is_not_retried_until_changed() {
        for status in [401, 403] {
            let mut guard = crate::provider_health::RejectedCredential::default();
            let error = classify_http_status(status);
            let result = fetch_claude_passively(
                synthetic_credentials("old", Some(i64::MAX)),
                &mut guard,
                |_| Err(error),
            );
            assert_eq!(result.err(), Some(PollError::AuthRequired));
            let result = fetch_claude_passively(
                synthetic_credentials("old", Some(i64::MAX)),
                &mut guard,
                |_| panic!("rejected credentials must not make a request"),
            );
            assert_eq!(result.err(), Some(PollError::AuthRequired));
            assert!(fetch_claude_passively(
                synthetic_credentials("new", Some(i64::MAX)),
                &mut guard,
                |_| Ok(UsageData::default())
            )
            .is_ok());
        }
    }

    #[test]
    fn expired_and_transient_claude_paths_never_require_a_model_task() {
        let mut guard = crate::provider_health::RejectedCredential::default();
        assert_eq!(
            fetch_claude_passively(
                synthetic_credentials("expired", Some(0)),
                &mut guard,
                |_| panic!("expired token must not be sent")
            )
            .err(),
            Some(PollError::TokenExpired)
        );
        for error in [
            PollError::NetworkUnavailable,
            PollError::RateLimited,
            PollError::ServerError,
        ] {
            assert_eq!(
                fetch_claude_passively(synthetic_credentials("valid", None), &mut guard, |_| Err(
                    error
                ))
                .err(),
                Some(error)
            );
            assert!(fetch_claude_passively(
                synthetic_credentials("valid", None),
                &mut guard,
                |_| Ok(UsageData::default())
            )
            .is_ok());
        }
        assert!(!run_codex_exec_last_resort());
    }

    #[test]
    fn credential_scopes_are_provider_and_account_specific() {
        assert_ne!(
            credential_scope("claude", "a"),
            credential_scope("claude", "b")
        );
        assert_ne!(
            credential_scope("claude", "a"),
            credential_scope("codex", "a")
        );
    }

    #[test]
    fn reserve_era_response_fields_are_ignored_without_affecting_usage() {
        // Live responses may still carry additional buckets, upsell banners
        // and ordinary-usage flags; none of them may break or change parsing.
        for extra in [
            serde_json::json!({}),
            serde_json::json!({"additional_rate_limits": null}),
            serde_json::json!({
                "ordinary_usage_allowed": false,
                "rate_limit_upsell": {"banner_type": "luna_reserve"},
                "additional_rate_limits": [
                    {"limit_name": "gpt-reserve", "metered_feature": "gpt-reserve",
                     "normal_model_slug": "gpt-5.5",
                     "rate_limit": {"allowed": false, "limit_reached": true,
                        "primary_window": {"used_percent": 100, "reset_at": 2000000200, "limit_window_seconds": 604800}}},
                    {"limit_name": "codex_other", "metered_feature": "codex_other", "rate_limit": null}
                ]
            }),
        ] {
            let mut response = serde_json::json!({
                "plan_type": "pro",
                "rate_limit": {"allowed": false, "limit_reached": true,
                    "primary_window": {"used_percent": 100, "reset_at": 2000000000, "limit_window_seconds": 18000},
                    "secondary_window": {"used_percent": 16, "reset_at": 2000500000, "limit_window_seconds": 604800}},
                "credits": {"has_credits": false, "unlimited": false, "balance": "0"}
            });
            for (key, value) in extra.as_object().unwrap() {
                response[key] = value.clone();
            }
            let data =
                codex_usage_from_response(serde_json::from_value(response).unwrap()).unwrap();
            assert_eq!(data.credits, Some(CreditBalance::Amount(0.0)));
            assert_eq!(data.session.percentage, 100.0);
            assert_eq!(data.weekly.percentage, 16.0);
        }
    }

    #[test]
    fn claude_credentials_path_honors_custom_config_directory() {
        assert_eq!(
            windows_credentials_path_from(
                Some(PathBuf::from(r"D:\claude-config")),
                Some(PathBuf::from(r"C:\Users\Ray")),
            ),
            Some(PathBuf::from(r"D:\claude-config\.credentials.json"))
        );
        assert_eq!(
            windows_credentials_path_from(None, Some(PathBuf::from(r"C:\Users\Ray"))),
            Some(PathBuf::from(r"C:\Users\Ray\.claude\.credentials.json"))
        );
    }

    #[test]
    fn claude_cli_resolution_includes_the_user_local_install_directory() {
        let candidates = claude_user_install_candidates(std::path::Path::new(r"C:\Users\Test"));
        assert_eq!(
            candidates,
            vec![
                PathBuf::from(r"C:\Users\Test\.local\bin\claude.cmd"),
                PathBuf::from(r"C:\Users\Test\.local\bin\claude.exe"),
                PathBuf::from(r"C:\Users\Test\.local\bin\claude"),
            ]
        );
    }

    #[test]
    fn expired_claude_credentials_require_passive_reauthentication() {
        let creds = Credentials {
            access_token: "synthetic".into(),
            expires_at: Some(0),
        };
        assert!(matches!(
            refresh_or_fallback(creds),
            Err(PollError::TokenExpired)
        ));
        assert!(!run_codex_exec_last_resort());
    }

    fn usage_with_session_percent(percentage: f64) -> UsageData {
        UsageData {
            session: UsageSection {
                percentage,
                resets_at: None,
                available: true,
            },
            weekly: UsageSection::default(),
            ..UsageData::default()
        }
    }

    #[test]
    fn remaining_percentage_is_clamped() {
        assert_eq!(remaining_percentage(30.0), 70.0);
        assert_eq!(remaining_percentage(-5.0), 100.0);
        assert_eq!(remaining_percentage(120.0), 0.0);
    }

    #[test]
    fn codex_weekly_only_window_is_not_misreported_as_session_usage() {
        let response: CodexUsageResponse = serde_json::from_str(
            r#"{
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 21,
                        "limit_window_seconds": 604800,
                        "reset_at": 1784500338
                    },
                    "secondary_window": null
                }
            }"#,
        )
        .expect("weekly-only response should deserialize");

        let usage = codex_usage_from_response(response).expect("rate limit should be available");

        assert_eq!(usage.session.percentage, 0.0);
        assert!(usage.session.resets_at.is_none());
        assert_eq!(usage.weekly.percentage, 21.0);
        assert!(usage.weekly.resets_at.is_some());
    }

    #[test]
    fn codex_credits_preserve_decimal_and_zero_balances() {
        let response: CodexUsageResponse = serde_json::from_str(
            r#"{
                "rate_limit": {
                    "primary_window": null,
                    "secondary_window": null
                },
                "credits": {
                    "has_credits": true,
                    "unlimited": false,
                    "balance": "98.5000000000"
                }
            }"#,
        )
        .unwrap();
        let usage = codex_usage_from_response(response).unwrap();
        assert_eq!(usage.credits, Some(CreditBalance::Amount(98.5)));

        let response: CodexUsageResponse = serde_json::from_str(
            r#"{
                "rate_limit": null,
                "credits": {
                    "has_credits": true,
                    "unlimited": false,
                    "balance": "0"
                }
            }"#,
        )
        .unwrap();
        let usage = codex_usage_from_response(response).unwrap();
        assert_eq!(usage.credits, Some(CreditBalance::Amount(0.0)));
    }

    #[test]
    fn codex_credits_keep_missing_and_unlimited_states_distinct() {
        let response: CodexUsageResponse = serde_json::from_str(
            r#"{
                "rate_limit": null,
                "credits": {
                    "has_credits": false,
                    "unlimited": false,
                    "balance": null
                }
            }"#,
        )
        .unwrap();
        let usage = codex_usage_from_response(response);
        assert!(usage.is_none());

        let response: CodexUsageResponse = serde_json::from_str(
            r#"{
                "rate_limit": null,
                "credits": {
                    "has_credits": true,
                    "unlimited": true,
                    "balance": null
                }
            }"#,
        )
        .unwrap();
        let usage = codex_usage_from_response(response).unwrap();
        assert_eq!(usage.credits, Some(CreditBalance::Unlimited));
    }

    #[test]
    fn codex_legacy_windows_keep_positional_mapping_without_durations() {
        let response: CodexUsageResponse = serde_json::from_str(
            r#"{
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 18,
                        "reset_at": 1784500338
                    },
                    "secondary_window": {
                        "used_percent": 33,
                        "reset_at": 1785000000
                    }
                }
            }"#,
        )
        .expect("legacy response should deserialize");

        let usage = codex_usage_from_response(response).expect("rate limit should be available");

        assert_eq!(usage.session.percentage, 18.0);
        assert_eq!(usage.weekly.percentage, 33.0);
    }

    #[test]
    fn classifies_http_failures_for_user_visible_recovery() {
        assert_eq!(classify_http_status(401), PollError::AuthRequired);
        assert_eq!(classify_http_status(403), PollError::AuthRequired);
        assert_eq!(classify_http_status(429), PollError::RateLimited);
        assert_eq!(classify_http_status(500), PollError::ServerError);
        assert_eq!(classify_http_status(503), PollError::ServerError);
        assert_eq!(classify_http_status(404), PollError::RequestFailed);
        assert_eq!(
            PollError::NetworkUnavailable.category(),
            "network_unavailable"
        );
    }

    #[test]
    fn simplified_chinese_line_labels_remaining_usage() {
        let strings = crate::localization::LanguageId::SimplifiedChinese.strings();
        assert_eq!(strings.session_window, "5h");
        assert_eq!(strings.weekly_window, "7d");
        let section = UsageSection {
            percentage: 30.0,
            resets_at: None,
            available: true,
        };
        assert_eq!(
            format_line(&section, strings, true, true, UsageWindowKind::Session),
            "剩余70%"
        );
        let session_reset = windows::Win32::Foundation::SYSTEMTIME {
            wHour: 18,
            wMinute: 30,
            ..Default::default()
        };
        assert_eq!(
            format_simplified_chinese_values(82.0, Some(session_reset), UsageWindowKind::Session,),
            "剩余82% 18:30重置"
        );
        let weekly_reset = windows::Win32::Foundation::SYSTEMTIME {
            wMonth: 7,
            wDay: 17,
            ..Default::default()
        };
        assert_eq!(
            format_simplified_chinese_values(97.0, Some(weekly_reset), UsageWindowKind::Weekly,),
            "剩余97% 07/17重置"
        );
    }

    #[test]
    fn usage_display_mode_complements_percentage_for_remaining() {
        let section = UsageSection {
            percentage: 18.0,
            resets_at: None,
            available: true,
        };
        let strings = crate::localization::LanguageId::English.strings();

        assert_eq!(
            format_line(&section, strings, false, true, UsageWindowKind::Session),
            "82%"
        );
        assert_eq!(
            format_line(&section, strings, false, false, UsageWindowKind::Session),
            "18%"
        );
    }

    #[test]
    fn countdown_format_keeps_hours_and_minutes_compact() {
        let strings = crate::localization::LanguageId::English.strings();
        assert_eq!(
            format_countdown_from_secs(3 * 3600 + 14 * 60, strings),
            "3h14m"
        );
        assert_eq!(format_countdown_from_secs(3600 + 3 * 60, strings), "1h03m");
        assert_eq!(format_countdown_from_secs(47 * 60, strings), "47m");
        assert_eq!(
            format_countdown_from_secs(86400 + 23 * 3600, strings),
            "1d23h"
        );
        assert_eq!(format_countdown_from_secs(2 * 86400, strings), "2d");
    }

    #[test]
    fn hour_minute_countdown_schedules_local_minute_ticks() {
        assert_eq!(
            time_until_display_change_from_secs(4 * 3600 + 59 * 60 + 30),
            Duration::from_secs(31)
        );
        assert_eq!(
            time_until_display_change_from_secs(47 * 60 + 20),
            Duration::from_secs(21)
        );
        assert_eq!(
            time_until_display_change_from_secs(59),
            Duration::from_secs(1)
        );
        assert_eq!(
            time_until_display_change_from_secs(2 * 86400),
            Duration::from_secs(1)
        );
        assert_eq!(
            time_until_display_change_from_secs(2 * 86400 + 4 * 3600 + 12),
            Duration::from_secs(13)
        );
    }

    #[test]
    fn codex_session_uses_reset_time_in_both_display_modes() {
        let section = UsageSection {
            percentage: 0.0,
            resets_at: Some(SystemTime::now() + Duration::from_secs(3 * 3600)),
            available: true,
        };
        let strings = crate::localization::LanguageId::English.strings();
        assert_eq!(
            format_codex_line(&section, strings, false, true, UsageWindowKind::Session),
            "100% 2h59m"
        );
        assert_eq!(
            format_codex_line(&section, strings, false, false, UsageWindowKind::Session),
            "0% 2h59m"
        );
    }

    #[test]
    fn codex_session_compacts_4h59m_but_not_4h58m() {
        let strings = crate::localization::LanguageId::English.strings();
        assert_eq!(
            format_codex_countdown_from_secs(4 * 3600 + 59 * 60, strings, UsageWindowKind::Session),
            "5h"
        );
        assert_eq!(
            format_codex_countdown_from_secs(4 * 3600 + 58 * 60, strings, UsageWindowKind::Session),
            "4h58m"
        );
    }

    #[test]
    fn zero_usage_with_active_reset_is_not_pinned_to_five_hours() {
        let section = UsageSection {
            percentage: 0.0,
            resets_at: Some(SystemTime::now() + Duration::from_secs(3 * 3600)),
            available: true,
        };
        let line = format_codex_line(
            &section,
            crate::localization::LanguageId::English.strings(),
            false,
            true,
            UsageWindowKind::Session,
        );
        assert!(line.ends_with("2h59m"));
        assert!(!line.ends_with("5h"));
    }

    #[test]
    fn weekly_countdown_keeps_normal_4h59m_formatting() {
        assert_eq!(
            format_codex_countdown_from_secs(
                4 * 3600 + 59 * 60,
                crate::localization::LanguageId::English.strings(),
                UsageWindowKind::Weekly,
            ),
            "4h59m"
        );
    }

    #[test]
    fn simplified_chinese_codex_reset_display_remains_localized() {
        let section = UsageSection {
            percentage: 0.0,
            resets_at: Some(SystemTime::now() + Duration::from_secs(3 * 3600)),
            available: true,
        };
        let line = format_codex_line(
            &section,
            crate::localization::LanguageId::SimplifiedChinese.strings(),
            true,
            true,
            UsageWindowKind::Session,
        );
        assert!(line.starts_with("剩余100% "));
        assert!(line.contains("重置"));
        assert!(!line.contains("5小时"));
    }

    #[test]
    fn session_countdown_uses_reset_time_even_when_rounded_remaining_is_hundred() {
        let section = UsageSection {
            percentage: 0.4,
            resets_at: Some(SystemTime::now() + Duration::from_secs(3 * 3600)),
            available: true,
        };
        let text = format_codex_line(
            &section,
            crate::localization::LanguageId::English.strings(),
            false,
            true,
            UsageWindowKind::Session,
        );
        // 99.6% remaining is not full: 100% is shown only when exactly full.
        assert!(text.starts_with("99% "));
        assert!(text.ends_with("2h59m"));
    }

    #[test]
    fn jwt_expiration_diagnostic_parser_never_needs_to_expose_token_contents() {
        assert_eq!(
            jwt_expiration_unix("header.eyJleHAiOjE3MDAwMDAwMDB9.signature"),
            Some(1_700_000_000)
        );
        assert_eq!(jwt_expiration_unix("not-a-jwt"), None);
        assert_eq!(jwt_expiration_unix("a.!!!!.b"), None);
    }

    #[test]
    fn model_free_refresh_uses_official_app_server_account_read() {
        let initialize = app_server_initialize_request();
        assert_eq!(initialize["method"], "initialize");
        assert_eq!(
            initialize["params"]["clientInfo"]["name"],
            "codex_usage_monitor"
        );
        assert_eq!(
            app_server_initialized_notification()["method"],
            "initialized"
        );

        let refresh = app_server_account_refresh_request();
        assert_eq!(refresh["method"], "account/read");
        assert_eq!(refresh["params"]["refreshToken"], true);
        assert_eq!(refresh["id"], 2);
    }

    #[test]
    fn provider_poll_carries_its_own_data_error_and_scope() {
        let scope = Some("codex-scope".to_string());
        let ok = finish_provider_poll(
            1,
            Ok(usage_with_session_percent(42.0)),
            (scope.clone(), ()),
            (scope.clone(), ()),
            None,
        );
        assert_eq!(ok.data.unwrap().session.percentage, 42.0);
        assert_eq!(ok.error, None);
        assert_eq!(ok.scope, scope);

        let failed = finish_provider_poll(
            0,
            Err(PollError::RequestFailed),
            (None, ()),
            (None, ()),
            None,
        );
        assert!(failed.data.is_none());
        assert_eq!(failed.error, Some(PollError::RequestFailed));
        assert_eq!(failed.scope, None);
    }

    #[test]
    fn provider_poll_rejects_responses_when_credentials_change_mid_request() {
        let poll = finish_provider_poll(
            0,
            Ok(usage_with_session_percent(64.0)),
            (Some("old".into()), ()),
            (Some("new".into()), ()),
            None,
        );
        assert!(poll.data.is_none());
        assert_eq!(poll.error, Some(PollError::AuthRequired));
        assert_eq!(poll.scope.as_deref(), Some("new"));
    }

    #[test]
    fn retry_after_hint_is_kept_only_for_rate_limits() {
        let limited = finish_provider_poll(
            0,
            Err(PollError::RateLimited),
            (Some("a".into()), ()),
            (Some("a".into()), ()),
            Some(471_000),
        );
        assert_eq!(limited.retry_after_ms, Some(471_000));
        let other = finish_provider_poll(
            0,
            Err(PollError::ServerError),
            (Some("a".into()), ()),
            (Some("a".into()), ()),
            Some(471_000),
        );
        assert_eq!(other.retry_after_ms, None);
    }

    #[test]
    fn antigravity_summary_prefers_gemini_group() {
        let response: AntigravityQuotaSummaryResponse = serde_json::from_str(
            r#"{
                "groups": [
                    {
                        "displayName": "Claude and GPT models",
                        "buckets": [
                            {
                                "bucketId": "3p-weekly",
                                "window": "weekly",
                                "resetTime": "2026-06-20T18:32:02Z",
                                "remainingFraction": 1
                            },
                            {
                                "bucketId": "3p-5h",
                                "window": "5h",
                                "resetTime": "2026-06-13T23:32:02Z",
                                "remainingFraction": 1
                            }
                        ]
                    },
                    {
                        "displayName": "Gemini Models",
                        "description": "Models within this group: Gemini Flash, Gemini Pro",
                        "buckets": [
                            {
                                "bucketId": "gemini-weekly",
                                "displayName": "Weekly Limit",
                                "window": "weekly",
                                "resetTime": "2026-06-20T17:08:54Z",
                                "remainingFraction": 0.99304295
                            },
                            {
                                "bucketId": "gemini-5h",
                                "displayName": "Five Hour Limit",
                                "window": "5h",
                                "resetTime": "2026-06-13T22:08:54Z",
                                "remainingFraction": 0.9582575
                            }
                        ]
                    }
                ]
            }"#,
        )
        .expect("summary response should deserialize");

        let usage =
            antigravity_usage_from_summary(response).expect("Gemini quota should be selected");

        assert!((usage.weekly.percentage - 0.695705).abs() < 0.000001);
        assert!((usage.session.percentage - 4.17425).abs() < 0.000001);
        assert!(usage.weekly.resets_at.is_some());
        assert!(usage.session.resets_at.is_some());
        assert_eq!(usage.quota_group.as_deref(), Some("Gemini Models"));
    }

    fn summary(json: &str) -> AntigravityQuotaSummaryResponse {
        serde_json::from_str(json).expect("summary response should deserialize")
    }

    /// The field case: Antigravity's own panel showed Gemini Models at 100%
    /// and Claude and GPT models at 97%/98%; the widget showed the latter.
    const OTHER_GROUP_FIRST: &str = r#"{"groups": [
        {"displayName": "Claude and GPT models", "buckets": [
            {"bucketId": "3p-5h", "window": "5h", "resetTime": "2026-06-13T23:32:02Z", "remainingFraction": 0.97},
            {"bucketId": "3p-weekly", "window": "weekly", "resetTime": "2026-06-20T18:32:02Z", "remainingFraction": 0.98}]},
        {"displayName": "Gemini Models", "buckets": [
            {"bucketId": "gemini-5h", "window": "5h", "resetTime": "2026-06-13T22:08:54Z", "remainingFraction": 1},
            {"bucketId": "gemini-weekly", "window": "weekly", "resetTime": "2026-06-20T17:08:54Z", "remainingFraction": 1}]}]}"#;

    #[test]
    fn antigravity_selects_gemini_whatever_the_group_order() {
        let usage = antigravity_usage_from_summary(summary(OTHER_GROUP_FIRST)).unwrap();
        assert_eq!(remaining_percentage(usage.session.percentage), 100.0);
        assert_eq!(remaining_percentage(usage.weekly.percentage), 100.0);
        assert_eq!(usage.quota_group.as_deref(), Some("Gemini Models"));
    }

    #[test]
    fn a_gemini_group_without_usable_windows_is_unavailable_not_replaced() {
        // Gemini reported without fractions: never borrow the other group's values.
        let usage = antigravity_usage_from_summary(summary(
            r#"{"groups": [
                {"displayName": "Claude and GPT models", "buckets": [
                    {"bucketId": "3p-5h", "window": "5h", "remainingFraction": 0.97}]},
                {"displayName": "Gemini Models", "buckets": [
                    {"bucketId": "gemini-5h", "window": "5h"},
                    {"bucketId": "gemini-weekly", "window": "weekly"}]}]}"#,
        ))
        .unwrap();
        assert!(!usage.session.available && !usage.weekly.available);
        assert_eq!(usage.quota_group.as_deref(), Some("Gemini Models"));

        let empty = antigravity_usage_from_summary(summary(
            r#"{"groups": [{"displayName": "Gemini Models"}]}"#,
        ))
        .unwrap();
        assert!(!empty.session.available && !empty.weekly.available);
    }

    #[test]
    fn without_a_gemini_group_the_other_group_is_shown_under_its_own_name() {
        let usage = antigravity_usage_from_summary(summary(
            r#"{"groups": [
                {"displayName": "Claude and GPT models", "buckets": [
                    {"bucketId": "3p-5h", "window": "5h", "remainingFraction": 0.97}]}]}"#,
        ))
        .unwrap();
        assert!((remaining_percentage(usage.session.percentage) - 97.0).abs() < 1e-9);
        assert!(!usage.weekly.available);
        assert_eq!(usage.quota_group.as_deref(), Some("Claude and GPT models"));
        assert!(antigravity_usage_from_summary(summary(r#"{"groups": []}"#)).is_none());
        assert!(antigravity_usage_from_summary(summary(r#"{}"#)).is_none());
    }

    #[test]
    fn antigravity_windows_and_gemini_groups_use_alternative_identifiers() {
        // No displayName; the group is Gemini by its bucket identifiers, and
        // windows come from identifiers or names when `window` is absent.
        let usage = antigravity_usage_from_summary(summary(
            r#"{"groups": [
                {"displayName": "Claude and GPT models", "description": "Not Gemini", "buckets": [
                    {"bucketId": "3p-5h", "window": "FIVE_HOUR", "remainingFraction": 0.5}]},
                {"buckets": [
                    {"bucketId": "gemini-5h", "remainingFraction": 0.75},
                    {"bucketId": "gemini-weekly-limit", "displayName": "Weekly Limit", "remainingFraction": 0.9}]}]}"#,
        ))
        .unwrap();
        assert!((usage.session.percentage - 25.0).abs() < 1e-9);
        assert!((usage.weekly.percentage - 10.0).abs() < 1e-9);
        assert_eq!(usage.quota_group.as_deref(), Some("Gemini Models"));
        // A name match outranks a description that merely mentions Gemini.
        let named = antigravity_usage_from_summary(summary(
            r#"{"groups": [
                {"description": "Gemini models excluded", "buckets": [
                    {"window": "5h", "remainingFraction": 0.1}]},
                {"displayName": "Gemini Models", "buckets": [
                    {"window": "5h", "remainingFraction": 0.6}]}]}"#,
        ))
        .unwrap();
        assert!((named.session.percentage - 40.0).abs() < 1e-9);
    }

    #[test]
    fn iso8601_offsets_are_applied() {
        let at = |s: &str| {
            parse_iso8601(Some(s)).map(|t| t.duration_since(UNIX_EPOCH).unwrap().as_secs())
        };
        // 2026-11-09T16:30:08Z
        let utc = 1_794_241_808;
        assert_eq!(at("2026-11-09T16:30:08Z"), Some(utc));
        assert_eq!(at("2026-11-09T16:30:08.5+00:00"), Some(utc));
        assert_eq!(at("2026-11-09T10:30:08.9951974-06:00"), Some(utc));
        assert_eq!(at("2026-11-09T22:00:08+05:30"), Some(utc));
        assert_eq!(at("2026-11-09T16:30:08"), Some(utc));
        assert_eq!(at("not a time"), None);
        assert_eq!(at("2026-11-09T16:30:08+99:00"), None);
    }

    #[test]
    fn an_expired_antigravity_token_is_not_sent() {
        let now = UNIX_EPOCH + Duration::from_secs(1_794_241_808);
        assert!(antigravity_token_expired(
            Some("2026-11-09T10:30:08-06:00"),
            now
        ));
        assert!(!antigravity_token_expired(
            Some("2026-11-09T11:30:08-06:00"),
            now
        ));
        // Unknown expiry: let the server decide.
        assert!(!antigravity_token_expired(None, now));
        assert!(!antigravity_token_expired(Some("garbage"), now));
    }

    #[test]
    fn percent_text_keeps_empty_and_full_exact() {
        for (value, text) in [
            (0.0, "0%"),
            (0.0001, "<1%"),
            (0.5, "<1%"),
            (0.99, "<1%"),
            (1.0, "1%"),
            (1.4, "1%"),
            (50.5, "51%"),
            (98.6, "99%"),
            (99.01, "99%"),
            (99.5, "99%"),
            (99.99, "99%"),
            (100.0, "100%"),
            (-3.0, "0%"),
            (130.0, "100%"),
            (f64::NAN, "0%"),
        ] {
            assert_eq!(format_percent(value), text, "{value}");
        }
        // Used 99.5% leaves 0.5%: shown as <1% remaining, not exhausted.
        assert_eq!(format_percent(remaining_percentage(99.5)), "<1%");
        assert!(!window_exhausted(99.5));
        assert!(!window_exhausted(99.99));
        assert!(window_exhausted(100.0));
        assert!(window_exhausted(120.0));
    }

    /// The local language server response shape (2026-11-12, descriptions
    /// trimmed): every group with its own windows, as Antigravity's panel.
    const LOCAL_SUMMARY: &str = r#"{"response":{"groups":[
        {"displayName":"Gemini Models","description":"Models within this group: Gemini Flash, Gemini Pro","buckets":[
            {"bucketId":"gemini-weekly","displayName":"Weekly Limit Remaining","window":"weekly","remainingFraction":0.98649085,"resetTime":"2026-11-18T11:52:55Z"},
            {"bucketId":"gemini-5h","displayName":"Five Hour Limit Remaining","window":"5h","remainingFraction":0.9987804,"resetTime":"2026-11-12T17:44:57Z"}]},
        {"displayName":"Claude and GPT models","buckets":[
            {"bucketId":"3p-weekly","window":"weekly","remainingFraction":0.9609568,"resetTime":"2026-11-13T03:35:17Z"},
            {"bucketId":"3p-5h","window":"5h","remainingFraction":0.9545424,"resetTime":"2026-11-12T17:44:33Z"}]},
        {"displayName":"Gemini 4 Argon","buckets":[
            {"bucketId":"gemini-argon-wtu-weekly","displayName":"Gemini 4 Argon Weekly Limit","window":"weekly","remainingFraction":0.8438432,"resetTime":"2026-11-18T11:51:57Z"}]}],
        "description":"Within each group, models share a weekly limit and a 5-hour limit."}}"#;

    fn local_groups() -> Vec<AntigravityQuotaSummaryGroup> {
        let local: AntigravityLocalSummary =
            serde_json::from_str(LOCAL_SUMMARY).expect("local summary should deserialize");
        local.response.unwrap().groups.unwrap()
    }

    #[test]
    fn auto_shows_gemini_models_from_the_local_summary() {
        let data = select_antigravity_group(&local_groups(), None).unwrap();
        assert_eq!(data.quota_group.as_deref(), Some("Gemini Models"));
        assert!((data.session.percentage - 0.12196).abs() < 1e-3);
        assert!((data.weekly.percentage - 1.350915).abs() < 1e-3);
        assert_eq!(
            data.weekly.resets_at,
            parse_iso8601(Some("2026-11-18T11:52:55Z"))
        );
        // A server that is not signed in answers without a summary.
        assert!(serde_json::from_str::<AntigravityLocalSummary>("{}")
            .unwrap()
            .response
            .is_none());
    }

    #[test]
    fn a_chosen_antigravity_group_is_shown_with_only_the_windows_it_has() {
        let groups = local_groups();
        let claude = select_antigravity_group(&groups, Some("claude and gpt models")).unwrap();
        assert_eq!(claude.quota_group.as_deref(), Some("Claude and GPT models"));
        assert!((claude.session.percentage - 4.54576).abs() < 1e-3);
        assert!((claude.weekly.percentage - 3.90432).abs() < 1e-3);

        let argon = select_antigravity_group(&groups, Some("Gemini 4 Argon")).unwrap();
        assert_eq!(argon.quota_group.as_deref(), Some("Gemini 4 Argon"));
        assert!(!argon.session.available, "Argon has no 5h limit");
        assert!(argon.weekly.available);
        assert!((argon.weekly.percentage - 15.61568).abs() < 1e-3);

        // A chosen group missing from the response (Google's partial
        // summary) falls back to Auto, labelled with the group shown.
        let partial: Vec<_> = groups.into_iter().skip(1).take(1).collect();
        let fallback = select_antigravity_group(&partial, Some("Gemini 4 Argon")).unwrap();
        assert_eq!(
            fallback.quota_group.as_deref(),
            Some("Claude and GPT models")
        );
    }
}
