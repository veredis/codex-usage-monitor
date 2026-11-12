//! Pure taskbar placement geometry and shell layout-event coalescing.
//!
//! Every placement path (automatic layout, manual offsets, live dragging,
//! cross-taskbar drops, refreshes and resizes) uses the same notification-area
//! boundary, so no path can put the widget over the Show hidden icons button,
//! the tray or the clock. Notification-area discovery and the shared placement
//! bounds adapt upstream PR #25 (diogomrpr); the composed secondary-clock
//! reservation adapts upstream PR #16 (benb-atlantis).

use std::time::{Duration, Instant};

use windows::Win32::Foundation::RECT;

/// Accept a taskbar descendant as part of the notification area, clipped to
/// the taskbar. Windows 11 secondary taskbars can expose only a nested clock
/// or a narrow right-edge composition window; the full-taskbar composition
/// canvas and centered task-button hosts are rejected.
pub fn notification_candidate_rect(taskbar: RECT, class_name: &str, child: RECT) -> Option<RECT> {
    let rect = RECT {
        left: taskbar.left.max(child.left),
        top: taskbar.top.max(child.top),
        right: taskbar.right.min(child.right),
        bottom: taskbar.bottom.min(child.bottom),
    };
    if rect.left >= rect.right || rect.top >= rect.bottom {
        return None;
    }
    match class_name {
        "TrayNotifyWnd" | "TrayClockWClass" | "ClockButton" => Some(rect),
        "Windows.UI.Composition.DesktopWindowContentBridge"
            if rect.left > taskbar.left + (taskbar.right - taskbar.left) / 2
                && rect.right == taskbar.right =>
        {
            Some(rect)
        }
        _ => None,
    }
}

pub fn union_rect(a: RECT, b: RECT) -> RECT {
    RECT {
        left: a.left.min(b.left),
        top: a.top.min(b.top),
        right: a.right.max(b.right),
        bottom: a.bottom.max(b.bottom),
    }
}

/// Width to reserve at the right edge when the notification area exposes no
/// window: the primary tray's width scaled to this taskbar's height, never
/// less than four taskbar heights (a clock and date at any scale).
pub fn fallback_notification_width(height: i32, primary_area: Option<(i32, i32)>) -> i32 {
    let minimum = height.max(0).saturating_mul(4);
    primary_area
        .filter(|&(width, primary_height)| width > 0 && primary_height > 0)
        .map(|(width, primary_height)| {
            ((width as f64 * height as f64 / primary_height as f64).ceil() as i32).max(minimum)
        })
        .unwrap_or(minimum)
}

/// Left edge of the notification area on this taskbar. A rectangle that does
/// not intersect the taskbar (for example a stale one from another display) is
/// not evidence that the right edge is free.
pub fn notification_boundary(
    taskbar: RECT,
    notification: Option<RECT>,
    fallback_reserve: i32,
) -> i32 {
    let usable = notification.filter(|rect| {
        rect.left < rect.right
            && rect.top < rect.bottom
            && rect.left < taskbar.right
            && rect.right > taskbar.left
            && rect.top < taskbar.bottom
            && rect.bottom > taskbar.top
    });
    match usable {
        Some(rect) => rect.left.clamp(taskbar.left, taskbar.right),
        None => (taskbar.right - fallback_reserve.max(0)).max(taskbar.left),
    }
}

/// Automatic placement also stays left of other discoverable right-side shell
/// windows (widgets, Copilot, pen menus) that sit before the tray.
pub fn safe_anchor_left(taskbar: RECT, boundary: i32, occupied: &[RECT]) -> i32 {
    let midpoint = taskbar.left + (taskbar.right - taskbar.left) / 2;
    let taskbar_width = (taskbar.right - taskbar.left).max(1);
    occupied
        .iter()
        .filter(|rect| {
            let width = rect.right - rect.left;
            rect.left >= midpoint
                && rect.right <= taskbar.right
                && rect.right > rect.left
                && width <= taskbar_width * 3 / 4
        })
        .map(|rect| rect.left)
        .chain(std::iter::once(boundary))
        .filter(|left| *left > taskbar.left)
        .min()
        .unwrap_or(taskbar.right)
        .max(taskbar.left)
}

