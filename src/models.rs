use std::time::SystemTime;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageSection {
    pub percentage: f64,
    pub resets_at: Option<SystemTime>,
    /// Distinguishes a missing provider window from a real 0%-used window.
    pub available: bool,
}

/// The optional model-fallback bucket exposed by Codex as `gpt-reserve`.
/// `active` is deliberately optional because most usage responses do not
/// identify which fallback is currently serving requests.
#[derive(Clone, Debug, PartialEq)]
pub struct LunaReserveUsage {
    pub section: UsageSection,
    pub available: bool,
    pub active: Option<bool>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CreditBalance {
    Amount(f64),
    Unlimited,
}

#[derive(Clone, Debug, Default)]
pub struct UsageData {
    pub session: UsageSection,
    pub weekly: UsageSection,
    pub credits: Option<CreditBalance>,
    pub luna_reserve: Option<LunaReserveUsage>,
}

#[derive(Clone, Debug, Default)]
pub struct AppUsageData {
    pub claude_code: Option<UsageData>,
    pub codex: Option<UsageData>,
    pub antigravity: Option<UsageData>,
}
