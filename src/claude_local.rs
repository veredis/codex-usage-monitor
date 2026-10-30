//! Read-only view of the Claude Code CLI's own global state (`~/.claude.json`).
//!
//! Two things are used, both verified against Claude Code 2.1.312:
//! - `oauthAccount.accountUuid`: the CLI's signed-in account. It is stable
//!   across token refreshes, so it scopes persisted snapshots and matches the
//!   Claude Desktop sign-in to the same account.
//! - `cachedUsageUtilization`: `{fetchedAtMs, accountUuid, utilization}`, where
//!   `utilization` is the CLI's own copy of the `/api/oauth/usage` response.
//!   The CLI writes it when it fetches usage (`/usage`, status line, startup)
//!   and drops it when the signed-in account changes. It is official
//!   account-wide data, but only as fresh as the CLI's last fetch.
//!
//! Reading it costs no request. The file is never written and its other
//! contents (projects, account details) are never retained or logged.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::models::UsageData;

/// Larger files are ignored rather than read into memory.
const MAX_STATE_BYTES: u64 = 16 * 1024 * 1024;
/// Older cached readings cannot describe either current window.
pub const MAX_CACHE_AGE: Duration = Duration::from_secs(8 * 24 * 60 * 60);

#[derive(Clone, Debug, Default)]
pub struct CliState {
    pub account_uuid: Option<String>,
    /// `oauthAccount.organizationUuid`: the organization the CLI is signed in to.
    pub organization_uuid: Option<String>,
    /// The CLI's cached usage reading and when the CLI fetched it.
    pub cached_usage: Option<(UsageData, SystemTime)>,
}

impl CliState {
    /// The identity a Claude Desktop sign-in must match to be used.
    pub fn desktop_expected(&self) -> crate::claude_desktop::Expected<'_> {
        crate::claude_desktop::Expected {
            account_uuid: self.account_uuid.as_deref(),
            org_uuid: self.organization_uuid.as_deref(),
        }
    }
}

#[derive(Deserialize)]
struct StateFile {
    #[serde(rename = "oauthAccount")]
    oauth_account: Option<OauthAccount>,
    #[serde(rename = "cachedUsageUtilization")]
    cached_usage: Option<CachedUsage>,
}

#[derive(Deserialize)]
struct OauthAccount {
    #[serde(rename = "accountUuid")]
    account_uuid: Option<String>,
    #[serde(rename = "organizationUuid")]
    organization_uuid: Option<String>,
}

#[derive(Deserialize)]
struct CachedUsage {
    #[serde(rename = "fetchedAtMs")]
    fetched_at_ms: Option<f64>,
    #[serde(rename = "accountUuid")]
    account_uuid: Option<String>,
    utilization: Option<crate::poller::UsageResponse>,
}

fn state_path() -> Option<PathBuf> {
    let config_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty());
    if let Some(dir) = config_dir.as_ref() {
        let legacy = dir.join(".config.json");
        if legacy.is_file() {
            return Some(legacy);
        }
    }
    Some(config_dir.or_else(dirs::home_dir)?.join(".claude.json"))
}

/// The CLI's state, re-parsed only when the file changes.
pub fn read() -> CliState {
    static CACHE: OnceLock<Mutex<Option<(PathBuf, SystemTime, u64, CliState)>>> = OnceLock::new();
    let Some(path) = state_path() else {
        return CliState::default();
    };
    let Ok(meta) = std::fs::metadata(&path) else {
        return CliState::default();
    };
    if meta.len() > MAX_STATE_BYTES {
        return CliState::default();
    }
    let stamp = meta.modified().unwrap_or(UNIX_EPOCH);
    let mut cache = CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some((cached_path, cached_stamp, cached_len, state)) = cache.as_ref() {
        if *cached_path == path && *cached_stamp == stamp && *cached_len == meta.len() {
            return state.clone();
        }
    }
    // A torn read during the CLI's write fails to parse and is retried on the
    // next change; nothing stale is kept for it.
    let state = std::fs::read(&path)
        .ok()
        .map(|bytes| parse(&bytes, SystemTime::now()))
        .unwrap_or_default();
    *cache = Some((path, stamp, meta.len(), state.clone()));
    state
}

fn parse(bytes: &[u8], now: SystemTime) -> CliState {
    let Ok(file) = serde_json::from_slice::<StateFile>(bytes) else {
        return CliState::default();
    };
    let (account_uuid, organization_uuid) = file
        .oauth_account
        .map(|account| (account.account_uuid, account.organization_uuid))
        .unwrap_or_default();
    let account_uuid = account_uuid.filter(|uuid| !uuid.is_empty());
    let organization_uuid = organization_uuid.filter(|uuid| !uuid.is_empty());
    let cached_usage = file
        .cached_usage
        .and_then(|cache| validated_cache(cache, account_uuid.as_deref(), now));
    CliState {
        account_uuid,
        organization_uuid,
        cached_usage,
    }
}

