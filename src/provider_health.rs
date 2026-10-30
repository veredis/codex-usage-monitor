//! Small cache/recovery boundary, independent of the shared poll scheduler.
//! Credential-change gating follows upstream provider_poll (PR #8); conservative
//! token-bound cache isolation follows SoKeiKei's PR #30. No scopes are logged.
use std::time::{Duration, SystemTime};

use crate::poller::PollError;

#[derive(Default)]
pub struct ProviderHealth {
    scope: Option<String>,
    pub error: Option<PollError>,
    updated_at: Option<SystemTime>,
}

impl ProviderHealth {
    /// Returns whether an existing snapshot must be discarded. Missing identity
    /// is never evidence that cached data still belongs to the current account.
    pub fn record(
        &mut self,
        scope: Option<String>,
        error: Option<PollError>,
        now: SystemTime,
    ) -> bool {
        let invalidate = scope.is_none() || self.scope != scope;
        if invalidate {
            self.updated_at = None;
        }
        self.scope = scope;
        self.error = error;
        if error.is_none() && self.scope.is_some() {
            self.updated_at = Some(now);
        }
        invalidate
    }

    pub fn stale(&self, now: SystemTime, interval_ms: u32) -> bool {
        self.error.is_some()
            || self.updated_at.is_none_or(|updated| {
                now.duration_since(updated).unwrap_or_default()
                    > Duration::from_millis(u64::from(interval_ms).saturating_mul(2).max(60_000))
            })
    }
}

#[derive(Default)]
pub struct RejectedCredential {
    rejected: Option<String>,
}

impl RejectedCredential {
    pub fn permits(&self, scope: &str) -> bool {
        self.rejected.as_deref() != Some(scope)
    }

    pub fn record(&mut self, scope: String, result: Result<(), PollError>) {
        self.rejected = match result {
            Err(PollError::AuthRequired | PollError::TokenExpired) => Some(scope),
            _ => None,
        };
    }
}

pub fn stale_text(text: &str, stale: bool) -> String {
    if stale && text.contains('%') {
        format!("!{}", text.trim_start_matches('!'))
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejected_unexpired_credentials_wait_for_a_change_and_recover() {
        let mut guard = RejectedCredential::default();
        assert!(guard.permits("a"));
        guard.record("a".into(), Err(PollError::AuthRequired));
        assert!(!guard.permits("a"));
        assert!(guard.permits("b"));
        guard.record("b".into(), Ok(()));
        assert!(guard.permits("b"));
        guard.record("b".into(), Err(PollError::ServerError));
        assert!(guard.permits("b"));
    }
    #[test]
    fn auth_failure_keeps_same_scope_stale_but_change_or_disable_invalidates() {
        let now = SystemTime::now();
        let mut claude = ProviderHealth::default();
        let mut codex = ProviderHealth::default();
        claude.record(Some("a".into()), None, now);
        codex.record(Some("b".into()), None, now);
        assert!(!claude.record(Some("a".into()), Some(PollError::AuthRequired), now));
        assert!(claude.stale(now, 30_000));
        assert!(!codex.stale(now, 30_000));
        assert!(claude.record(Some("new".into()), Some(PollError::AuthRequired), now));
        assert!(claude.record(None, None, now));
        assert!(claude.record(Some("a".into()), Some(PollError::TokenExpired), now));
        claude.record(Some("a".into()), None, now);
        assert!(!claude.stale(now, 30_000));
        assert!(claude.stale(now + Duration::from_secs(61), 30_000));
    }
    #[test]
    fn stale_marker_distinguishes_data_from_unavailable() {
        assert_eq!(stale_text("88% 1h04m", true), "!88% 1h04m");
        assert_eq!(stale_text("88% 1h03m", true), "!88% 1h03m");
        assert_eq!(stale_text("88% 1h03m", false), "88% 1h03m");
        assert_eq!(stale_text("!", true), "!");
    }
}