/// A suspicious boundary reading must be seen again this much later before
/// the widget follows it.
pub const BOUNDARY_CONFIRM_MS: u64 = 750;
/// A boundary that keeps changing is accepted after this long regardless.
pub const BOUNDARY_MAX_HOLD_MS: u64 = 5_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoundaryDecision {
    /// Place the widget against this boundary.
    Use(i32),
    /// The reading was not confirmed: keep `boundary` (the last confirmed
    /// one) and re-read after `recheck_ms`.
    Hold { boundary: i32, recheck_ms: u32 },
}

/// Explorer briefly reports transient notification-area geometry while the
/// shell reflows (seen in the field: the tray's left edge at 302 or 1198 px
/// instead of 1838, or no tray window at all, which falls back to a
/// reservation over the tray icons), then reports the real geometry again.
/// Following every reading moved the widget onto the task buttons or the
/// tray for seconds. The gate follows ordinary tray growth at once (the
/// widget must make room) and a new taskbar or taskbar size at once, but a
/// boundary moving right, or jumping left by more than about two tray icons,
/// only once a second reading `BOUNDARY_CONFIRM_MS` later agrees.
#[derive(Debug, Default)]
pub struct BoundaryGate {
    confirmed: Option<(isize, RECT, i32)>,
    pending: Option<(i32, Instant)>,
    diverged_since: Option<Instant>,
}

impl BoundaryGate {
    pub const fn new() -> Self {
        Self {
            confirmed: None,
            pending: None,
            diverged_since: None,
        }
    }

    pub fn decide(
        &mut self,
        now: Instant,
        taskbar_key: isize,
        taskbar: RECT,
        reading: i32,
    ) -> BoundaryDecision {
        let Some(last) = self
            .confirmed
            .filter(|(key, rect, _)| *key == taskbar_key && *rect == taskbar)
            .map(|(_, _, boundary)| boundary)
        else {
            return self.accept(taskbar_key, taskbar, reading);
        };
        let growth = last - reading;
        let height = (taskbar.bottom - taskbar.top).max(1);
        if growth == 0 || (growth > 0 && growth <= height.saturating_mul(2)) {
            return self.accept(taskbar_key, taskbar, reading);
        }
        let since = *self.diverged_since.get_or_insert(now);
        if now.duration_since(since) >= Duration::from_millis(BOUNDARY_MAX_HOLD_MS) {
            return self.accept(taskbar_key, taskbar, reading);
        }
        match self.pending {
            Some((pending, seen)) if pending == reading => {
                let waited = now.duration_since(seen);
                let confirm = Duration::from_millis(BOUNDARY_CONFIRM_MS);
                if waited >= confirm {
                    return self.accept(taskbar_key, taskbar, reading);
                }
                BoundaryDecision::Hold {
                    boundary: last,
                    recheck_ms: millis_ceil(confirm - waited),
                }
            }
            _ => {
                self.pending = Some((reading, now));
                BoundaryDecision::Hold {
                    boundary: last,
                    recheck_ms: BOUNDARY_CONFIRM_MS as u32,
                }
            }
        }
    }

    fn accept(&mut self, taskbar_key: isize, taskbar: RECT, reading: i32) -> BoundaryDecision {
        self.confirmed = Some((taskbar_key, taskbar, reading));
        self.pending = None;
        self.diverged_since = None;
        BoundaryDecision::Use(reading)
    }
}

fn millis_ceil(duration: Duration) -> u32 {
    u32::try_from(duration.as_nanos().div_ceil(1_000_000))
        .unwrap_or(u32::MAX)
        .max(1)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    /// Desktop coordinates; callers map them into the parent's client area.
    pub rect: RECT,
    /// The offset actually applied. The saved preference is left untouched so
    /// a temporarily narrower taskbar does not erase a manual position.
    pub offset: i32,
}

/// Right-anchor the widget `offset` pixels left of `anchor`. The offset is
/// clamped so the widget never crosses the anchor (and so never covers the
/// notification area) and, where it fits, never leaves the taskbar's left.
pub fn compute_placement(
    taskbar: RECT,
    anchor: i32,
    width: i32,
    height: i32,
    offset: i32,
) -> Placement {
    let anchor = anchor.clamp(taskbar.left, taskbar.right);
    let max_offset = (anchor - taskbar.left - width).max(0);
    let offset = offset.clamp(0, max_offset);
    let right = anchor - offset;
    let taskbar_height = taskbar.bottom - taskbar.top;
    let top = (taskbar.bottom - height).max(taskbar.top);
    Placement {
        rect: RECT {
            left: right - width,
            top,
            right,
            bottom: top + height.min(taskbar_height.max(0)),
        },
        offset,
    }
}

