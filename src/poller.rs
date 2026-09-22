use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::ffi::c_void;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use std::os::windows::process::CommandExt;

use crate::diagnose;
use crate::localization::Strings;
use crate::models::{AppUsageData, CreditBalance, LunaReserveUsage, UsageData, UsageSection};
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
const CLAUDE_EXEC_REFRESH_COOLDOWN_SECS: u64 = 24 * 60 * 60;
const CLAUDE_PASSIVE_RECOVERY_POLLS: u8 = 3;

static LAST_CLAUDE_EXEC_REFRESH_UNIX: AtomicU64 = AtomicU64::new(0);
static CLAUDE_PASSIVE_RECOVERY_FAILURES: AtomicU8 = AtomicU8::new(0);

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

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

pub fn is_transient_error(error: PollError) -> bool {
    matches!(
        error,
        PollError::NetworkUnavailable | PollError::RateLimited | PollError::ServerError
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialWatchMode {
    ActiveSource,
    AllSources,
    Codex,
    Antigravity,
}

pub type CredentialWatchSnapshot = Vec<String>;

#[derive(Default)]
pub struct PollOutcome {
    pub data: AppUsageData,
    pub claude_error: Option<PollError>,
    pub codex_error: Option<PollError>,
    pub antigravity_error: Option<PollError>,
    pub first_error: Option<PollError>,
    pub has_success: bool,
}

#[derive(Deserialize)]
struct UsageResponse {
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
    #[serde(default)]
    ordinary_usage_allowed: Option<bool>,
    credits: Option<CodexCredits>,
    #[serde(default)]
    additional_rate_limits: Option<Vec<CodexAdditionalRateLimit>>,
    #[serde(default)]
    rate_limit_upsell: Option<CodexRateLimitUpsell>,
}

#[derive(Deserialize)]
struct CodexRateLimitUpsell {
    banner_type: Option<String>,
}

#[derive(Deserialize)]
struct CodexAdditionalRateLimit {
    limit_name: Option<String>,
    metered_feature: Option<String>,
    rate_limit: Option<Option<Box<CodexRateLimitDetails>>>,
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
    #[serde(default)]
    allowed: Option<bool>,
    #[serde(default)]
    limit_reached: Option<bool>,
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

pub fn poll(show_claude_code: bool, show_codex: bool, show_antigravity: bool) -> PollOutcome {
    diagnose::log(format!(
        "usage poll started providers=claude:{show_claude_code},codex:{show_codex},antigravity:{show_antigravity}"
    ));
    poll_with(
        show_claude_code,
        show_codex,
        show_antigravity,
        poll_claude_code,
        poll_codex,
        poll_antigravity,
    )
}

/// Whether Claude Code CLI credentials are available from a supported source.
/// Claude Desktop authentication is intentionally not treated as CLI access.
pub fn claude_code_credentials_available() -> bool {
    read_first_credentials().is_some()
}

fn poll_with(
    show_claude_code: bool,
    show_codex: bool,
    show_antigravity: bool,
    mut poll_claude_code: impl FnMut() -> Result<UsageData, PollError>,
    mut poll_codex: impl FnMut() -> Result<UsageData, PollError>,
    mut poll_antigravity: impl FnMut() -> Result<UsageData, PollError>,
) -> PollOutcome {
    let mut data = AppUsageData::default();
    let mut first_error = None;
    let mut claude_error = None;
    let mut codex_error = None;
    let mut antigravity_error = None;
    let active_provider_count = show_claude_code as u8 + show_codex as u8 + show_antigravity as u8;

    if show_claude_code {
        match poll_claude_code() {
            Ok(claude_code) => data.claude_code = Some(claude_code),
            Err(error) => {
                claude_error = Some(error);
                if active_provider_count > 1 {
                    diagnose::log(format!("Claude Code usage poll failed: {error:?}"));
                }
                first_error.get_or_insert(error);
            }
        }
    }

    if show_codex {
        match poll_codex() {
            Ok(codex) => data.codex = Some(codex),
            Err(error) => {
                codex_error = Some(error);
                if active_provider_count > 1 {
                    diagnose::log(format!("Codex usage poll failed: {error:?}"));
                }
                first_error.get_or_insert(error);
            }
        }
    }

    if show_antigravity {
        match poll_antigravity() {
            Ok(antigravity) => data.antigravity = Some(antigravity),
            Err(error) => {
                antigravity_error = Some(error);
                if active_provider_count > 1 {
                    diagnose::log(format!("Antigravity usage poll failed: {error:?}"));
                }
                first_error.get_or_insert(error);
            }
        }
    }

    let has_success =
        data.claude_code.is_some() || data.codex.is_some() || data.antigravity.is_some();
    if let Some(codex) = data.codex.as_ref() {
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
        if let Some(reserve) = codex.luna_reserve.as_ref() {
            let reset = reserve
                .section
                .resets_at
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs());
            diagnose::log(format!(
                "Codex Luna Reserve available={} active={:?} used={:.2}% remaining={:.2}% reset_unix={reset:?}",
                reserve.available,
                reserve.active,
                reserve.section.percentage,
                remaining_percentage(reserve.section.percentage),
            ));
        }
    }
    diagnose::log(format!(
        "usage poll completed status={} codex_error={:?}",
        if has_success {
            "partial_or_ok"
        } else {
            "failed"
        },
        codex_error
    ));
    PollOutcome {
        data,
        claude_error,
        codex_error,
        antigravity_error,
        first_error,
        has_success,
    }
}

fn poll_claude_code() -> Result<UsageData, PollError> {
    let creds = match read_first_credentials() {
        Some(c) => c,
        None => {
            diagnose::log(
                "Claude Code CLI credentials missing; Claude Desktop sign-in is not used for CLI monitoring",
            );
            return Err(PollError::NoCredentials);
        }
    };

    let creds = refresh_or_fallback(creds)?;

    fetch_usage_with_fallback(&creds.access_token)
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
    let creds = match read_antigravity_credentials() {
        Some(creds) => creds,
        None => {
            diagnose::log("Antigravity usage poll failed: no Antigravity credentials found");
            return Err(PollError::NoCredentials);
        }
    };

    fetch_antigravity_usage(&creds.access_token)
}

fn refresh_or_fallback(mut creds: Credentials) -> Result<Credentials, PollError> {
    let mut passive_failure = None;
    loop {
        if !is_token_expired(creds.expires_at) {
            CLAUDE_PASSIVE_RECOVERY_FAILURES.store(0, Ordering::Release);
            return Ok(creds);
        }

        let source = creds.source.clone();
        if passive_failure.is_none() {
            passive_failure = Some(
                CLAUDE_PASSIVE_RECOVERY_FAILURES
                    .fetch_add(1, Ordering::AcqRel)
                    .saturating_add(1),
            );
        }
        let passive_failure_count = passive_failure.unwrap_or(CLAUDE_PASSIVE_RECOVERY_POLLS);
        if claude_passive_recovery_should_defer(passive_failure_count) {
            diagnose::log(
                format!(
                    "Claude credentials are expired; passive auth recovery poll {passive_failure_count}/{CLAUDE_PASSIVE_RECOVERY_POLLS}, no model task started"
                ),
            );
        } else if claude_exec_refresh_allowed(now_unix_secs()) {
            diagnose::log(
                "Claude passive auth recovery exhausted; starting guarded last-resort model refresh",
            );
            cli_refresh_token(&source);
        } else {
            diagnose::log(
                "Claude credentials are expired; last-resort model refresh cooldown is active",
            );
        }

        match read_credentials_from_source(&source) {
            Some(refreshed) if !is_token_expired(refreshed.expires_at) => {
                CLAUDE_PASSIVE_RECOVERY_FAILURES.store(0, Ordering::Release);
                return Ok(refreshed);
            }
            Some(_) => diagnose::log(format!(
                "credentials from {source:?} still expired after refresh attempt"
            )),
            None => diagnose::log(format!(
                "credentials from {source:?} unavailable after refresh attempt"
            )),
        }

        if claude_passive_recovery_should_defer(passive_failure_count) {
            return Err(PollError::TokenExpired);
        }

        match read_next_credentials_after(&source) {
            Some(next) => {
                creds = next;
                passive_failure = Some(passive_failure_count);
            }
            None => return Err(PollError::TokenExpired),
        }
    }
}

fn claude_passive_recovery_should_defer(failures: u8) -> bool {
    failures < CLAUDE_PASSIVE_RECOVERY_POLLS
}

fn claude_exec_refresh_allowed(now: u64) -> bool {
    let previous = LAST_CLAUDE_EXEC_REFRESH_UNIX.load(Ordering::Acquire);
    if previous != 0 && now.saturating_sub(previous) < CLAUDE_EXEC_REFRESH_COOLDOWN_SECS {
        return false;
    }
    LAST_CLAUDE_EXEC_REFRESH_UNIX
        .compare_exchange(previous, now, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

/// Invoke the Claude CLI with a minimal prompt to force its internal
/// OAuth token refresh.
fn cli_refresh_token(source: &CredentialSource) {
    match source {
        CredentialSource::Windows(_) => cli_refresh_windows_token(),
        CredentialSource::Wsl { distro } => cli_refresh_wsl_token(distro),
    }
}

fn cli_refresh_windows_token() {
    let claude_path = resolve_windows_claude_path();
    let is_cmd = claude_path.to_lowercase().ends_with(".cmd");
    diagnose::log(format!(
        "attempting Windows Claude token refresh via {claude_path}"
    ));

    let args: &[&str] = &["-p", "."];

    let mut cmd = if is_cmd {
        let mut c = Command::new("cmd.exe");
        c.arg("/c").arg(&claude_path).args(args);
        c
    } else {
        let mut c = Command::new(&claude_path);
        c.args(args);
        c
    };
    cmd.env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(error) => {
            diagnose::log_error("unable to spawn Windows Claude token refresh", error);
            return;
        }
    };

    // Wait up to 30 seconds — don't block the poll thread forever
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > Duration::from_secs(30) {
                    let _ = child.kill();
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(_) => break,
        }
    }
}

fn cli_refresh_wsl_token(distro: &str) {
    diagnose::log(format!(
        "attempting WSL Claude token refresh in distro {distro}"
    ));
    let mut cmd = Command::new("wsl.exe");
    cmd.arg("-d")
        .arg(distro)
        .arg("--")
        .arg("bash")
        .arg("-lic")
        .arg("if command -v claude >/dev/null 2>&1; then claude -p .; elif [ -x \"$HOME/.local/bin/claude\" ]; then \"$HOME/.local/bin/claude\" -p .; else exit 127; fi")
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(error) => {
            diagnose::log_error("unable to spawn WSL Claude token refresh", error);
            return;
        }
    };

    wait_for_refresh(&mut child);
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
    let codex_path = resolve_windows_codex_path();
    diagnose::log("starting last-resort Codex exec auth refresh (may use model allowance)");
    let mut command = codex_command(&codex_path, &["exec", "."]);
    command
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            diagnose::log_error("unable to spawn last-resort Codex exec auth refresh", error);
            return false;
        }
    };
    wait_for_refresh(&mut child)
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

