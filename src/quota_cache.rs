//! Per-provider snapshots of the last successfully fetched quota, restored at
//! startup so a restart during an outage or an expired sign-in still shows the
//! last known values, marked stale.
//!
//! Adapted from SoKeiKei's upstream PR #30 (`src/quota_cache.rs`, MIT): the
//! versioned, size-capped, `deny_unknown_fields` file format, scope-matched
//! loading, "older never replaces newer", and temp-file-and-rename writes.
//! Changes for this fork: scopes are account fingerprints (stable across token
//! refreshes) rather than token digests, Codex Credits and window
//! availability are kept, snapshots expire after `MAX_SNAPSHOT_AGE`, and
//! unchanged values are rewritten at most every `REWRITE_INTERVAL`.
//!
//! Files hold only percentages, reset/update times and a 64-hex SHA-256 scope:
//! never tokens, emails or account identifiers.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::models::{CreditBalance, UsageData, UsageSection};

const CACHE_VERSION: u8 = 1;
const MAX_CACHE_BYTES: u64 = 16 * 1024;
/// A weekly window is the longest any provider reports; older data is noise.
pub const MAX_SNAPSHOT_AGE: Duration = Duration::from_secs(8 * 24 * 60 * 60);
/// Unchanged values are rewritten at most this often (Codex polls every 30 s).
pub const REWRITE_INTERVAL: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub data: UsageData,
    pub updated_at: SystemTime,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredCache {
    version: u8,
    scope: String,
    updated_at_unix_ms: u64,
    session: StoredSection,
    weekly: StoredSection,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    credits: Option<StoredCredits>,
    /// The provider's quota group (Antigravity). Absent for providers with one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    quota_group: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredSection {
    percentage: f64,
    resets_at_unix_secs: Option<u64>,
    available: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
enum StoredCredits {
    Amount(f64),
    Unlimited,
}

fn cache_dir() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("APPDATA")?).join("CodexUsage"))
}

fn file_name(id: usize) -> Option<&'static str> {
    match id {
        0 => Some("quota-claude.json"),
        1 => Some("quota-codex.json"),
        2 => Some("quota-antigravity.json"),
        _ => None,
    }
}

/// The snapshot for `id` when it belongs to `scope` and is still plausible.
pub fn load(id: usize, scope: &str) -> Option<Snapshot> {
    load_from(&cache_dir()?.join(file_name(id)?), scope, SystemTime::now())
        .filter(|snapshot| restorable(id, snapshot))
}

/// Antigravity snapshots written before Build 18 do not say which quota group
/// they hold, and earlier builds could pick the wrong one: not restored.
fn restorable(id: usize, snapshot: &Snapshot) -> bool {
    id != 2 || snapshot.data.quota_group.is_some()
}

/// Persist a successful observation. Older or equal-time observations never
/// replace a newer snapshot for the same scope.
pub fn save(id: usize, scope: &str, data: &UsageData, updated_at: SystemTime) -> io::Result<()> {
    let dir = cache_dir().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no APPDATA"))?;
    let name = file_name(id).ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    save_to(&dir.join(name), scope, data, updated_at, SystemTime::now())
}

/// Whether a new observation is worth writing: values changed, or the
/// unchanged snapshot is older than `REWRITE_INTERVAL`.
pub fn should_write(previous: Option<&Snapshot>, data: &UsageData, updated_at: SystemTime) -> bool {
    match previous {
        None => true,
        Some(previous) => {
            !same_values(&previous.data, data)
                || updated_at
                    .duration_since(previous.updated_at)
                    .is_ok_and(|age| age >= REWRITE_INTERVAL)
        }
    }
}

fn same_values(a: &UsageData, b: &UsageData) -> bool {
    a.session == b.session
        && a.weekly == b.weekly
        && a.credits == b.credits
        && a.quota_group == b.quota_group
}

fn load_from(path: &Path, scope: &str, now: SystemTime) -> Option<Snapshot> {
    let stored = read_stored(path)?;
    let updated_at = valid_time(&stored, scope, now)?;
    Some(Snapshot {
        data: from_stored(&stored)?,
        updated_at,
    })
}