/// Offset that puts a widget dropped at `desired_left` on this anchor.
pub fn offset_for_left(anchor: i32, width: i32, desired_left: i32) -> i32 {
    (anchor - width - desired_left).max(0)
}

/// Shell layout events can arrive while our own tray-icon update is settling
/// (suppressed) or in bursts (throttled). Neither may be dropped: each one
/// leaves a pending reconciliation that re-reads the final geometry.
pub const LAYOUT_THROTTLE_MS: u64 = 500;
pub const LAYOUT_SETTLE_MS: u64 = 300;
/// A continuous event stream still reconciles at least this often.
pub const LAYOUT_MAX_DEFER_MS: u64 = 2_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutAction {
    /// Reposition now, then reconcile after `reconcile_ms` for late geometry.
    Immediate { reconcile_ms: u32 },
    /// Do not move now; reconcile after `reconcile_ms`.
    Deferred { reconcile_ms: u32 },
}

#[derive(Debug, Default)]
pub struct LayoutEventGate {
    suppressed_until: Option<Instant>,
    last_immediate: Option<Instant>,
    pending_since: Option<Instant>,
    pending_at: Option<Instant>,
}

impl LayoutEventGate {
    pub const fn new() -> Self {
        Self {
            suppressed_until: None,
            last_immediate: None,
            pending_since: None,
            pending_at: None,
        }
    }

    pub fn suppress(&mut self, now: Instant, duration: Duration) -> u32 {
        self.suppressed_until = Some(now + duration);
        self.schedule(now, duration + Duration::from_millis(LAYOUT_SETTLE_MS))
    }

    pub fn is_suppressed(&self, now: Instant) -> bool {
        self.suppressed_until.is_some_and(|until| now < until)
    }

    pub fn on_shell_event(&mut self, now: Instant) -> LayoutAction {
        if let Some(until) = self.suppressed_until.filter(|until| now < *until) {
            let wait = until - now + Duration::from_millis(LAYOUT_SETTLE_MS);
            return LayoutAction::Deferred {
                reconcile_ms: self.schedule(now, wait),
            };
        }
        let throttled = self.last_immediate.is_some_and(|last| {
            now.duration_since(last) < Duration::from_millis(LAYOUT_THROTTLE_MS)
        });
        let reconcile_ms = self.schedule(now, Duration::from_millis(LAYOUT_SETTLE_MS));
        if throttled {
            LayoutAction::Deferred { reconcile_ms }
        } else {
            self.last_immediate = Some(now);
            LayoutAction::Immediate { reconcile_ms }
        }
    }

    /// The reconciliation timer fired. While still suppressed it returns the
    /// rescheduled delay instead of applying transient geometry.
    pub fn take_due(&mut self, now: Instant) -> Result<(), u32> {
        if let Some(until) = self.suppressed_until.filter(|until| now < *until) {
            let wait = until - now + Duration::from_millis(LAYOUT_SETTLE_MS);
            self.pending_since = None;
            self.pending_at = None;
            return Err(self.schedule(now, wait));
        }
        self.pending_since = None;
        self.pending_at = None;
        Ok(())
    }

    #[cfg(test)]
    pub fn pending_at(&self) -> Option<Instant> {
        self.pending_at
    }

    /// Trailing debounce bounded by LAYOUT_MAX_DEFER_MS from the first event.
    fn schedule(&mut self, now: Instant, delay: Duration) -> u32 {
        let since = *self.pending_since.get_or_insert(now);
        let desired = now + delay;
        let latest = since + Duration::from_millis(LAYOUT_MAX_DEFER_MS).max(delay);
        let at = self
            .pending_at
            .map_or(desired, |pending| pending.max(desired))
            .min(latest)
            .max(now);
        self.pending_at = Some(at);
        u32::try_from(at.duration_since(now).as_millis())
            .unwrap_or(u32::MAX)
            .max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT {
            left,
            top,
            right,
            bottom,
        }
    }