fn wait_for_refresh(child: &mut std::process::Child) -> bool {
    // Wait up to 30 seconds; don't block the poll thread forever.
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => {
                if start.elapsed() > Duration::from_secs(30) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(_) => return false,
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
        ureq::Error::Status(status, _) => classify_http_status(*status),
        ureq::Error::Transport(_) => PollError::NetworkUnavailable,
    }
}

pub fn credential_watch_snapshot(mode: CredentialWatchMode) -> CredentialWatchSnapshot {
    if mode == CredentialWatchMode::Antigravity {
        return vec![antigravity_credential_watch_signature()];
    }
    if mode == CredentialWatchMode::Codex {
        return vec![codex_credential_watch_signature()];
    }

    let sources = match mode {
        CredentialWatchMode::ActiveSource => read_first_credentials()
            .map(|creds| vec![creds.source])
            .unwrap_or_else(all_known_credential_sources),
        CredentialWatchMode::AllSources => all_known_credential_sources(),
        CredentialWatchMode::Codex => unreachable!(),
        CredentialWatchMode::Antigravity => unreachable!(),
    };

    let mut snapshot: CredentialWatchSnapshot = sources
        .into_iter()
        .filter_map(|source| credential_watch_signature(&source))
        .collect();
    snapshot.sort();
    snapshot.dedup();
    snapshot
}