fn save_to(
    path: &Path,
    scope: &str,
    data: &UsageData,
    updated_at: SystemTime,
    now: SystemTime,
) -> io::Result<()> {
    static WRITE_LOCK: Mutex<()> = Mutex::new(());
    let invalid = |message: &str| io::Error::new(io::ErrorKind::InvalidInput, message.to_string());
    if !valid_scope(scope) {
        return Err(invalid("scope must be a SHA-256 fingerprint"));
    }
    if updated_at > now {
        return Err(invalid("update time is in the future"));
    }
    let stored =
        to_stored(scope, data, updated_at).ok_or_else(|| invalid("invalid quota values"))?;
    let bytes = serde_json::to_vec(&stored).map_err(io::Error::other)?;

    let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = read_stored(path).and_then(|s| valid_time(&s, scope, now)) {
        if existing >= updated_at {
            return Ok(());
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let written = (|| {
        let mut file = File::create(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        // MoveFileExW(REPLACE_EXISTING): readers see the old or the new file.
        std::fs::rename(&temp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    written
}

fn read_stored(path: &Path) -> Option<StoredCache> {
    let mut bytes = Vec::new();
    File::open(path)
        .ok()?
        .take(MAX_CACHE_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_CACHE_BYTES {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

fn valid_time(stored: &StoredCache, scope: &str, now: SystemTime) -> Option<SystemTime> {
    if stored.version != CACHE_VERSION || stored.scope != scope || !valid_scope(scope) {
        return None;
    }
    let updated_at = UNIX_EPOCH.checked_add(Duration::from_millis(stored.updated_at_unix_ms))?;
    let age = now.duration_since(updated_at).ok()?;
    (age <= MAX_SNAPSHOT_AGE).then_some(updated_at)
}

fn valid_scope(scope: &str) -> bool {
    scope.len() == 64 && scope.bytes().all(|b| b.is_ascii_hexdigit())
}

fn valid_percentage(value: f64) -> bool {
    value.is_finite() && (0.0..=100.0).contains(&value)
}

fn to_stored(scope: &str, data: &UsageData, updated_at: SystemTime) -> Option<StoredCache> {
    fn section(section: &UsageSection) -> Option<StoredSection> {
        if !valid_percentage(section.percentage) {
            return None;
        }
        let resets_at_unix_secs = match section.resets_at {
            Some(reset) => Some(reset.duration_since(UNIX_EPOCH).ok()?.as_secs()),
            None => None,
        };
        Some(StoredSection {
            percentage: section.percentage,
            resets_at_unix_secs,
            available: section.available,
        })
    }
    let credits = match data.credits {
        Some(CreditBalance::Amount(amount)) if amount.is_finite() => {
            Some(StoredCredits::Amount(amount))
        }
        Some(CreditBalance::Amount(_)) => return None,
        Some(CreditBalance::Unlimited) => Some(StoredCredits::Unlimited),
        None => None,
    };
    Some(StoredCache {
        version: CACHE_VERSION,
        scope: scope.to_string(),
        updated_at_unix_ms: updated_at
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_millis()
            .try_into()
            .ok()?,
        session: section(&data.session)?,
        weekly: section(&data.weekly)?,
        credits,
        quota_group: data.quota_group.clone(),
    })
}

fn from_stored(stored: &StoredCache) -> Option<UsageData> {
    fn section(section: &StoredSection) -> Option<UsageSection> {
        if !valid_percentage(section.percentage) {
            return None;
        }
        let resets_at = match section.resets_at_unix_secs {
            Some(secs) => Some(UNIX_EPOCH.checked_add(Duration::from_secs(secs))?),
            None => None,
        };
        Some(UsageSection {
            percentage: section.percentage,
            resets_at,
            available: section.available,
        })
    }
    let credits = match stored.credits {
        Some(StoredCredits::Amount(amount)) if amount.is_finite() => {
            Some(CreditBalance::Amount(amount))
        }
        Some(StoredCredits::Amount(_)) => return None,
        Some(StoredCredits::Unlimited) => Some(CreditBalance::Unlimited),
        None => None,
    };
    Some(UsageData {
        session: section(&stored.session)?,
        weekly: section(&stored.weekly)?,
        credits,
        quota_group: stored.quota_group.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "codex-usage-quota-cache-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn file(&self) -> PathBuf {
            self.0.join("quota.json")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn scope(c: char) -> String {
        std::iter::repeat_n(c, 64).collect()
    }

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn data(session: f64) -> UsageData {
        UsageData {
            session: UsageSection {
                percentage: session,
                resets_at: Some(at(1_800_010_000)),
                available: true,
            },
            weekly: UsageSection {
                percentage: 61.0,
                resets_at: None,
                available: false,
            },
            quota_group: None,
            credits: Some(CreditBalance::Amount(12.5)),
        }
    }

    const NOW: u64 = 1_800_000_000;

    #[test]
    fn round_trip_keeps_values_availability_credits_and_time() {
        let dir = TempDir::new();
        let updated = at(NOW - 30);
        save_to(&dir.file(), &scope('a'), &data(24.5), updated, at(NOW)).unwrap();
        let loaded = load_from(&dir.file(), &scope('a'), at(NOW)).unwrap();
        assert_eq!(loaded.updated_at, updated);
        assert_eq!(loaded.data.session, data(24.5).session);
        assert_eq!(loaded.data.weekly, data(24.5).weekly);
        assert_eq!(loaded.data.credits, Some(CreditBalance::Amount(12.5)));
        let text = std::fs::read_to_string(dir.file()).unwrap();
        assert!(!text.contains("token") && !text.contains('@'));
    }

    #[test]
    fn antigravity_snapshots_keep_their_group_and_unlabelled_ones_are_not_restored() {
        let dir = TempDir::new();
        // A file without a group, as written before Build 18.
        save_to(&dir.file(), &scope('a'), &data(2.0), at(NOW - 30), at(NOW)).unwrap();
        let text = std::fs::read_to_string(dir.file()).unwrap();
        assert!(
            !text.contains("quota_group"),
            "unchanged format without a group"
        );
        let old = load_from(&dir.file(), &scope('a'), at(NOW)).unwrap();
        assert!(!restorable(2, &old));
        assert!(restorable(0, &old) && restorable(1, &old));

        let mut gemini = data(0.0);
        gemini.quota_group = Some("Gemini Models".into());
        save_to(&dir.file(), &scope('a'), &gemini, at(NOW - 10), at(NOW)).unwrap();
        let loaded = load_from(&dir.file(), &scope('a'), at(NOW)).unwrap();
        assert_eq!(loaded.data.quota_group.as_deref(), Some("Gemini Models"));
        assert!(restorable(2, &loaded));
        assert!(!same_values(&old.data, &{
            let mut relabelled = old.data.clone();
            relabelled.quota_group = Some("Gemini Models".into());
            relabelled
        }));
    }

    #[test]
    fn older_or_equal_observations_never_replace_newer_ones() {
        let dir = TempDir::new();
        save_to(&dir.file(), &scope('a'), &data(40.0), at(NOW - 10), at(NOW)).unwrap();
        save_to(&dir.file(), &scope('a'), &data(10.0), at(NOW - 20), at(NOW)).unwrap();
        save_to(&dir.file(), &scope('a'), &data(11.0), at(NOW - 10), at(NOW)).unwrap();
        let loaded = load_from(&dir.file(), &scope('a'), at(NOW)).unwrap();
        assert_eq!(loaded.data.session.percentage, 40.0);
        save_to(&dir.file(), &scope('a'), &data(45.0), at(NOW - 5), at(NOW)).unwrap();
        let loaded = load_from(&dir.file(), &scope('a'), at(NOW)).unwrap();
        assert_eq!(loaded.data.session.percentage, 45.0);
    }

    #[test]
    fn other_accounts_corrupt_truncated_future_and_expired_files_are_ignored() {
        let dir = TempDir::new();
        save_to(&dir.file(), &scope('a'), &data(40.0), at(NOW - 10), at(NOW)).unwrap();
        assert!(load_from(&dir.file(), &scope('b'), at(NOW)).is_none());
        // A different account's snapshot never blocks this account's save.
        save_to(&dir.file(), &scope('b'), &data(5.0), at(NOW - 20), at(NOW)).unwrap();
        assert_eq!(
            load_from(&dir.file(), &scope('b'), at(NOW))
                .unwrap()
                .data
                .session
                .percentage,
            5.0
        );
        // Snapshots past MAX_SNAPSHOT_AGE, or from the future, are not restored.
        assert!(load_from(
            &dir.file(),
            &scope('b'),
            at(NOW) + MAX_SNAPSHOT_AGE + Duration::from_secs(60)
        )
        .is_none());
        assert!(load_from(&dir.file(), &scope('b'), at(NOW - 3600)).is_none());
        let full = std::fs::read(dir.file()).unwrap();
        std::fs::write(dir.file(), &full[..full.len() / 2]).unwrap();
        assert!(load_from(&dir.file(), &scope('b'), at(NOW)).is_none());
        let mut tampered: serde_json::Value = serde_json::from_slice(&full).unwrap();
        tampered["session"]["percentage"] = serde_json::json!(140.0);
        std::fs::write(dir.file(), serde_json::to_vec(&tampered).unwrap()).unwrap();
        assert!(load_from(&dir.file(), &scope('b'), at(NOW)).is_none());
        let mut extra: serde_json::Value = serde_json::from_slice(&full).unwrap();
        extra["access_token"] = serde_json::json!("synthetic");
        std::fs::write(dir.file(), serde_json::to_vec(&extra).unwrap()).unwrap();
        assert!(load_from(&dir.file(), &scope('b'), at(NOW)).is_none());
        std::fs::write(dir.file(), vec![b' '; (MAX_CACHE_BYTES + 1) as usize]).unwrap();
        assert!(load_from(&dir.file(), &scope('b'), at(NOW)).is_none());
    }

    #[test]
    fn invalid_saves_are_rejected_and_keep_the_previous_snapshot() {
        let dir = TempDir::new();
        save_to(&dir.file(), &scope('a'), &data(40.0), at(NOW - 10), at(NOW)).unwrap();
        assert!(save_to(&dir.file(), "short", &data(1.0), at(NOW - 1), at(NOW)).is_err());
        assert!(save_to(
            &dir.file(),
            &scope('a'),
            &data(f64::NAN),
            at(NOW - 1),
            at(NOW)
        )
        .is_err());
        assert!(save_to(&dir.file(), &scope('a'), &data(1.0), at(NOW + 10), at(NOW)).is_err());
        assert_eq!(
            load_from(&dir.file(), &scope('a'), at(NOW))
                .unwrap()
                .data
                .session
                .percentage,
            40.0
        );
        let leftovers = std::fs::read_dir(&dir.0)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|x| x == "tmp")
            })
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn concurrent_saves_leave_one_valid_newest_snapshot() {
        let dir = TempDir::new();
        let path = dir.file();
        std::thread::scope(|threads| {
            for i in 0..8u64 {
                let path = path.clone();
                threads.spawn(move || {
                    let _ = save_to(
                        &path,
                        &scope('a'),
                        &data(i as f64),
                        at(NOW - 100 + i),
                        at(NOW),
                    );
                });
            }
        });
        let loaded = load_from(&path, &scope('a'), at(NOW)).unwrap();
        assert_eq!(loaded.data.session.percentage, 7.0);
    }

    #[test]
    fn unchanged_values_are_rewritten_only_after_the_interval() {
        let previous = Snapshot {
            data: data(40.0),
            updated_at: at(NOW),
        };
        assert!(should_write(None, &data(40.0), at(NOW)));
        assert!(!should_write(Some(&previous), &data(40.0), at(NOW + 30)));
        assert!(should_write(Some(&previous), &data(41.0), at(NOW + 30)));
        assert!(should_write(
            Some(&previous),
            &data(40.0),
            at(NOW) + REWRITE_INTERVAL
        ));
    }
}
