//! Per-provider request deadlines. The model follows upstream provider_poll
//! (PR #8, feizaodemon): each provider owns its due time, backoff and server
//! Retry-After deadline, so one provider's failures never delay another.
//! Requests themselves run on per-provider workers (see window::spawn_poll).
use std::time::{Duration, Instant};

pub const CLAUDE: usize = 0;
pub const CODEX: usize = 1;
pub const PROVIDERS: usize = 3;

pub const RETRY_BASE_MS: u32 = 30_000;
pub const RETRY_MAX_MS: u32 = 30 * 60 * 1000;
/// Build 6 design decision (revisable): Anthropic's usage endpoint shares its
/// rate limit with every Claude Code status line, so Claude usage requests are
/// spaced at least this far apart whatever the configured cadence.
pub const CLAUDE_MIN_POLL_MS: u32 = 120_000;
/// Longest server Retry-After deadline honored; longer hints are capped.
pub const MAX_SERVER_DELAY_MS: u32 = 24 * 60 * 60 * 1000;
const MIN_TIMER_MS: u32 = 1_000;

/// Cadence for one provider given the configured (or adaptive) cadence.
pub fn provider_interval_ms(id: usize, base_ms: u32) -> u32 {
    if id == CLAUDE {
        base_ms.max(CLAUDE_MIN_POLL_MS)
    } else {
        base_ms
    }
}

/// A regular Claude request due this close before a known reset is moved to
/// just after it. Requests stay at least `CLAUDE_MIN_POLL_MS` apart, so
/// otherwise the first post-reset reading could trail the reset by up to that
/// long while the row shows the old value marked `!`.
pub const RESET_ALIGN_WINDOW_MS: u32 = 60_000;
/// Margin after the reset, so the provider has rolled the window over.
pub const RESET_ALIGN_MARGIN_MS: u32 = 5_000;

/// Delay until the next regular request after a success, given the time
/// until the provider's nearest future reset. Only Claude is aligned (other
/// providers poll often enough); the request count never grows, and the
/// moved request is at most `RESET_ALIGN_WINDOW_MS + RESET_ALIGN_MARGIN_MS`
/// later than planned, inside the stale threshold of twice the cadence.
pub fn reset_aligned_delay_ms(id: usize, delay_ms: u32, until_reset_ms: Option<u64>) -> u32 {
    match until_reset_ms {
        Some(until)
            if id == CLAUDE
                && until > u64::from(delay_ms)
                && until - u64::from(delay_ms) < u64::from(RESET_ALIGN_WINDOW_MS) =>
        {
            u32::try_from(until + u64::from(RESET_ALIGN_MARGIN_MS)).unwrap_or(u32::MAX)
        }
        _ => delay_ms,
    }
}

fn min_request_spacing(id: usize) -> Duration {
    if id == CLAUDE {
        Duration::from_millis(u64::from(CLAUDE_MIN_POLL_MS))
    } else {
        Duration::ZERO
    }
}

/// Exponential backoff that is never more aggressive than the provider cadence.
pub fn transient_retry_delay_ms(retry_count: u32, base_interval_ms: u32) -> u32 {
    let exponent = retry_count.saturating_sub(1).min(31);
    let backoff = RETRY_BASE_MS.saturating_mul(1u32 << exponent);
    backoff
        .max(base_interval_ms)
        .min(RETRY_MAX_MS.max(base_interval_ms))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Success,
    /// Network, rate-limit, server or malformed-response failure: back off and
    /// honor any server deadline.
    Transient {
        server_delay_ms: Option<u32>,
    },
    /// Authentication wait or missing credentials: retry after `delay_ms`
    /// without growing backoff. Rejected credentials are not resent, so no
    /// request spacing is charged.
    Wait {
        delay_ms: u32,
    },
}

/// Which gates a dispatch may bypass. A server Retry-After deadline and an
/// in-flight request are never bypassed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Request {
    pub providers: [bool; PROVIDERS],
    /// Start providers before their scheduled time.
    pub ignore_due: bool,
    /// Also start providers waiting out their own backoff.
    pub ignore_backoff: bool,
    /// Bypass minimum request spacing (explicit user refresh only).
    pub ignore_spacing: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct Slot {
    /// `None` means due immediately.
    due: Option<Instant>,
    retry_count: u32,
    server_not_before: Option<Instant>,
    last_request: Option<Instant>,
    in_flight: bool,
}

impl Slot {
    fn ready_at(&self, id: usize, now: Instant) -> Instant {
        let spaced = self.last_request.map(|last| last + min_request_spacing(id));
        [self.due, self.server_not_before, spaced]
            .into_iter()
            .flatten()
            .fold(now, Instant::max)
    }
}