fn all_known_credential_sources() -> Vec<CredentialSource> {
    let mut sources = Vec::new();
    if let Some(source) = windows_credential_source() {
        sources.push(source);
    }
    for distro in list_wsl_distros() {
        sources.push(CredentialSource::Wsl { distro });
    }
    sources
}

fn windows_credential_source() -> Option<CredentialSource> {
    Some(CredentialSource::Windows(windows_credentials_path_from(
        std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from),
        dirs::home_dir(),
    )?))
}

fn windows_credentials_path_from(
    config_dir: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    let config_dir = config_dir.filter(|path| !path.as_os_str().is_empty());
    let directory = config_dir.or_else(|| home.map(|path| path.join(".claude")))?;
    Some(directory.join(".credentials.json"))
}

fn credential_watch_signature(source: &CredentialSource) -> Option<String> {
    match source {
        CredentialSource::Windows(path) => Some(windows_credential_watch_signature(path)),
        CredentialSource::Wsl { distro } => wsl_credential_watch_signature(distro),
    }
}

fn windows_credential_watch_signature(path: &PathBuf) -> String {
    let key = format!("win:{}", path.display());
    match std::fs::metadata(path) {
        Ok(metadata) => {
            let modified = metadata
                .modified()
                .ok()
                .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
                .map(|value| value.as_secs())
                .unwrap_or(0);
            format!("{key}|present|{}|{modified}", metadata.len())
        }
        Err(_) => format!("{key}|missing"),
    }
}

