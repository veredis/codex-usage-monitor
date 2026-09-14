use std::time::SystemTime;

#[derive(Clone, Debug, Default)]
pub struct UsageSection {
    pub percentage: f64,
    pub resets_at: Option<SystemTime>,
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
