use std::time::SystemTime;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageSection {
    pub percentage: f64,
    pub resets_at: Option<SystemTime>,
    /// Distinguishes a missing provider window from a real 0%-used window.
    pub available: bool,
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
}

#[derive(Clone, Debug, Default)]
pub struct AppUsageData {
    pub claude_code: Option<UsageData>,
    pub codex: Option<UsageData>,
    pub antigravity: Option<UsageData>,
}