fn wsl_credential_watch_signature(distro: &str) -> Option<String> {
    let output = run_with_timeout(
        Command::new("wsl.exe")
            .arg("-d")
            .arg(distro)
            .arg("--")
            .arg("sh")
            .arg("-lc")
            .arg(
                "if [ -f ~/.claude/.credentials.json ]; then \
                 stat -c 'present|%s|%Y' ~/.claude/.credentials.json; \
                 else echo missing; fi",
            )
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()),
        Duration::from_secs(5),
    )?;

    let state = if output.status.success() {
        decode_wsl_text(&output.stdout).trim().to_string()
    } else {
        format!("status-{}", output.status)
    };

    Some(format!("wsl:{distro}|{state}"))
}

fn fetch_usage_with_fallback(token: &str) -> Result<UsageData, PollError> {
    // The dedicated usage endpoint is a passive read. Do not fall back to a
    // Messages request: that request is a real model invocation and can
    // consume the user's Claude allowance.
    try_usage_endpoint(token)
}

fn try_usage_endpoint(token: &str) -> Result<UsageData, PollError> {
    let agent = build_agent()?;

    let resp = match agent
        .get(USAGE_URL)
        .set("Authorization", &format!("Bearer {token}"))
        .set("anthropic-beta", "oauth-2025-04-20")
        .call()
    {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, _)) if code == 401 || code == 403 => {
            diagnose::log(format!(
                "Claude Code usage endpoint returned auth error status {code}; CLI credentials are stale or invalid"
            ));
            return Err(PollError::AuthRequired);
        }
        Err(ureq::Error::Status(code, _)) => return Err(classify_http_status(code)),
        Err(error) => return Err(classify_ureq_error(&error)),
    };

    let response: UsageResponse = match resp.into_json() {
        Ok(response) => response,
        Err(_) => return Err(PollError::RequestFailed),
    };
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
        return Err(PollError::RequestFailed);
    }
    Ok(data)
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
            diagnose::log_error("Codex usage endpoint request failed", error);
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

    let ordinary_allowed = response.ordinary_usage_allowed.or_else(|| {
        response
            .rate_limit
            .as_ref()
            .and_then(|value| value.as_ref())
            .and_then(|details| details.allowed)
    });
    let explicit_reserve_active = response
        .rate_limit_upsell
        .as_ref()
        .and_then(|upsell| upsell.banner_type.as_deref())
        .map(|banner| banner.eq_ignore_ascii_case("luna_reserve"));

    if let Some(reserve) = response.additional_rate_limits.as_ref().and_then(|limits| {
        limits.iter().find(|limit| {
            limit
                .limit_name
                .as_deref()
                .is_some_and(|name| name.eq_ignore_ascii_case("gpt-reserve"))
                || limit
                    .metered_feature
                    .as_deref()
                    .is_some_and(|name| name.eq_ignore_ascii_case("gpt-reserve"))
        })
    }) {
        if let Some(details) = reserve.rate_limit.as_ref().and_then(|value| value.as_ref()) {
            let window = details
                .primary_window
                .as_ref()
                .and_then(|value| value.as_ref())
                .or_else(|| {
                    details
                        .secondary_window
                        .as_ref()
                        .and_then(|value| value.as_ref())
                });
            if let Some(window) = window {
                let available =
                    details.allowed != Some(false) && details.limit_reached != Some(true);
                let active = explicit_reserve_active
                    .map(|is_reserve| is_reserve && ordinary_allowed == Some(false) && available);
                data.luna_reserve = Some(LunaReserveUsage {
                    section: codex_section_from_window(window),
                    available,
                    active,
                });
            }
        }
    }

    let Some(details) = response.rate_limit.flatten().map(|details| *details) else {
        return (data.credits.is_some() || data.luna_reserve.is_some()).then_some(data);
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

fn antigravity_credential_watch_signature() -> String {
    let Some(content) = read_windows_generic_credential(ANTIGRAVITY_CREDENTIAL_TARGET) else {
        return format!("{ANTIGRAVITY_CREDENTIAL_TARGET}|missing");
    };

    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    format!(
        "{ANTIGRAVITY_CREDENTIAL_TARGET}|present|{}|{}",
        content.len(),
        hasher.finish()
    )
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

    let session = fetch_antigravity_model_quota(base_url, token, project.as_deref())?;
    let weekly = UsageSection::default();

    Ok(UsageData {
        session,
        weekly,
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
) -> Result<UsageSection, PollError> {
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

    best_antigravity_section(response.models.into_iter().filter_map(|(model, info)| {
        let quota = info.quota_info?;
        if !is_antigravity_display_model(&model) {
            return None;
        }
        antigravity_section_from_quota(quota)
    }))
    .ok_or(PollError::RequestFailed)
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

fn antigravity_usage_from_summary(response: AntigravityQuotaSummaryResponse) -> Option<UsageData> {
    let mut fallback = None;

    for group in response.groups.unwrap_or_default() {
        let is_gemini = is_antigravity_gemini_summary_group(&group);
        let usage = antigravity_usage_from_summary_group(group);

        if is_gemini && usage.is_some() {
            return usage;
        }

        if fallback.is_none() {
            fallback = usage;
        }
    }

    fallback
}

fn antigravity_usage_from_summary_group(group: AntigravityQuotaSummaryGroup) -> Option<UsageData> {
    let mut data = UsageData::default();
    let mut has_quota = false;

    for bucket in group.buckets.unwrap_or_default() {
        let Some(section) = antigravity_section_from_summary_bucket(&bucket) else {
            continue;
        };

        match bucket.window.as_deref() {
            Some(window) if window.eq_ignore_ascii_case("5h") => {
                data.session = section;
                has_quota = true;
            }
            Some(window) if window.eq_ignore_ascii_case("weekly") => {
                data.weekly = section;
                has_quota = true;
            }
            _ => {}
        }
    }

    has_quota.then_some(data)
}

fn is_antigravity_gemini_summary_group(group: &AntigravityQuotaSummaryGroup) -> bool {
    group
        .display_name
        .as_deref()
        .is_some_and(|name| name.to_ascii_lowercase().contains("gemini"))
        || group
            .description
            .as_deref()
            .is_some_and(|description| description.to_ascii_lowercase().contains("gemini"))
        || group.buckets.as_ref().is_some_and(|buckets| {
            buckets.iter().any(|bucket| {
                bucket
                    .bucket_id
                    .as_deref()
                    .is_some_and(|id| id.to_ascii_lowercase().starts_with("gemini-"))
                    || bucket
                        .display_name
                        .as_deref()
                        .is_some_and(|name| name.to_ascii_lowercase().contains("gemini"))
            })
        })
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
    source: CredentialSource,
}

#[derive(Clone, Debug)]
enum CredentialSource {
    Windows(PathBuf),
    Wsl { distro: String },
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
    let CredentialSource::Windows(cred_path) = windows_credential_source()? else {
        return None;
    };
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
    parse_credentials(&content, CredentialSource::Windows(cred_path))
}

fn read_credentials_from_source(source: &CredentialSource) -> Option<Credentials> {
    match source {
        CredentialSource::Windows(path) => {
            let content = std::fs::read_to_string(path).ok()?;
            parse_credentials(&content, source.clone())
        }
        CredentialSource::Wsl { distro } => read_wsl_credentials(distro),
    }
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
    parse_credentials(
        &content,
        CredentialSource::Wsl {
            distro: distro.to_string(),
        },
    )
}

fn parse_credentials(content: &str, source: CredentialSource) -> Option<Credentials> {
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
        source,
    })
}

fn read_next_credentials_after(source: &CredentialSource) -> Option<Credentials> {
    match source {
        CredentialSource::Windows(_) => {
            for distro in list_wsl_distros() {
                if let Some(creds) = read_wsl_credentials(&distro) {
                    return Some(creds);
                }
            }
        }
        CredentialSource::Wsl { distro } => {
            let mut past_current = false;
            for candidate_distro in list_wsl_distros() {
                if !past_current {
                    past_current = candidate_distro == *distro;
                    continue;
                }
                if let Some(creds) = read_wsl_credentials(&candidate_distro) {
                    return Some(creds);
                }
            }
        }
    }

    None
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
    let s = s?;
    // Strip timezone offset to get "YYYY-MM-DDTHH:MM:SS" or with fractional seconds
    // The API returns formats like "2026-03-05T08:00:00.321598+00:00"
    let datetime_part = s.split('+').next().unwrap_or(s);
    let datetime_part = datetime_part.split('Z').next().unwrap_or(datetime_part);

    // Try parsing with and without fractional seconds
    let formats = ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S"];
    for fmt in &formats {
        if let Ok(secs) = parse_datetime_to_unix(datetime_part, fmt) {
            return Some(UNIX_EPOCH + Duration::from_secs(secs));
        }
    }
    None
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
    let pct = format!("{percentage:.0}%");
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
    let percentage = format!("{percentage:.0}%");
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
        return format!("{label}{percentage:.0}%");
    };
    match window {
        UsageWindowKind::Session => {
            format!(
                "{label}{percentage:.0}% {:02}:{:02}重置",
                reset.wHour, reset.wMinute
            )
        }
        UsageWindowKind::Weekly => {
            format!(
                "{label}{percentage:.0}% {:02}/{:02}重置",
                reset.wMonth, reset.wDay
            )
        }
    }
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
pub fn is_past_reset(data: &UsageData) -> bool {
    let now = SystemTime::now();
    let past = |s: &UsageSection| matches!(s.resets_at, Some(t) if now.duration_since(t).is_ok());
    past(&data.session) || past(&data.weekly)
}

pub fn app_is_past_reset(data: &AppUsageData) -> bool {
    data.claude_code.as_ref().is_some_and(is_past_reset)
        || data.codex.as_ref().is_some_and(is_past_reset)
        || data.antigravity.as_ref().is_some_and(is_past_reset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserve_is_optional_on_every_successful_response() {
        for present in [false, true, false, true] {
            let mut response = serde_json::json!({
                "rate_limit": {"primary_window": {"used_percent": 10, "reset_at": 2000000000, "limit_window_seconds": 18000}},
                "credits": {"balance": 333.704065}
            });
            if present {
                response["additional_rate_limits"] = serde_json::json!([{
                    "limit_name": "gpt-reserve",
                    "rate_limit": {"allowed": true, "primary_window": {"used_percent": 25, "reset_at": 2000000000}}
                }]);
            }
            let data =
                codex_usage_from_response(serde_json::from_value(response).unwrap()).unwrap();
            assert_eq!(data.luna_reserve.is_some(), present);
            assert_eq!(data.credits, Some(CreditBalance::Amount(333.704065)));
            assert_eq!(data.session.percentage, 10.0);
            if let Some(reserve) = data.luna_reserve {
                assert_eq!(reserve.active, None);
                assert_eq!(reserve.section.percentage, 25.0);
            }
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
    fn claude_model_refresh_is_deferred_for_initial_passive_recovery_polls() {
        assert!(claude_passive_recovery_should_defer(1));
        assert!(claude_passive_recovery_should_defer(2));
        assert!(!claude_passive_recovery_should_defer(3));
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
    fn codex_gpt_reserve_is_parsed_without_claiming_activation_unless_explicit() {
        let response: CodexUsageResponse = serde_json::from_str(
            r#"{
                "rate_limit": {
                    "allowed": false,
                    "primary_window": {"used_percent": 100, "limit_window_seconds": 300, "reset_at": 2000000100}
                },
                "rate_limit_upsell": {"banner_type": "luna_reserve"},
                "additional_rate_limits": [{
                    "limit_name": "gpt-reserve",
                    "metered_feature": "base_model_inference",
                    "rate_limit": {
                        "allowed": true,
                        "limit_reached": false,
                        "primary_window": {"used_percent": 25, "limit_window_seconds": 604800, "reset_at": 2000000200}
                    }
                }]
            }"#,
        )
        .unwrap();
        let usage = codex_usage_from_response(response).unwrap();
        let reserve = usage
            .luna_reserve
            .expect("reserve bucket should be retained");
        assert!(reserve.available);
        assert_eq!(reserve.section.percentage, 25.0);
        assert_eq!(reserve.active, Some(true));
    }

    #[test]
    fn codex_reserve_without_activation_evidence_remains_unknown_active_state() {
        let response: CodexUsageResponse = serde_json::from_str(
            r#"{
                "rate_limit": {"allowed": true, "primary_window": {"used_percent": 1, "limit_window_seconds": 300, "reset_at": 2000000100}},
                "additional_rate_limits": [{
                    "limit_name": "gpt-reserve",
                    "rate_limit": {"allowed": true, "primary_window": {"used_percent": 0, "limit_window_seconds": 604800, "reset_at": 2000000200}}
                }]
            }"#,
        )
        .unwrap();
        assert_eq!(
            codex_usage_from_response(response)
                .unwrap()
                .luna_reserve
                .unwrap()
                .active,
            None
        );
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
        assert!(text.starts_with("100% "));
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
    fn claude_failure_does_not_block_codex_when_both_are_enabled() {
        let data = poll_with(
            true,
            true,
            false,
            || Err(PollError::AuthRequired),
            || Ok(usage_with_session_percent(42.0)),
            || unreachable!("antigravity is disabled"),
        );

        assert!(data.has_success);
        assert!(data.data.claude_code.is_none());
        assert_eq!(data.data.codex.unwrap().session.percentage, 42.0);
    }

    #[test]
    fn codex_failure_does_not_block_claude_when_both_are_enabled() {
        let data = poll_with(
            true,
            true,
            false,
            || Ok(usage_with_session_percent(64.0)),
            || Err(PollError::RequestFailed),
            || unreachable!("antigravity is disabled"),
        );

        assert!(data.has_success);
        assert_eq!(data.data.claude_code.unwrap().session.percentage, 64.0);
        assert!(data.data.codex.is_none());
        assert_eq!(data.codex_error, Some(PollError::RequestFailed));
    }

    #[test]
    fn returns_first_error_when_no_enabled_provider_succeeds() {
        let outcome = poll_with(
            true,
            true,
            true,
            || Err(PollError::AuthRequired),
            || Err(PollError::RequestFailed),
            || Err(PollError::NoCredentials),
        );

        assert!(!outcome.has_success);
        assert_eq!(outcome.first_error, Some(PollError::AuthRequired));
        assert_eq!(outcome.codex_error, Some(PollError::RequestFailed));
    }

    #[test]
    fn antigravity_failure_does_not_block_codex_when_both_are_enabled() {
        let data = poll_with(
            false,
            true,
            true,
            || unreachable!("claude code is disabled"),
            || Ok(usage_with_session_percent(42.0)),
            || Err(PollError::NoCredentials),
        );

        assert!(data.has_success);
        assert!(data.data.antigravity.is_none());
        assert_eq!(data.data.codex.unwrap().session.percentage, 42.0);
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
    }
}
