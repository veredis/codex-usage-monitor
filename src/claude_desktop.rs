//! Read-only access to the Claude Desktop app's own sign-in.
//!
//! Claude Desktop (whose Code tab many people use instead of the CLI) keeps a
//! separate login that it refreshes itself; the CLI's `.credentials.json` then
//! goes stale. Desktop stores its OAuth token cache in `config.json` under
//! `oauth:tokenCacheV2`, encrypted with Chromium's `os_crypt` scheme
//! ("v10" + 12-byte nonce + AES-256-GCM ciphertext + 16-byte tag) under a key
//! that `Local State` keeps DPAPI-protected for the current Windows user.
//!
//! The approach (and the data-directory candidates, including the MSIX and
//! `Claude-Data` locations) follows SKGoC's MIT-licensed Claude Meter for
//! Windows (`src/DesktopCredentialStore.cs`); this is an independent Rust
//! implementation. It never writes, refreshes or rotates anything: the token
//! is only read, held in memory for the usage request, and never logged.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// A Desktop access token for one account. The refresh token is never kept.
#[derive(Clone)]
pub struct DesktopToken {
    pub access_token: String,
    pub expires_at_ms: Option<i64>,
    /// Account UUID from the key's `acct:<uuid>|` prefix (lowercase).
    pub account_uuid: Option<String>,
    /// Organization UUID, the key's second field (lowercase).
    pub org_uuid: Option<String>,
    /// The key's scope includes `user:profile`, which the usage endpoint needs.
    pub profile_scope: bool,
}

impl std::fmt::Debug for DesktopToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DesktopToken")
            .field("expires_at_ms", &self.expires_at_ms)
            .field("has_account", &self.account_uuid.is_some())
            .field("has_org", &self.org_uuid.is_some())
            .field("profile_scope", &self.profile_scope)
            .finish_non_exhaustive()
    }
}

/// Why no Desktop token is available; safe to log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unavailable {
    NotInstalled,
    NoTokenCache,
    DecryptFailed,
    /// No entry key carries an `acct:` account: a format we cannot attribute.
    AccountUnparsed,
    /// Entries name accounts, none of them the CLI's account.
    AccountMismatch,
    /// The CLI's account is present only under another organization.
    OrganizationMismatch,
    /// Several accounts or organizations fit and nothing decides between them.
    Ambiguous,
    /// The matching entries lack the `user:profile` scope.
    NoProfileScope,
}

impl Unavailable {
    pub fn category(self) -> &'static str {
        match self {
            Self::NotInstalled => "not_installed",
            Self::NoTokenCache => "no_token_cache",
            Self::DecryptFailed => "decrypt_failed",
            Self::AccountUnparsed => "account_unparsed",
            Self::AccountMismatch => "account_mismatch",
            Self::OrganizationMismatch => "organization_mismatch",
            Self::Ambiguous => "ambiguous",
            Self::NoProfileScope => "no_profile_scope",
        }
    }
}

/// The signed-in Claude Code CLI identity a Desktop entry must match.
#[derive(Clone, Copy, Default)]
pub struct Expected<'a> {
    pub account_uuid: Option<&'a str>,
    pub org_uuid: Option<&'a str>,
}

const MAX_CONFIG_BYTES: u64 = 4 * 1024 * 1024;

struct Cached {
    path: PathBuf,
    stamp: SystemTime,
    len: u64,
    entries: Result<Vec<DesktopToken>, Unavailable>,
}

/// Desktop's token for the CLI's signed-in account and organization, or for
/// the only signed-in account when the CLI identity is unknown.
///
/// Decryption is repeated only when `config.json` changes; Desktop rewrites it
/// when it rotates its token.
pub fn token_for_account(expected: Expected<'_>) -> Result<DesktopToken, Unavailable> {
    let entries = cached_entries()?;
    select_token(&entries, expected)
}

