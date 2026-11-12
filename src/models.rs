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
    /// The provider's name for the quota group shown, when it reports several
    /// (Antigravity: "Gemini Models"). Live data only; not persisted.
    pub quota_group: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct AppUsageData {
    pub claude_code: Option<UsageData>,
    pub codex: Option<UsageData>,
    pub antigravity: Option<UsageData>,
}