    /// The live Windows 11 primary taskbar measured during the Build 7 audit:
    /// TrayNotifyWnd starts exactly at the Show hidden icons chevron.
    const TASKBAR: RECT = RECT {
        left: 0,
        top: 1032,
        right: 2560,
        bottom: 1080,
    };
    const TRAY: RECT = RECT {
        left: 1834,
        top: 1032,
        right: 2560,
        bottom: 1080,
    };

    #[test]
    fn primary_tray_boundary_is_the_show_hidden_icons_edge() {
        let boundary = notification_boundary(TASKBAR, Some(TRAY), 192);
        assert_eq!(boundary, 1834);
        let placement = compute_placement(TASKBAR, boundary, 284, 46, 0);
        assert_eq!(placement.rect, rect(1550, 1034, 1834, 1080));
        assert!(placement.rect.right <= TRAY.left);
    }

    #[test]
    fn notification_candidates_accept_tray_and_clock_but_not_the_full_canvas() {
        assert_eq!(
            notification_candidate_rect(TASKBAR, "TrayNotifyWnd", TRAY),
            Some(TRAY)
        );
        assert_eq!(
            notification_candidate_rect(TASKBAR, "TrayClockWClass", rect(2400, 1032, 2600, 1080)),
            Some(rect(2400, 1032, 2560, 1080))
        );
        let bridge = "Windows.UI.Composition.DesktopWindowContentBridge";
        assert_eq!(notification_candidate_rect(TASKBAR, bridge, TASKBAR), None);
        assert_eq!(
            notification_candidate_rect(TASKBAR, bridge, rect(2380, 1032, 2560, 1080)),
            Some(rect(2380, 1032, 2560, 1080))
        );
        assert_eq!(
            notification_candidate_rect(TASKBAR, "MSTaskListWClass", rect(55, 1032, 187, 1080)),
            None
        );
        // A clock reported on a different display does not belong here.
        assert_eq!(
            notification_candidate_rect(TASKBAR, "ClockButton", rect(2400, 0, 2560, 48)),
            None
        );
    }

    #[test]
    fn missing_notification_window_reserves_a_scaled_clock_area() {
        assert_eq!(fallback_notification_width(48, Some((274, 48))), 274);
        assert_eq!(fallback_notification_width(72, Some((274, 48))), 411);
        assert_eq!(fallback_notification_width(48, Some((100, 48))), 192);
        assert_eq!(fallback_notification_width(48, None), 192);
        assert_eq!(fallback_notification_width(48, Some((274, 0))), 192);

        // Secondary taskbar left of the primary, negative origin, 150% scale.
        let secondary = rect(-2880, 1008, 0, 1080);
        let reserve = fallback_notification_width(72, None);
        let boundary = notification_boundary(secondary, None, reserve);
        assert_eq!(boundary, -288);
        let placement = compute_placement(secondary, boundary, 426, 69, 0);
        assert_eq!(placement.rect.right, -288);
        assert_eq!(placement.rect.left, -714);
    }

    #[test]
    fn stale_or_foreign_notification_rect_falls_back_to_reservation() {
        for stale in [
            rect(1834, 0, 2560, 48),
            rect(3000, 1032, 3200, 1080),
            rect(2000, 1032, 2000, 1080),
        ] {
            assert_eq!(notification_boundary(TASKBAR, Some(stale), 192), 2368);
        }
        // A rectangle partly past the taskbar still clamps to the taskbar.
        assert_eq!(
            notification_boundary(TASKBAR, Some(rect(-50, 1032, 400, 1080)), 192),
            0
        );
    }

    #[test]
    fn tray_growth_moves_the_widget_left_instead_of_over_the_chevron() {
        // The live log shows the tray growing by 28 px (1834 -> 1806).
        for (tray_left, offset) in [(1834, 0), (1806, 0), (1834, 120), (1806, 120)] {
            let tray = RECT {
                left: tray_left,
                ..TRAY
            };
            let boundary = notification_boundary(TASKBAR, Some(tray), 192);
            let placement = compute_placement(TASKBAR, boundary, 284, 46, offset);
            assert_eq!(placement.rect.right, tray_left - offset);
            assert!(placement.rect.right <= tray.left);
        }
    }