fn cached_entries() -> Result<Vec<DesktopToken>, Unavailable> {
    static CACHE: OnceLock<Mutex<Option<Cached>>> = OnceLock::new();
    let dir = find_data_dir().ok_or(Unavailable::NotInstalled)?;
    let config = dir.join("config.json");
    let meta = std::fs::metadata(&config).map_err(|_| Unavailable::NotInstalled)?;
    let stamp = meta.modified().unwrap_or(UNIX_EPOCH);
    let mut cache = CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(cached) = cache.as_ref() {
        if cached.path == config && cached.stamp == stamp && cached.len == meta.len() {
            return cached.entries.clone();
        }
    }
    let entries = read_entries(&dir, meta.len());
    *cache = Some(Cached {
        path: config,
        stamp,
        len: meta.len(),
        entries: entries.clone(),
    });
    entries
}

fn read_entries(dir: &Path, len: u64) -> Result<Vec<DesktopToken>, Unavailable> {
    if len > MAX_CONFIG_BYTES {
        return Err(Unavailable::NoTokenCache);
    }
    let config: Value = std::fs::read(dir.join("config.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .ok_or(Unavailable::NoTokenCache)?;
    let blob = ["oauth:tokenCacheV2", "oauth:tokenCache"]
        .iter()
        .find_map(|key| config.get(*key).and_then(Value::as_str))
        .filter(|value| !value.is_empty())
        .and_then(decode_base64)
        .ok_or(Unavailable::NoTokenCache)?;
    let plain =
        decrypt_os_crypt(&blob, &dir.join("Local State")).ok_or(Unavailable::DecryptFailed)?;
    let json: Value = serde_json::from_slice(&plain).map_err(|_| Unavailable::DecryptFailed)?;
    let entries = parse_token_cache(&json);
    if entries.is_empty() {
        Err(Unavailable::NoTokenCache)
    } else {
        Ok(entries)
    }
}

/// Privacy-safe description of the Desktop sign-in for `--probe-claude-sign-in`:
/// location kind, counts, match booleans and expiry only, never identifiers.
pub fn probe_lines(expected: Expected<'_>) -> Vec<String> {
    let mut lines = Vec::new();
    let Some(dir) = find_data_dir() else {
        lines.push("desktop_data_dir=none".into());
        return lines;
    };
    let text = dir.to_string_lossy().to_ascii_lowercase();
    let kind = if text.contains("\\packages\\claude_") {
        "msix_package"
    } else if text.contains("claude-data") {
        "claude_data"
    } else {
        "classic_appdata"
    };
    lines.push(format!("desktop_data_dir={kind}"));
    match cached_entries() {
        Err(reason) => lines.push(format!("desktop_token_cache={}", reason.category())),
        Ok(entries) => {
            let stats = KeyStats::of(&entries, expected);
            lines.push(format!(
                "desktop_token_cache=decrypted entries={} entries_with_account={} accounts={} entries_with_org={}",
                entries.len(),
                stats.with_account,
                stats.accounts,
                stats.with_org
            ));
            lines.push(format!(
                "desktop_match cli_account_in_cache={} cli_org_known={} cli_org_entries={} other_org_entries={} profile_scope_entries={}",
                stats.account_entries > 0,
                expected.org_uuid.is_some(),
                stats.org_entries,
                stats.account_entries - stats.org_entries,
                stats.profile_entries
            ));
            match select_token(&entries, expected) {
                Err(reason) => lines.push(format!("desktop_selected={}", reason.category())),
                Ok(token) => lines.push(format!(
                    "desktop_selected=ok matches_cli_account={} expires_in_s={}",
                    expected.account_uuid.is_some(),
                    token
                        .expires_at_ms
                        .map(|exp| ((exp - unix_ms()) / 1000).to_string())
                        .unwrap_or_else(|| "unknown".into())
                )),
            }
        }
    }
    lines
}

/// Desktop's `config.json`, for before/after change checks (metadata only).
pub fn config_path() -> Option<PathBuf> {
    find_data_dir().map(|dir| dir.join("config.json"))
}

fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// Desktop's data directory: classic `%APPDATA%\Claude`, the MSIX package's
/// virtualized Roaming folder, or `%LOCALAPPDATA%\Claude-Data\*`. The freshest
/// `config.json` wins. Resolved on every call because updates move it.
fn find_data_dir() -> Option<PathBuf> {
    let mut roots = Vec::new();
    if let Some(appdata) = std::env::var_os("APPDATA") {
        roots.push(PathBuf::from(appdata).join("Claude"));
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
        for (parent, prefix, suffix) in [
            (
                local.join("Packages"),
                Some("Claude_"),
                Some(["LocalCache", "Roaming", "Claude"]),
            ),
            (local.join("Claude-Data"), None, None),
        ] {
            let Ok(children) = std::fs::read_dir(&parent) else {
                continue;
            };
            for child in children.flatten() {
                let name = child.file_name();
                if prefix.is_some_and(|prefix| !name.to_string_lossy().starts_with(prefix)) {
                    continue;
                }
                let mut path = child.path();
                for part in suffix.iter().flatten() {
                    path.push(part);
                }
                roots.push(path);
            }
        }
    }
    roots
        .into_iter()
        .filter(|dir| dir.join("config.json").is_file() && dir.join("Local State").is_file())
        .max_by_key(|dir| {
            std::fs::metadata(dir.join("config.json"))
                .and_then(|meta| meta.modified())
                .unwrap_or(UNIX_EPOCH)
        })
}

/// Entries of the decrypted cache, each with `token` and `expiresAt`. A
/// `token` counts only beside an expiry or refresh-token field, so a stray
/// property elsewhere cannot pass for the access token.
fn parse_token_cache(root: &Value) -> Vec<DesktopToken> {
    let mut found = Vec::new();
    walk(root, None, &mut found);
    found
}

fn walk(node: &Value, key: Option<&str>, found: &mut Vec<DesktopToken>) {
    match node {
        Value::Object(map) => {
            let mut token = None;
            let mut expires_at_ms = None;
            let mut corroborated = false;
            for (name, value) in map {
                match name.to_ascii_lowercase().as_str() {
                    "token" | "accesstoken" | "access_token" => {
                        token = value.as_str().filter(|t| !t.is_empty())
                    }
                    "expiresat" | "expires_at" => {
                        expires_at_ms = epoch_ms(value);
                        corroborated = true;
                    }
                    "refreshtoken" | "refresh_token" => corroborated = true,
                    _ => {}
                }
            }
            if let (Some(token), true) = (token, corroborated) {
                let parts = key.map(parse_cache_key).unwrap_or_default();
                found.push(DesktopToken {
                    access_token: token.to_string(),
                    expires_at_ms,
                    account_uuid: parts.account_uuid,
                    org_uuid: parts.org_uuid,
                    profile_scope: parts.profile_scope,
                });
            }
            for (name, value) in map {
                if value.is_object() || value.is_array() {
                    walk(value, Some(name), found);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                walk(item, key, found);
            }
        }
        _ => {}
    }
}

#[derive(Default)]
struct KeyParts {
    account_uuid: Option<String>,
    org_uuid: Option<String>,
    profile_scope: bool,
}

/// Parse a token-cache key as Claude Desktop builds it (verified in the
/// Desktop 4.4212 bundle, `OVn`/`Sj`/`Cj`):
///
/// `acct:<accountUuid>|<clientId>:<orgId>:<apiHost>:<scope>`
///
/// Older keys omit the `acct:<accountUuid>|` prefix; their account is unknown.
/// The client id is a UUID too, so fields are taken by position, never by
/// "the first UUID". The scope (space-separated, itself containing colons)
/// follows the API host, whose URL also contains a colon.
fn parse_cache_key(key: &str) -> KeyParts {
    let (account_uuid, rest) = match key.strip_prefix("acct:") {
        Some(tail) => match tail.split_once('|') {
            Some((account, rest)) => (
                Some(account)
                    .filter(|a| looks_like_uuid(a))
                    .map(str::to_ascii_lowercase),
                rest,
            ),
            None => (None, tail),
        },
        None => (None, key),
    };
    let mut fields = rest.splitn(3, ':');
    let _client_id = fields.next();
    let org_uuid = fields
        .next()
        .filter(|org| looks_like_uuid(org))
        .map(str::to_ascii_lowercase);
    let host_and_scope = fields.next().unwrap_or_default();
    let profile_scope = host_and_scope
        .split_whitespace()
        .any(|scope| scope == "user:profile" || scope.ends_with(":user:profile"));
    KeyParts {
        account_uuid,
        org_uuid,
        profile_scope,
    }
}

/// Counts for the probe; booleans and counts only.
struct KeyStats {
    with_account: usize,
    accounts: usize,
    with_org: usize,
    account_entries: usize,
    org_entries: usize,
    profile_entries: usize,
}

impl KeyStats {
    fn of(entries: &[DesktopToken], expected: Expected<'_>) -> Self {
        let mut accounts: Vec<_> = entries
            .iter()
            .filter_map(|e| e.account_uuid.as_deref())
            .collect();
        accounts.sort_unstable();
        accounts.dedup();
        let account = expected.account_uuid.map(str::to_ascii_lowercase);
        let org = expected.org_uuid.map(str::to_ascii_lowercase);
        let for_account: Vec<_> = entries
            .iter()
            .filter(|e| account.is_some() && e.account_uuid == account)
            .collect();
        Self {
            with_account: entries.iter().filter(|e| e.account_uuid.is_some()).count(),
            accounts: accounts.len(),
            with_org: entries.iter().filter(|e| e.org_uuid.is_some()).count(),
            account_entries: for_account.len(),
            org_entries: for_account
                .iter()
                .filter(|e| org.is_some() && e.org_uuid == org)
                .count(),
            profile_entries: for_account.iter().filter(|e| e.profile_scope).count(),
        }
    }
}

fn looks_like_uuid(value: &str) -> bool {
    value.len() == 36
        && value.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

fn epoch_ms(value: &Value) -> Option<i64> {
    let number = value
        .as_i64()
        .or_else(|| value.as_f64().map(|v| v as i64))
        .or_else(|| value.as_str().and_then(|s| s.parse::<i64>().ok()))?;
    // Seconds and milliseconds are both seen in the wild.
    Some(if number > 100_000_000_000 {
        number
    } else {
        number.saturating_mul(1000)
    })
}

/// The longest-lived `user:profile` token for the CLI's account and, when
/// known, its organization. Only `acct:`-attributed entries are eligible.
/// Mismatches and ambiguity are refused, never guessed:
/// - no attributed entry at all: `AccountUnparsed`;
/// - attributed entries, none for the CLI account: `AccountMismatch`;
/// - the account only under other organizations: `OrganizationMismatch`;
/// - no CLI identity and several accounts or organizations: `Ambiguous`.
fn select_token(
    entries: &[DesktopToken],
    expected: Expected<'_>,
) -> Result<DesktopToken, Unavailable> {
    let attributed: Vec<&DesktopToken> = entries
        .iter()
        .filter(|e| e.account_uuid.is_some())
        .collect();
    if attributed.is_empty() {
        return Err(Unavailable::AccountUnparsed);
    }
    let distinct = |values: Vec<Option<&str>>| {
        let mut values = values;
        values.sort_unstable();
        values.dedup();
        values.len()
    };
    let mut candidates: Vec<&DesktopToken> = match expected.account_uuid {
        Some(account) => {
            let account = account.to_ascii_lowercase();
            let mine: Vec<_> = attributed
                .into_iter()
                .filter(|e| e.account_uuid.as_deref() == Some(account.as_str()))
                .collect();
            if mine.is_empty() {
                return Err(Unavailable::AccountMismatch);
            }
            mine
        }
        None => {
            // Unattributed entries could belong to anyone: with no CLI
            // identity, only a cache that is entirely one account qualifies.
            if attributed.len() != entries.len()
                || distinct(
                    attributed
                        .iter()
                        .map(|e| e.account_uuid.as_deref())
                        .collect(),
                ) != 1
            {
                return Err(Unavailable::Ambiguous);
            }
            attributed
        }
    };
    match expected.org_uuid {
        Some(org) => {
            let org = org.to_ascii_lowercase();
            candidates.retain(|e| e.org_uuid.as_deref() == Some(org.as_str()));
            if candidates.is_empty() {
                return Err(Unavailable::OrganizationMismatch);
            }
        }
        None => {
            if distinct(candidates.iter().map(|e| e.org_uuid.as_deref()).collect()) > 1 {
                return Err(Unavailable::Ambiguous);
            }
        }
    }
    candidates.retain(|e| e.profile_scope);
    candidates
        .into_iter()
        .max_by_key(|entry| entry.expires_at_ms.unwrap_or(i64::MIN))
        .cloned()
        .ok_or(Unavailable::NoProfileScope)
}

/// Standard or URL-safe base64, with or without padding.
pub(crate) fn decode_base64(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            b'\r' | b'\n' | b' ' => continue,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Some(out)
}

fn decrypt_os_crypt(blob: &[u8], local_state: &Path) -> Option<Vec<u8>> {
    if blob.len() >= 31 && blob.starts_with(b"v1") && matches!(blob[2], b'0' | b'1') {
        let key = master_key(local_state)?;
        let nonce = &blob[3..15];
        let (cipher, tag) = blob[15..].split_at(blob.len() - 15 - 16);
        aes_gcm_decrypt(&key, nonce, cipher, tag)
    } else {
        // Pre-v10 Chromium stored a raw DPAPI blob.
        dpapi_unprotect(blob)
    }
}

fn master_key(local_state: &Path) -> Option<Vec<u8>> {
    let state: Value = serde_json::from_slice(&std::fs::read(local_state).ok()?).ok()?;
    let wrapped = decode_base64(state.get("os_crypt")?.get("encrypted_key")?.as_str()?)?;
    let key = dpapi_unprotect(wrapped.strip_prefix(b"DPAPI")?)?;
    (key.len() == 32).then_some(key)
}

fn dpapi_unprotect(data: &[u8]) -> Option<Vec<u8>> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};
    let input = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(data.len()).ok()?,
        pbData: data.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptUnprotectData(&input, None, None, None, None, 0, &mut output).ok()?;
        let plain = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        let _ = LocalFree(HLOCAL(output.pbData.cast()));
        Some(plain)
    }
}

fn aes_gcm_decrypt(key: &[u8], nonce: &[u8], cipher: &[u8], tag: &[u8]) -> Option<Vec<u8>> {
    use windows::core::PCWSTR;
    use windows::Win32::Security::Cryptography::{
        BCryptCloseAlgorithmProvider, BCryptDecrypt, BCryptDestroyKey, BCryptGenerateSymmetricKey,
        BCryptOpenAlgorithmProvider, BCryptSetProperty, BCRYPT_AES_ALGORITHM, BCRYPT_ALG_HANDLE,
        BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO, BCRYPT_CHAINING_MODE, BCRYPT_CHAIN_MODE_GCM,
        BCRYPT_FLAGS, BCRYPT_HANDLE, BCRYPT_KEY_HANDLE, BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS,
    };
    unsafe {
        let mut alg = BCRYPT_ALG_HANDLE::default();
        if BCryptOpenAlgorithmProvider(
            &mut alg,
            BCRYPT_AES_ALGORITHM,
            PCWSTR::null(),
            BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS(0),
        )
        .is_err()
        {
            return None;
        }
        let result = (|| {
            let mode: Vec<u8> = BCRYPT_CHAIN_MODE_GCM
                .as_wide()
                .iter()
                .chain(std::iter::once(&0u16))
                .flat_map(|unit| unit.to_le_bytes())
                .collect();
            BCryptSetProperty(BCRYPT_HANDLE(alg.0), BCRYPT_CHAINING_MODE, &mode, 0)
                .ok()
                .ok()?;
            let mut handle = BCRYPT_KEY_HANDLE::default();
            BCryptGenerateSymmetricKey(alg, &mut handle, None, key, 0)
                .ok()
                .ok()?;
            let mut nonce = nonce.to_vec();
            let mut tag = tag.to_vec();
            let info = BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO {
                cbSize: std::mem::size_of::<BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO>() as u32,
                dwInfoVersion: 1,
                pbNonce: nonce.as_mut_ptr(),
                cbNonce: nonce.len() as u32,
                pbTag: tag.as_mut_ptr(),
                cbTag: tag.len() as u32,
                ..Default::default()
            };
            let mut plain = vec![0u8; cipher.len()];
            let mut written = 0u32;
            let status = BCryptDecrypt(
                handle,
                Some(cipher),
                Some(&info as *const _ as *const std::ffi::c_void),
                None,
                Some(&mut plain),
                &mut written,
                BCRYPT_FLAGS(0),
            );
            let _ = BCryptDestroyKey(handle);
            status.ok().ok()?;
            plain.truncate(written as usize);
            Some(plain)
        })();
        let _ = BCryptCloseAlgorithmProvider(alg, 0);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ACCOUNT: &str = "00000000-0000-4000-8000-000000000001";
    const OTHER: &str = "00000000-0000-4000-8000-000000000002";

    #[test]
    fn base64_decodes_standard_and_url_safe_alphabets() {
        assert_eq!(decode_base64("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(decode_base64("aGVsbG8").unwrap(), b"hello");
        assert_eq!(
            decode_base64("-_8=").unwrap(),
            decode_base64("+/8=").unwrap()
        );
        assert!(decode_base64("not*base64").is_none());
    }

    // Synthetic identifiers. The client id is a UUID as in the real format,
    // so a parser that takes "the first UUID" would mistake it for the account.
    const CLIENT: &str = "11111111-1111-4111-8111-111111111111";
    const ORG: &str = "22222222-2222-4222-8222-222222222222";
    const OTHER_ORG: &str = "33333333-3333-4333-8333-333333333333";
    const HOST: &str = "https://api.example.invalid";
    const SCOPE: &str = "user:profile user:inference user:sessions:claude_code";

    fn key(account: Option<&str>, org: &str, scope: &str) -> String {
        let base = format!("{CLIENT}:{org}:{HOST}:{scope}");
        match account {
            Some(account) => format!("acct:{account}|{base}"),
            None => base,
        }
    }

    fn entry(token: &str, expires: i64) -> Value {
        json!({"token": token, "refreshToken": "synthetic-refresh", "expiresAt": expires})
    }

    fn expect<'a>(account: Option<&'a str>, org: Option<&'a str>) -> Expected<'a> {
        Expected {
            account_uuid: account,
            org_uuid: org,
        }
    }

    #[test]
    fn keys_are_parsed_by_position_not_by_the_first_uuid() {
        let parts = parse_cache_key(&key(Some(ACCOUNT), ORG, SCOPE));
        assert_eq!(parts.account_uuid.as_deref(), Some(ACCOUNT));
        assert_eq!(parts.org_uuid.as_deref(), Some(ORG));
        assert!(parts.profile_scope);
        // A legacy key has no account even though it holds two UUIDs.
        let legacy = parse_cache_key(&key(None, ORG, SCOPE));
        assert_eq!(legacy.account_uuid, None);
        assert_eq!(legacy.org_uuid.as_deref(), Some(ORG));
        // Upper-case identifiers compare equal; malformed prefixes are not accounts.
        let upper = parse_cache_key(&key(Some(&ACCOUNT.to_uppercase()), ORG, SCOPE));
        assert_eq!(upper.account_uuid.as_deref(), Some(ACCOUNT));
        assert_eq!(
            parse_cache_key("acct:not-a-uuid|x:y:z:w").account_uuid,
            None
        );
        assert_eq!(
            parse_cache_key(&format!("acct:{ACCOUNT}")).account_uuid,
            None
        );
        // Scope detection survives the colon in the host URL.
        assert!(!parse_cache_key(&key(Some(ACCOUNT), ORG, "user:inference")).profile_scope);
        assert!(
            parse_cache_key(&key(Some(ACCOUNT), ORG, "user:profile org:desktop_config"))
                .profile_scope
        );
    }

    #[test]
    fn token_cache_entries_need_corroborating_fields_and_keep_their_identity() {
        let cache = json!({
            key(Some(ACCOUNT), ORG, SCOPE): entry("synthetic-access", 1_900_000_000_000),
            "unrelated": {"token": "not-an-oauth-token"},
            key(Some(OTHER), ORG, SCOPE): {"token": "synthetic-other", "expiresAt": 1_800_000_000}
        });
        let entries = parse_token_cache(&cache);
        assert_eq!(entries.len(), 2);
        let mine = entries
            .iter()
            .find(|e| e.account_uuid.as_deref() == Some(ACCOUNT))
            .unwrap();
        assert_eq!(mine.access_token, "synthetic-access");
        assert_eq!(mine.org_uuid.as_deref(), Some(ORG));
        let other = entries
            .iter()
            .find(|e| e.account_uuid.as_deref() == Some(OTHER))
            .unwrap();
        assert_eq!(
            other.expires_at_ms,
            Some(1_800_000_000_000),
            "seconds become ms"
        );
        let debug = format!("{mine:?}");
        assert!(!debug.contains("synthetic") && !debug.contains(ACCOUNT) && !debug.contains(ORG));
    }

    #[test]
    fn the_matching_account_and_organization_is_selected() {
        // The probe's real shape: three entries, one account, client id first.
        let cache = json!({
            key(Some(ACCOUNT), ORG, SCOPE): entry("t-chat", 30),
            key(Some(ACCOUNT), ORG, "user:profile org:desktop_config"): entry("t-config", 20),
            key(Some(ACCOUNT), ORG, "user:inference"): entry("t-no-profile", 99),
        });
        let entries = parse_token_cache(&cache);
        let chosen = select_token(&entries, expect(Some(ACCOUNT), Some(ORG))).unwrap();
        assert_eq!(
            chosen.access_token, "t-chat",
            "longest-lived user:profile entry"
        );
        // The CLI may report identifiers in another case.
        let upper = ACCOUNT.to_uppercase();
        assert!(select_token(&entries, expect(Some(&upper), Some(ORG))).is_ok());
        // Without an organization, a single-organization account is accepted.
        assert!(select_token(&entries, expect(Some(ACCOUNT), None)).is_ok());
    }

    #[test]
    fn mismatched_accounts_and_organizations_are_refused() {
        let cache = json!({
            key(Some(OTHER), ORG, SCOPE): entry("t-other", 50),
            key(Some(ACCOUNT), OTHER_ORG, SCOPE): entry("t-other-org", 40),
        });
        let entries = parse_token_cache(&cache);
        let stranger = "00000000-0000-4000-8000-00000000000f";
        assert_eq!(
            select_token(&entries, expect(Some(stranger), Some(ORG))).unwrap_err(),
            Unavailable::AccountMismatch
        );
        assert_eq!(
            select_token(&entries, expect(Some(ACCOUNT), Some(ORG))).unwrap_err(),
            Unavailable::OrganizationMismatch,
            "the CLI's account under another organization is not its usage"
        );
        assert_eq!(
            select_token(&entries, expect(Some(OTHER), Some(ORG)))
                .unwrap()
                .access_token,
            "t-other",
            "different accounts stay distinguishable"
        );
    }

    #[test]
    fn ambiguous_or_unattributed_caches_are_refused_rather_than_guessed() {
        let two_accounts = parse_token_cache(&json!({
            key(Some(ACCOUNT), ORG, SCOPE): entry("a", 1),
            key(Some(OTHER), ORG, SCOPE): entry("b", 2),
        }));
        assert_eq!(
            select_token(&two_accounts, expect(None, None)).unwrap_err(),
            Unavailable::Ambiguous
        );
        let two_orgs = parse_token_cache(&json!({
            key(Some(ACCOUNT), ORG, SCOPE): entry("a", 1),
            key(Some(ACCOUNT), OTHER_ORG, SCOPE): entry("b", 2),
        }));
        assert_eq!(
            select_token(&two_orgs, expect(Some(ACCOUNT), None)).unwrap_err(),
            Unavailable::Ambiguous
        );
        let legacy_only = parse_token_cache(&json!({
            key(None, ORG, SCOPE): entry("legacy", 1),
        }));
        for identity in [expect(Some(ACCOUNT), Some(ORG)), expect(None, None)] {
            assert_eq!(
                select_token(&legacy_only, identity).unwrap_err(),
                Unavailable::AccountUnparsed,
                "a parsing gap is reported as such, not as a mismatch"
            );
        }
        let mixed = parse_token_cache(&json!({
            key(Some(ACCOUNT), ORG, SCOPE): entry("attributed", 1),
            key(None, ORG, SCOPE): entry("legacy", 2),
        }));
        assert_eq!(
            select_token(&mixed, expect(Some(ACCOUNT), Some(ORG)))
                .unwrap()
                .access_token,
            "attributed",
            "an unattributed entry is never used for the CLI account"
        );
        assert_eq!(
            select_token(&mixed, expect(None, None)).unwrap_err(),
            Unavailable::Ambiguous
        );
        let single = parse_token_cache(&json!({ key(Some(OTHER), ORG, SCOPE): entry("only", 5) }));
        assert_eq!(
            select_token(&single, expect(None, None))
                .unwrap()
                .access_token,
            "only"
        );
        let no_profile = parse_token_cache(&json!({
            key(Some(ACCOUNT), ORG, "user:inference"): entry("np", 5),
        }));
        assert_eq!(
            select_token(&no_profile, expect(Some(ACCOUNT), Some(ORG))).unwrap_err(),
            Unavailable::NoProfileScope
        );
    }

    #[test]
    fn probe_statistics_hold_counts_and_booleans_only() {
        let entries = parse_token_cache(&json!({
            key(Some(ACCOUNT), ORG, SCOPE): entry("a", 1),
            key(Some(ACCOUNT), OTHER_ORG, "user:inference"): entry("b", 2),
            key(None, ORG, SCOPE): entry("c", 3),
        }));
        let stats = KeyStats::of(&entries, expect(Some(ACCOUNT), Some(ORG)));
        assert_eq!(
            (stats.with_account, stats.accounts, stats.with_org),
            (2, 1, 3)
        );
        assert_eq!(
            (
                stats.account_entries,
                stats.org_entries,
                stats.profile_entries
            ),
            (2, 1, 1)
        );
        let mismatch = KeyStats::of(&entries, expect(Some(OTHER), Some(ORG)));
        assert_eq!(mismatch.account_entries, 0);
    }

    #[test]
    fn dpapi_and_gcm_round_trip_with_a_synthetic_key() {
        use windows::Win32::Foundation::{LocalFree, HLOCAL};
        use windows::Win32::Security::Cryptography::{CryptProtectData, CRYPT_INTEGER_BLOB};
        let secret = b"synthetic dpapi payload".to_vec();
        let input = CRYPT_INTEGER_BLOB {
            cbData: secret.len() as u32,
            pbData: secret.as_ptr() as *mut u8,
        };
        let mut output = CRYPT_INTEGER_BLOB::default();
        let protected = unsafe {
            CryptProtectData(&input, None, None, None, None, 0, &mut output).unwrap();
            let bytes = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
            let _ = LocalFree(HLOCAL(output.pbData.cast()));
            bytes
        };
        assert_eq!(dpapi_unprotect(&protected).unwrap(), secret);
        // A wrong tag must fail authentication rather than return garbage.
        assert!(aes_gcm_decrypt(&[7u8; 32], &[1u8; 12], b"ciphertext", &[0u8; 16]).is_none());
    }

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn gcm_matches_the_nist_aes256_known_answer_and_os_crypt_layout() {
        // NIST GCM test case 14: K = 0^256, IV = 0^96, P = 0^128.
        let cipher = hex("cea7403d4d606b6e074ec5d3baf39d18");
        let tag = hex("d0d1c8a799996bf0265b98b5d48ab919");
        assert_eq!(
            aes_gcm_decrypt(&[0u8; 32], &[0u8; 12], &cipher, &tag).unwrap(),
            vec![0u8; 16]
        );
        // The same vector framed as an os_crypt "v10" blob.
        let mut blob = b"v10".to_vec();
        blob.extend_from_slice(&[0u8; 12]);
        blob.extend_from_slice(&cipher);
        blob.extend_from_slice(&tag);
        let (c, t) = blob[15..].split_at(blob.len() - 15 - 16);
        assert_eq!((c, t), (cipher.as_slice(), tag.as_slice()));
    }
}