/// Mirrors the CLI's own acceptance rule (a reading under another account
/// is ignored) and rejects future, ancient or unparseable readings.
fn validated_cache(
    cache: CachedUsage,
    account_uuid: Option<&str>,
    now: SystemTime,
) -> Option<(UsageData, SystemTime)> {
    if cache.account_uuid.as_deref() != account_uuid {
        return None;
    }
    let ms = cache
        .fetched_at_ms
        .filter(|ms| ms.is_finite() && *ms > 0.0)?;
    let fetched_at = UNIX_EPOCH.checked_add(Duration::from_millis(ms as u64))?;
    let age = now.duration_since(fetched_at).ok()?;
    if age > MAX_CACHE_AGE {
        return None;
    }
    let data = crate::poller::claude_usage_from_response(&cache.utilization?)?;
    Some((data, fetched_at))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ACCOUNT: &str = "00000000-0000-4000-8000-000000000001";

    fn now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_800_000_000)
    }

    fn state(cache_account: Option<&str>, fetched_ms: f64) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "oauthAccount": {"accountUuid": ACCOUNT, "organizationUuid": "22222222-2222-4222-8222-222222222222", "emailAddress": "private@example.invalid"},
            "projects": {"C:/private": {"history": []}},
            "cachedUsageUtilization": {
                "fetchedAtMs": fetched_ms,
                "accountUuid": cache_account,
                "utilization": {
                    "five_hour": {"utilization": 25, "resets_at": "2027-01-15T08:00:00.000000+00:00"},
                    "seven_day": {"utilization": 35.5, "resets_at": "2027-01-17T08:00:00Z"},
                    "seven_day_opus": null,
                    "extra_usage": {"is_enabled": false}
                }
            }
        }))
        .unwrap()
    }

    #[test]
    fn account_matched_cache_parses_official_utilization() {
        let fetched = 1_800_000_000_000.0 - 30_000.0;
        let parsed = parse(&state(Some(ACCOUNT), fetched), now());
        assert_eq!(
            parsed.organization_uuid.as_deref(),
            Some("22222222-2222-4222-8222-222222222222")
        );
        assert_eq!(parsed.account_uuid.as_deref(), Some(ACCOUNT));
        let (data, at) = parsed.cached_usage.unwrap();
        assert_eq!(data.session.percentage, 25.0);
        assert_eq!(data.weekly.percentage, 35.5);
        assert!(data.session.available && data.weekly.available);
        assert!(data.session.resets_at.is_some());
        assert_eq!(now().duration_since(at).unwrap(), Duration::from_secs(30));
    }

    #[test]
    fn foreign_future_ancient_or_corrupt_caches_are_rejected() {
        let recent = 1_800_000_000_000.0 - 1_000.0;
        let other = "00000000-0000-4000-8000-000000000002";
        assert!(parse(&state(Some(other), recent), now())
            .cached_usage
            .is_none());
        assert!(parse(&state(None, recent), now()).cached_usage.is_none());
        let future = 1_800_000_000_000.0 + 60_000.0;
        assert!(parse(&state(Some(ACCOUNT), future), now())
            .cached_usage
            .is_none());
        let ancient = 1_800_000_000_000.0 - 9.0 * 24.0 * 3600.0 * 1000.0;
        assert!(parse(&state(Some(ACCOUNT), ancient), now())
            .cached_usage
            .is_none());
        let full = state(Some(ACCOUNT), recent);
        let truncated = &full[..full.len() / 2];
        let parsed = parse(truncated, now());
        assert!(parsed.account_uuid.is_none() && parsed.cached_usage.is_none());
        let empty_windows = json!({
            "oauthAccount": {"accountUuid": ACCOUNT},
            "cachedUsageUtilization": {"fetchedAtMs": recent, "accountUuid": ACCOUNT,
                "utilization": {"five_hour": null, "seven_day": null}}
        });
        let parsed = parse(&serde_json::to_vec(&empty_windows).unwrap(), now());
        assert!(parsed.cached_usage.is_none());
        assert_eq!(parsed.account_uuid.as_deref(), Some(ACCOUNT));
    }

    #[test]
    fn missing_state_has_no_account_and_no_cache() {
        let parsed = parse(b"{}", now());
        assert!(parsed.account_uuid.is_none() && parsed.cached_usage.is_none());
    }
}