    #[test]
    fn manual_and_dragged_offsets_share_the_same_clamp() {
        for (offset, expected_right, expected_offset) in [
            (-500, 1834, 0),
            (0, 1834, 0),
            (321, 1513, 321),
            (i32::MAX, 284, 1550),
        ] {
            let placement = compute_placement(TASKBAR, 1834, 284, 46, offset);
            assert_eq!(placement.rect.right, expected_right);
            assert_eq!(placement.offset, expected_offset);
            assert!(placement.rect.left >= TASKBAR.left);
            assert!(placement.rect.right <= TRAY.left);
        }
        // Dropping the widget over the tray snaps back to the boundary.
        assert_eq!(offset_for_left(1834, 284, 2000), 0);
        assert_eq!(offset_for_left(1834, 284, 1000), 550);
    }

    #[test]
    fn invalid_saved_offset_is_corrected_for_placement_without_losing_preference() {
        // A narrow taskbar (another display, or a lower resolution) cannot
        // hold the saved offset; the placement is clamped, the request is not.
        let narrow = rect(0, 728, 1024, 768);
        let tray = rect(800, 728, 1024, 768);
        let boundary = notification_boundary(narrow, Some(tray), 160);
        let placement = compute_placement(narrow, boundary, 284, 38, 1200);
        assert_eq!(placement.offset, 516);
        assert_eq!(placement.rect.left, 0);
        assert_eq!(placement.rect.right, 284);
        let wide = compute_placement(TASKBAR, 1834, 284, 46, 1200);
        assert_eq!(wide.offset, 1200);
    }

    #[test]
    fn widget_wider_than_the_free_span_never_crosses_the_tray() {
        let placement = compute_placement(rect(0, 0, 600, 48), 400, 700, 46, 50);
        assert_eq!(placement.offset, 0);
        assert_eq!(placement.rect.right, 400);
    }

    #[test]
    fn widget_width_changes_keep_the_right_anchor() {
        for width in [244, 278, 284, 290, 426] {
            let placement = compute_placement(TASKBAR, 1834, width, 46, 0);
            assert_eq!(placement.rect.right, 1834);
            assert_eq!(placement.rect.right - placement.rect.left, width);
        }
    }

    #[test]
    fn automatic_anchor_avoids_right_side_shell_windows_but_not_task_buttons() {
        let occupied = [
            rect(55, 1032, 187, 1080),
            TRAY,
            rect(1700, 1032, 1780, 1080),
            TASKBAR,
        ];
        assert_eq!(safe_anchor_left(TASKBAR, 1834, &occupied), 1700);
        assert_eq!(safe_anchor_left(TASKBAR, 1834, &[]), 1834);
    }

    #[test]
    fn top_taskbar_and_dpi_heights_anchor_to_the_bottom_edge() {
        let top = rect(0, 0, 2560, 72);
        let placement = compute_placement(top, 2200, 426, 69, 0);
        assert_eq!(placement.rect.top, 3);
        assert_eq!(placement.rect.bottom, 72);
    }

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    #[test]
    fn transient_tray_readings_from_the_field_log_never_move_the_widget() {
        // Field log (PID 39360): boundaries 302, 1198 and the 2368 fallback
        // (no tray window) each appeared for one reading, then 1838 again.
        let start = Instant::now();
        let mut gate = BoundaryGate::new();
        assert_eq!(
            gate.decide(start, 1, TASKBAR, 1838),
            BoundaryDecision::Use(1838)
        );
        for (step, transient) in [302, 1198, 2368, 1870].into_iter().enumerate() {
            let at = start + ms(10_000 * (step as u64 + 1));
            assert_eq!(
                gate.decide(at, 1, TASKBAR, transient),
                BoundaryDecision::Hold {
                    boundary: 1838,
                    recheck_ms: BOUNDARY_CONFIRM_MS as u32
                }
            );
            // The re-read finds the real tray again.
            assert_eq!(
                gate.decide(at + ms(BOUNDARY_CONFIRM_MS), 1, TASKBAR, 1838),
                BoundaryDecision::Use(1838)
            );
        }
    }