#[derive(Debug, Default)]
pub struct ProviderSchedule {
    slots: [Slot; PROVIDERS],
}

impl ProviderSchedule {
    /// Marks and returns the providers that may start a request now.
    pub fn begin(&mut self, now: Instant, request: Request) -> [bool; PROVIDERS] {
        std::array::from_fn(|id| {
            let slot = &mut self.slots[id];
            let future = |at: Option<Instant>| at.is_some_and(|at| at > now);
            let start = request.providers[id]
                && !slot.in_flight
                && !future(slot.server_not_before)
                && (request.ignore_due || !future(slot.due))
                && (request.ignore_backoff || slot.retry_count == 0 || !future(slot.due))
                && (request.ignore_spacing
                    || slot
                        .last_request
                        .is_none_or(|last| now.duration_since(last) >= min_request_spacing(id)));
            if start {
                slot.in_flight = true;
                slot.last_request = Some(now);
            }
            start
        })
    }

    /// Records a completed request and schedules the provider's next one.
    pub fn finish(&mut self, id: usize, now: Instant, outcome: Outcome, interval_ms: u32) {
        let slot = &mut self.slots[id];
        slot.in_flight = false;
        let delay_ms = match outcome {
            Outcome::Success => {
                slot.retry_count = 0;
                slot.server_not_before = None;
                interval_ms
            }
            Outcome::Transient { server_delay_ms } => {
                slot.retry_count = slot.retry_count.saturating_add(1);
                let backoff = transient_retry_delay_ms(slot.retry_count, interval_ms);
                match server_delay_ms {
                    Some(server) => {
                        let server = server.min(MAX_SERVER_DELAY_MS);
                        slot.server_not_before = Some(now + millis(server));
                        backoff.max(server)
                    }
                    None => backoff,
                }
            }
            Outcome::Wait { delay_ms } => {
                slot.retry_count = 0;
                slot.last_request = None;
                delay_ms
            }
        };
        slot.due = Some(now + millis(delay_ms));
    }

    /// A response discarded after a selection change: release the provider
    /// for a prompt retry, but keep any server deadline it carried.
    pub fn discard(&mut self, id: usize, now: Instant, server_delay_ms: Option<u32>) {
        let slot = &mut self.slots[id];
        slot.in_flight = false;
        slot.due = None;
        if let Some(server) = server_delay_ms {
            slot.server_not_before = Some(now + millis(server.min(MAX_SERVER_DELAY_MS)));
        }
    }

    /// A disabled provider forgets its cadence and backoff, but not a server
    /// deadline or request spacing, so re-enabling cannot bypass either.
    pub fn disable(&mut self, id: usize) {
        let slot = &mut self.slots[id];
        slot.due = None;
        slot.retry_count = 0;
    }

    pub fn retry_count(&self, id: usize) -> u32 {
        self.slots[id].retry_count
    }

    /// Delay until the earliest enabled, idle provider may start.
    pub fn next_delay_ms(&self, now: Instant, enabled: [bool; PROVIDERS]) -> Option<u32> {
        (0..PROVIDERS)
            .filter(|&id| enabled[id] && !self.slots[id].in_flight)
            .map(|id| {
                let wait = self.slots[id].ready_at(id, now).duration_since(now);
                // Round up: a truncated delay can wake the timer just before
                // the deadline (upstream PR #16, benb-atlantis). The caller
                // re-arms after every pass, so an early wake costs at most
                // one more timer, never a skipped cycle.
                (wait.as_nanos().div_ceil(1_000_000).min(u32::MAX as u128) as u32).max(MIN_TIMER_MS)
            })
            .min()
    }

