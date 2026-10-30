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
    /// Values came from a persisted snapshot and have not been confirmed by
    /// this process yet; they stay stale (`!`) until a successful poll.
    restored: bool,
}

impl ProviderHealth {
    /// Returns whether an existing snapshot must be discarded. Missing identity
    /// is never evidence that cached data still belongs to the current account.
    /// `observed_at` is when a successful result was observed (now, or the
    /// fetch time of a local cache).
    pub fn record(
        &mut self,
        scope: Option<String>,
        error: Option<PollError>,
        observed_at: SystemTime,
    ) -> bool {
        let invalidate = scope.is_none() || self.scope != scope;
        if invalidate {
            self.updated_at = None;
            self.restored = false;
        }
        self.scope = scope;
        self.error = error;
        if error.is_none() && self.scope.is_some() {
            self.restored = false;
            self.updated_at = Some(self.updated_at.map_or(observed_at, |u| u.max(observed_at)));
        }
        invalidate
    }

    /// Adopt a persisted snapshot for `scope` at startup.
    pub fn restore(&mut self, scope: String, updated_at: SystemTime) {
        self.scope = Some(scope);
        self.error = None;
        self.updated_at = Some(updated_at);
        self.restored = true;
    }

    /// Accept an older-than-now observation (Claude Code's cached reading)
    /// while the provider's own request failed. Only a newer observation than
    /// the one shown is accepted; the error, and so the `!` marker, remain.
    pub fn adopt_observation(&mut self, observed_at: SystemTime) -> bool {
        if self.scope.is_none() || self.updated_at.is_some_and(|u| u >= observed_at) {
            return false;
        }
        self.updated_at = Some(observed_at);
        true
    }

    pub fn updated_at(&self) -> Option<SystemTime> {
        self.updated_at
    }

    pub fn stale(&self, now: SystemTime, interval_ms: u32) -> bool {
        self.restored
            || self.error.is_some()
            || self.updated_at.is_none_or(|updated| {
                now.duration_since(updated).unwrap_or_default()
                    > Duration::from_millis(u64::from(interval_ms).saturating_mul(2).max(60_000))
            })
    }
}

/// Whether a window's shown value predates a reset that has since passed.
/// Its old percentage would be false, so it is shown as unknown instead of
/// inventing the post-reset value.
pub fn window_reset_since(
    resets_at: Option<SystemTime>,
    updated_at: Option<SystemTime>,
    now: SystemTime,
) -> bool {
    resets_at.is_some_and(|reset| reset <= now && updated_at.is_none_or(|u| u < reset))
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

    /// Whether a credential is currently parked after a rejection.
    pub fn is_parked(&self) -> bool {
        self.rejected.is_some()
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
    fn restored_snapshots_stay_stale_until_a_live_success_even_when_recent() {
        let now = SystemTime::now();
        let mut claude = ProviderHealth::default();
        claude.restore("account".into(), now - Duration::from_secs(5));
        assert!(
            claude.stale(now, 120_000),
            "restored values are never fresh"
        );
        // An expired sign-in under the same account keeps the snapshot.
        assert!(!claude.record(Some("account".into()), Some(PollError::TokenExpired), now));
        assert!(claude.stale(now, 120_000));
        assert_eq!(claude.updated_at(), Some(now - Duration::from_secs(5)));
        assert!(!claude.record(Some("account".into()), None, now));
        assert!(!claude.stale(now, 120_000));
        // A different account drops it.
        let mut other = ProviderHealth::default();
        other.restore("account".into(), now);
        assert!(other.record(Some("other".into()), Some(PollError::AuthRequired), now));
        assert_eq!(other.updated_at(), None);
    }

    #[test]
    fn older_observations_never_move_freshness_backwards() {
        let now = SystemTime::now();
        let mut claude = ProviderHealth::default();
        claude.record(Some("a".into()), None, now);
        claude.record(Some("a".into()), None, now - Duration::from_secs(30));
        assert_eq!(claude.updated_at(), Some(now));
        claude.record(Some("a".into()), Some(PollError::AuthRequired), now);
        assert!(!claude.adopt_observation(now - Duration::from_secs(1)));
        assert!(claude.adopt_observation(now + Duration::from_secs(1)));
        assert!(
            claude.stale(now + Duration::from_secs(1), 120_000),
            "error keeps !"
        );
        let mut unscoped = ProviderHealth::default();
        assert!(!unscoped.adopt_observation(now));
    }

    #[test]
    fn a_passed_reset_makes_older_values_unknown() {
        let t = |s| SystemTime::UNIX_EPOCH + Duration::from_secs(s);
        assert!(window_reset_since(Some(t(100)), Some(t(50)), t(150)));
        assert!(window_reset_since(Some(t(100)), None, t(150)));
        assert!(
            !window_reset_since(Some(t(100)), Some(t(120)), t(150)),
            "observed after reset"
        );
        assert!(
            !window_reset_since(Some(t(200)), Some(t(50)), t(150)),
            "reset still ahead"
        );
        assert!(!window_reset_since(None, Some(t(50)), t(150)));
    }

    #[test]
    fn stale_marker_distinguishes_data_from_unavailable() {
        assert_eq!(stale_text("88% 1h04m", true), "!88% 1h04m");
        assert_eq!(stale_text("88% 1h03m", true), "!88% 1h03m");
        assert_eq!(stale_text("88% 1h03m", false), "88% 1h03m");
        assert_eq!(stale_text("!", true), "!");
    }
}