    #[test]
    fn ordinary_tray_growth_applies_at_once_and_lasting_changes_are_confirmed() {
        let start = Instant::now();
        let mut gate = BoundaryGate::new();
        gate.decide(start, 1, TASKBAR, 1838);
        // A new tray icon (28 px) must push the widget left immediately.
        assert_eq!(
            gate.decide(start, 1, TASKBAR, 1810),
            BoundaryDecision::Use(1810)
        );
        // The icon goes away: the widget follows once the reading holds.
        assert!(matches!(
            gate.decide(start + ms(100), 1, TASKBAR, 1838),
            BoundaryDecision::Hold { boundary: 1810, .. }
        ));
        assert_eq!(
            gate.decide(start + ms(400), 1, TASKBAR, 1838),
            BoundaryDecision::Hold {
                boundary: 1810,
                recheck_ms: 450
            }
        );
        assert_eq!(
            gate.decide(start + ms(850), 1, TASKBAR, 1838),
            BoundaryDecision::Use(1838)
        );
        // A large real growth (many icons at once) also follows when confirmed.
        assert!(matches!(
            gate.decide(start + ms(2_000), 1, TASKBAR, 1500),
            BoundaryDecision::Hold { .. }
        ));
        assert_eq!(
            gate.decide(start + ms(2_750), 1, TASKBAR, 1500),
            BoundaryDecision::Use(1500)
        );
    }

    #[test]
    fn new_taskbar_or_size_is_used_at_once_and_unsettled_geometry_is_bounded() {
        let start = Instant::now();
        let mut gate = BoundaryGate::new();
        gate.decide(start, 1, TASKBAR, 1838);
        // Another display's taskbar, or a resized taskbar: no history applies.
        assert_eq!(
            gate.decide(start, 2, TASKBAR, 600),
            BoundaryDecision::Use(600)
        );
        let taller = RECT {
            top: 1008,
            ..TASKBAR
        };
        assert_eq!(
            gate.decide(start, 2, taller, 1700),
            BoundaryDecision::Use(1700)
        );
        // Readings that never settle are still followed after the bound.
        let mut now = start;
        let mut decision = BoundaryDecision::Use(0);
        for value in (0..12).map(|step| 1900 + step * 50) {
            now += ms(700);
            decision = gate.decide(now, 2, taller, value);
            if matches!(decision, BoundaryDecision::Use(_)) {
                break;
            }
        }
        assert!(now - start <= ms(BOUNDARY_MAX_HOLD_MS + 1_400));
        assert!(matches!(decision, BoundaryDecision::Use(_)));
    }

    #[test]
    fn suppressed_events_are_deferred_until_suppression_ends() {
        let start = Instant::now();
        let mut gate = LayoutEventGate::default();
        let first = gate.suppress(start, Duration::from_millis(750));
        assert_eq!(first, 1050);
        let action = gate.on_shell_event(start + Duration::from_millis(100));
        assert_eq!(action, LayoutAction::Deferred { reconcile_ms: 950 });
        // The timer firing early during suppression reschedules instead of
        // either dropping or applying transient geometry.
        assert_eq!(gate.take_due(start + Duration::from_millis(500)), Err(550));
        assert_eq!(gate.take_due(start + Duration::from_millis(1050)), Ok(()));
        assert_eq!(gate.pending_at(), None);
    }

    #[test]
    fn throttled_bursts_end_with_a_trailing_reconciliation() {
        let start = Instant::now();
        let mut gate = LayoutEventGate::default();
        assert_eq!(
            gate.on_shell_event(start),
            LayoutAction::Immediate { reconcile_ms: 300 }
        );
        // A second change 200 ms later used to be dropped entirely.
        assert_eq!(
            gate.on_shell_event(start + Duration::from_millis(200)),
            LayoutAction::Deferred { reconcile_ms: 300 }
        );
        assert_eq!(gate.pending_at(), Some(start + Duration::from_millis(500)));
        assert_eq!(gate.take_due(start + Duration::from_millis(500)), Ok(()));
        assert!(matches!(
            gate.on_shell_event(start + Duration::from_millis(900)),
            LayoutAction::Immediate { .. }
        ));
    }

    #[test]
    fn continuous_event_streams_still_reconcile_within_the_bound() {
        let start = Instant::now();
        let mut gate = LayoutEventGate::default();
        for step in 0..20 {
            let _ = gate.on_shell_event(start + Duration::from_millis(step * 100));
        }
        let due = gate.pending_at().unwrap();
        assert!(due <= start + Duration::from_millis(LAYOUT_MAX_DEFER_MS));
    }
}