    /// Compact log description of every enabled provider's schedule.
    pub fn describe(&self, now: Instant, enabled: [bool; PROVIDERS]) -> String {
        (0..PROVIDERS)
            .filter(|&id| enabled[id])
            .map(|id| {
                let slot = &self.slots[id];
                let wait = |at: Option<Instant>| {
                    at.map(|at| at.saturating_duration_since(now).as_millis())
                        .unwrap_or(0)
                };
                format!(
                    "{}:ready_in_ms={},retry={},server_wait_ms={},in_flight={}",
                    crate::poller::provider_name(id),
                    slot.ready_at(id, now).duration_since(now).as_millis(),
                    slot.retry_count,
                    wait(slot.server_not_before),
                    slot.in_flight
                )
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

fn millis(ms: u32) -> Duration {
    Duration::from_millis(u64::from(ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [bool; 3] = [true, true, false];
    const S30: u32 = 30_000;

    fn scheduled() -> Request {
        Request {
            providers: ALL,
            ..Default::default()
        }
    }

    fn manual() -> Request {
        Request {
            providers: ALL,
            ignore_due: true,
            ignore_backoff: true,
            ignore_spacing: true,
        }
    }

    fn reconfigure() -> Request {
        Request {
            providers: ALL,
            ignore_due: true,
            ..Default::default()
        }
    }

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn claude_requests_due_just_before_a_reset_move_to_just_after_it() {
        let interval = CLAUDE_MIN_POLL_MS;
        // Next request would land 30 s before the reset: moved to reset + 5 s.
        assert_eq!(
            reset_aligned_delay_ms(CLAUDE, interval, Some(150_000)),
            155_000
        );
        // Far from the reset, after it, or no reset: unchanged.
        for until in [Some(181_000), Some(60_000), Some(120_000), None] {
            assert_eq!(reset_aligned_delay_ms(CLAUDE, interval, until), interval);
        }
        // Other providers poll often enough and are never moved.
        assert_eq!(reset_aligned_delay_ms(CODEX, S30, Some(40_000)), S30);
        // The moved request stays inside the stale threshold (2x cadence).
        for until in (interval as u64 + 1)..(interval + RESET_ALIGN_WINDOW_MS) as u64 {
            let delay = reset_aligned_delay_ms(CLAUDE, interval, Some(until));
            assert!(delay >= interval && delay < 2 * interval);
        }
    }

    #[test]
    fn a_reset_aligned_claude_request_reads_the_new_window_within_seconds() {
        // Success at t0; the 5h reset is at t0 + 150 s. Without alignment the
        // request would run at t0 + 120 s and the next allowed one at
        // t0 + 240 s, 90 s after the reset.
        let t0 = Instant::now();
        let mut schedule = ProviderSchedule::default();
        assert!(schedule.begin(t0, scheduled())[CLAUDE]);
        let delay = reset_aligned_delay_ms(CLAUDE, CLAUDE_MIN_POLL_MS, Some(150_000));
        schedule.finish(CLAUDE, t0, Outcome::Success, delay);
        assert!(!schedule.begin(t0 + secs(120), scheduled())[CLAUDE]);
        assert!(schedule.begin(t0 + secs(155), scheduled())[CLAUDE]);
    }

    #[test]
    fn claude_rate_limit_storm_never_delays_codex() {
        // Mirrors the Build 5 field log: fixed 30 s cadence, Claude 429 on
        // every request. Codex must keep its own 30 s cadence throughout.
        let t0 = Instant::now();
        let mut schedule = ProviderSchedule::default();
        let mut now = t0;
        let mut codex_starts = Vec::new();
        let mut claude_starts = Vec::new();
        while now < t0 + secs(40 * 60) {
            let started = schedule.begin(now, scheduled());
            if started[CLAUDE] {
                claude_starts.push(now);
                schedule.finish(
                    CLAUDE,
                    now,
                    Outcome::Transient {
                        server_delay_ms: None,
                    },
                    provider_interval_ms(CLAUDE, S30),
                );
            }
            if started[CODEX] {
                codex_starts.push(now);
                schedule.finish(
                    CODEX,
                    now,
                    Outcome::Success,
                    provider_interval_ms(CODEX, S30),
                );
            }
            now += secs(1);
        }
        assert!(codex_starts
            .windows(2)
            .all(|pair| pair[1] - pair[0] <= secs(31)));
        assert!(codex_starts.len() >= 79);
        // Claude backs off on its own and never faster than its 120 s floor.
        assert!(claude_starts
            .windows(2)
            .all(|pair| pair[1] - pair[0] >= secs(120)));
        assert!(schedule.retry_count(CLAUDE) >= 4);
        assert_eq!(schedule.retry_count(CODEX), 0);
    }

    #[test]
    fn retry_after_deadline_survives_manual_refresh_disable_and_discard() {
        let now = Instant::now();
        let mut schedule = ProviderSchedule::default();
        assert_eq!(schedule.begin(now, manual()), [true, true, false]);
        schedule.finish(
            CLAUDE,
            now,
            Outcome::Transient {
                server_delay_ms: Some(471_000),
            },
            provider_interval_ms(CLAUDE, S30),
        );
        schedule.finish(CODEX, now, Outcome::Success, S30);

        let later = now + secs(200);
        assert_eq!(schedule.begin(later, manual()), [false, true, false]);
        schedule.finish(CODEX, later, Outcome::Success, S30);
        schedule.disable(CLAUDE);
        assert!(!schedule.begin(later, manual())[CLAUDE]);

        // A response discarded after a selection change still carries its deadline.
        let after = now + secs(472);
        assert!(schedule.begin(after, manual())[CLAUDE]);
        schedule.discard(CLAUDE, after, Some(60_000));
        assert!(!schedule.begin(after + secs(30), manual())[CLAUDE]);
        assert!(schedule.begin(after + secs(61), manual())[CLAUDE]);
    }

    #[test]
    fn server_deadline_extends_backoff_and_is_capped() {
        let now = Instant::now();
        let mut schedule = ProviderSchedule::default();
        schedule.begin(now, scheduled());
        schedule.finish(
            CODEX,
            now,
            Outcome::Transient {
                server_delay_ms: Some(u32::MAX),
            },
            S30,
        );
        let enabled = [false, true, false];
        assert_eq!(
            schedule.next_delay_ms(now, enabled),
            Some(MAX_SERVER_DELAY_MS)
        );
        assert!(!schedule.begin(now + secs(3600), manual())[CODEX]);
    }

    #[test]
    fn claude_spacing_applies_to_every_trigger_except_manual_refresh() {
        let now = Instant::now();
        let mut schedule = ProviderSchedule::default();
        assert!(schedule.begin(now, scheduled())[CLAUDE]);
        schedule.finish(
            CLAUDE,
            now,
            Outcome::Success,
            provider_interval_ms(CLAUDE, S30),
        );
        assert_eq!(provider_interval_ms(CLAUDE, S30), CLAUDE_MIN_POLL_MS);
        assert_eq!(provider_interval_ms(CODEX, S30), S30);

        let soon = now + secs(45);
        assert!(!schedule.begin(soon, reconfigure())[CLAUDE]);
        assert!(schedule.begin(soon, manual())[CLAUDE]);
        schedule.finish(CLAUDE, soon, Outcome::Success, CLAUDE_MIN_POLL_MS);
        assert_eq!(
            schedule.next_delay_ms(soon, [true, false, false]),
            Some(CLAUDE_MIN_POLL_MS)
        );
    }

    #[test]
    fn auth_wait_keeps_passive_recovery_at_base_cadence() {
        let now = Instant::now();
        let mut schedule = ProviderSchedule::default();
        schedule.begin(now, scheduled());
        schedule.finish(
            CLAUDE,
            now,
            Outcome::Wait { delay_ms: S30 },
            CLAUDE_MIN_POLL_MS,
        );
        assert_eq!(schedule.retry_count(CLAUDE), 0);
        assert!(schedule.begin(now + secs(30), scheduled())[CLAUDE]);
    }

    #[test]
    fn recovery_resets_only_the_recovered_provider() {
        let now = Instant::now();
        let mut schedule = ProviderSchedule::default();
        schedule.begin(now, scheduled());
        let transient = Outcome::Transient {
            server_delay_ms: None,
        };
        schedule.finish(CLAUDE, now, transient, CLAUDE_MIN_POLL_MS);
        schedule.finish(CODEX, now, transient, S30);
        let later = now + secs(150);
        schedule.begin(later, scheduled());
        schedule.finish(CODEX, later, Outcome::Success, S30);
        assert_eq!(schedule.retry_count(CODEX), 0);
        schedule.finish(CLAUDE, later, transient, CLAUDE_MIN_POLL_MS);
        assert_eq!(schedule.retry_count(CLAUDE), 2);
    }

    #[test]
    fn reconfiguration_refreshes_idle_providers_but_preserves_backoff() {
        let now = Instant::now();
        let mut schedule = ProviderSchedule::default();
        schedule.begin(now, scheduled());
        schedule.finish(CLAUDE, now, Outcome::Success, CLAUDE_MIN_POLL_MS);
        schedule.finish(
            CODEX,
            now,
            Outcome::Transient {
                server_delay_ms: None,
            },
            S30,
        );
        // Codex is backing off: a frequency change keeps its deadline.
        let soon = now + secs(5);
        assert!(!schedule.begin(soon, reconfigure())[CODEX]);

        let mut idle = ProviderSchedule::default();
        idle.begin(now, scheduled());
        idle.finish(CODEX, now, Outcome::Success, S30);
        // An idle provider refreshes promptly instead of waiting its old cadence.
        assert!(!idle.begin(soon, scheduled())[CODEX]);
        assert!(idle.begin(soon, reconfigure())[CODEX]);
    }

    #[test]
    fn in_flight_providers_are_not_restarted_and_do_not_arm_the_timer() {
        let now = Instant::now();
        let mut schedule = ProviderSchedule::default();
        assert_eq!(schedule.begin(now, manual()), [true, true, false]);
        assert_eq!(schedule.begin(now, manual()), [false, false, false]);
        assert_eq!(schedule.next_delay_ms(now, ALL), None);
        schedule.finish(CODEX, now, Outcome::Success, S30);
        assert_eq!(schedule.next_delay_ms(now, ALL), Some(S30));
    }

    #[test]
    fn transient_retry_wait_is_never_shorter_than_provider_cadence() {
        const POLL_5_MIN: u32 = 5 * 60_000;
        const POLL_1_HOUR: u32 = 60 * 60_000;
        assert_eq!(transient_retry_delay_ms(1, POLL_5_MIN), POLL_5_MIN);
        assert_eq!(transient_retry_delay_ms(2, POLL_5_MIN), POLL_5_MIN);
        assert_eq!(transient_retry_delay_ms(4, POLL_5_MIN), POLL_5_MIN);
        assert_eq!(transient_retry_delay_ms(5, POLL_5_MIN), 8 * 60_000);
        assert_eq!(transient_retry_delay_ms(10, POLL_5_MIN), RETRY_MAX_MS);
        assert_eq!(transient_retry_delay_ms(1, POLL_1_HOUR), POLL_1_HOUR);
    }

    /// Upstream PR #16's edge case: a deadline 0.5 ms after the last
    /// completion must not be rounded down to an early, empty wake.
    #[test]
    fn sub_millisecond_remainders_round_up() {
        let mut schedule = ProviderSchedule::default();
        let now = Instant::now();
        let all = Request {
            providers: [false, true, false],
            ..Default::default()
        };
        assert_eq!(schedule.begin(now, all), [false, true, false]);
        schedule.finish(CODEX, now, Outcome::Success, 15 * 60_000);
        let probe = now + Duration::from_millis(15 * 60_000) - Duration::from_micros(500);
        assert_eq!(
            schedule.next_delay_ms(probe, [false, true, false]),
            Some(MIN_TIMER_MS)
        );
        let probe = now + Duration::from_millis(15 * 60_000 - 1_500) - Duration::from_micros(500);
        assert_eq!(
            schedule.next_delay_ms(probe, [false, true, false]),
            Some(1_501),
            "1500.5 ms rounds up, not down"
        );
    }

    /// Two simulated hours of 15-minute cycles where every timer fires 2 ms
    /// early, re-arming after each pass as `window::spawn_poll` does: every
    /// cycle still runs within the minimum timer of its deadline.
    #[test]
    fn early_timer_wakes_never_skip_a_cycle() {
        const INTERVAL: u32 = 15 * 60_000;
        let mut schedule = ProviderSchedule::default();
        let start = Instant::now();
        let enabled = [false, true, false];
        let scheduled = Request {
            providers: enabled,
            ..Default::default()
        };
        let mut now = start;
        let mut starts = Vec::new();
        for _ in 0..200 {
            if schedule.begin(now, scheduled)[CODEX] {
                starts.push(now);
                schedule.finish(CODEX, now, Outcome::Success, INTERVAL);
            }
            if starts.len() == 9 {
                break;
            }
            let delay = schedule.next_delay_ms(now, enabled).unwrap();
            now += Duration::from_millis(u64::from(delay)) - Duration::from_millis(2);
        }
        assert_eq!(starts.len(), 9, "t=0 plus eight 15-minute cycles");
        assert!(starts[8] - start <= Duration::from_millis(8 * u64::from(INTERVAL + MIN_TIMER_MS)));
        for pair in starts.windows(2) {
            let gap = pair[1] - pair[0];
            assert!(
                gap >= Duration::from_millis(u64::from(INTERVAL))
                    && gap <= Duration::from_millis(u64::from(INTERVAL + MIN_TIMER_MS)),
                "cycle gap {gap:?}"
            );
        }
    }

    #[test]
    fn server_deadlines_hold_against_early_wakes() {
        let mut schedule = ProviderSchedule::default();
        let now = Instant::now();
        let claude = Request {
            providers: [true, false, false],
            ignore_due: true,
            ignore_backoff: true,
            ignore_spacing: true,
        };
        assert!(schedule.begin(now, claude)[CLAUDE]);
        schedule.finish(
            CLAUDE,
            now,
            Outcome::Transient {
                server_delay_ms: Some(471_000),
            },
            CLAUDE_MIN_POLL_MS,
        );
        let early = now + Duration::from_millis(471_000) - Duration::from_millis(2);
        assert!(
            !schedule.begin(early, claude)[CLAUDE],
            "even a forced request waits"
        );
        assert!(schedule.begin(now + Duration::from_millis(471_000), claude)[CLAUDE]);
    }
}
