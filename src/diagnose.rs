use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

const LOG_FILE_NAME: &str = "codex-usage.log";
const LOG_LIMIT_BYTES: u64 = 5 * 1024 * 1024;
const MAX_RECORD_BYTES: usize = 8 * 1024;

struct DiagnoseState {
    path: PathBuf,
    verbose: bool,
    lock: Mutex<()>,
}

static DIAGNOSE_STATE: OnceLock<DiagnoseState> = OnceLock::new();

/// Initialize bounded persistent operational logging. Failure is intentionally
/// non-fatal so diagnostics can never prevent the taskbar monitor from starting.
pub fn init(verbose: bool) -> Result<PathBuf, String> {
    let path = active_log_file_path()
        .ok_or_else(|| "Local application data directory is unavailable".to_string())?;
    let directory = path
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "Operational log directory is unavailable".to_string())?;
    fs::create_dir_all(&directory).map_err(|error| {
        format!(
            "Unable to create operational log directory {}: {error}",
            directory.display()
        )
    })?;

    rotate_if_needed(&path);
    let state = DiagnoseState {
        path: path.clone(),
        verbose,
        lock: Mutex::new(()),
    };
    let _ = DIAGNOSE_STATE.set(state);
    log("operational logging enabled");
    if verbose {
        log_verbose("extra-verbose diagnostics enabled");
    }
    Ok(path)
}

pub fn is_enabled() -> bool {
    DIAGNOSE_STATE.get().is_some()
}

pub fn active_log_file_path() -> Option<PathBuf> {
    let local_app_data = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(dirs::data_local_dir);
    resolve_active_log_file_path(
        DIAGNOSE_STATE.get().map(|state| state.path.as_path()),
        local_app_data.as_deref(),
    )
}

fn resolve_active_log_file_path(
    logger_path: Option<&Path>,
    local_app_data: Option<&Path>,
) -> Option<PathBuf> {
    logger_path
        .map(Path::to_path_buf)
        .or_else(|| local_app_data.map(log_file_path))
}

pub fn log_open_target(log_file: &Path, log_file_exists: bool) -> PathBuf {
    if log_file_exists {
        log_file.to_path_buf()
    } else {
        log_file.parent().unwrap_or(log_file).to_path_buf()
    }
}

pub fn log(message: impl AsRef<str>) {
    write_record(message.as_ref());
}

pub fn log_verbose(message: impl AsRef<str>) {
    if DIAGNOSE_STATE
        .get()
        .map(|state| state.verbose)
        .unwrap_or(false)
    {
        write_record(message.as_ref());
    }
}

pub fn log_error(context: &str, error: impl std::fmt::Display) {
    log(format!("{context}: {error}"));
}

fn write_record(message: &str) {
    let Some(state) = DIAGNOSE_STATE.get() else {
        return;
    };
    let Ok(_guard) = state.lock.lock() else {
        return;
    };

    append_record_to(&state.path, message);
}

fn log_file_path(local_app_data: &std::path::Path) -> PathBuf {
    log_directory_path(local_app_data).join(LOG_FILE_NAME)
}

fn log_directory_path(local_app_data: &std::path::Path) -> PathBuf {
    local_app_data.join("CodexUsage").join("logs")
}

fn append_record_to(path: &std::path::Path, message: &str) {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let message = truncate_record(message);
    let line = format!("[{timestamp}] {message}\n");
    rotate_if_record_would_exceed(path, line.len() as u64);
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(line.as_bytes());
        let _ = file.flush();
    }
}

fn truncate_record(message: &str) -> &str {
    if message.len() <= MAX_RECORD_BYTES {
        return message;
    }
    let mut end = MAX_RECORD_BYTES;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    &message[..end]
}

fn rotate_if_needed(path: &Path) {
    trim_previous(path);
    let len = fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    if len >= LOG_LIMIT_BYTES {
        rotate(path);
    }
}

fn rotate_if_record_would_exceed(path: &Path, record_len: u64) {
    let len = fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    if len.saturating_add(record_len) > LOG_LIMIT_BYTES {
        rotate(path);
    }
}

fn rotate(path: &Path) {
    let previous = path.with_extension("log.1");
    if previous.exists() && fs::remove_file(&previous).is_err() {
        truncate_file(path);
        truncate_file(&previous);
        return;
    }
    if fs::rename(path, &previous).is_err() {
        truncate_file(path);
    }
}

fn trim_previous(path: &Path) {
    let previous = path.with_extension("log.1");
    if fs::metadata(&previous).is_ok_and(|metadata| metadata.len() > LOG_LIMIT_BYTES)
        && fs::remove_file(&previous).is_err()
    {
        truncate_file(&previous);
    }
}

fn truncate_file(path: &std::path::Path) {
    let _ = OpenOptions::new().write(true).truncate(true).open(path);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_keeps_at_most_one_bounded_previous_log() {
        let base = std::env::temp_dir().join(format!(
            "codex-usage-log-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&base).unwrap();
        let path = base.join(LOG_FILE_NAME);
        fs::write(&path, vec![b'x'; LOG_LIMIT_BYTES as usize]).unwrap();
        rotate_if_needed(&path);
        assert!(!path.exists());
        assert_eq!(
            fs::metadata(path.with_extension("log.1")).unwrap().len(),
            LOG_LIMIT_BYTES
        );
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn logging_appends_across_calls_and_writes_nothing_to_console() {
        let base = std::env::temp_dir().join(format!(
            "codex-usage-log-append-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&base).unwrap();
        let path = base.join(LOG_FILE_NAME);
        append_record_to(&path, "startup event");
        append_record_to(&path, "poll event");
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("startup event"));
        assert!(contents.contains("poll event"));
        assert_eq!(contents.lines().count(), 2);
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn failed_log_writes_and_failed_rotation_fallback_are_nonfatal_and_bounded() {
        let base = std::env::temp_dir().join(format!(
            "codex-usage-log-failure-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&base).unwrap();
        let invalid_path = base.join("is-a-directory");
        fs::create_dir(&invalid_path).unwrap();
        append_record_to(&invalid_path, "write failure is ignored");

        let path = base.join(LOG_FILE_NAME);
        fs::write(&path, vec![b'x'; 32]).unwrap();
        let previous = path.with_extension("log.1");
        fs::create_dir(&previous).unwrap();
        fs::write(previous.join("keep-openable"), b"locked-retained").unwrap();
        rotate(&path);
        assert_eq!(fs::metadata(&path).unwrap().len(), 0);
        assert!(fs::metadata(&previous).unwrap().is_dir());
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn log_directory_is_under_local_app_data() {
        assert_eq!(
            log_file_path(std::path::Path::new(r"C:\Users\Example\AppData\Local")),
            PathBuf::from(r"C:\Users\Example\AppData\Local\CodexUsage\logs\codex-usage.log")
        );
    }

    #[test]
    fn log_open_target_selects_file_or_logs_folder_without_launching_a_program() {
        let log_file = Path::new(r"C:\Users\Example\AppData\Local\CodexUsage\logs\codex-usage.log");
        assert_eq!(log_open_target(log_file, true), log_file);
        assert_eq!(
            log_open_target(log_file, false),
            PathBuf::from(r"C:\Users\Example\AppData\Local\CodexUsage\logs")
        );
    }

    #[test]
    fn active_log_path_uses_the_exact_initialized_logger_path() {
        let logger_path =
            PathBuf::from(r"C:\Users\Example\AppData\Local\CodexUsage\logs\codex-usage.log");
        let other_appdata = Path::new(r"D:\different-profile\AppData\Local");
        assert_eq!(
            resolve_active_log_file_path(Some(&logger_path), Some(other_appdata)),
            Some(logger_path)
        );
    }

    #[test]
    fn record_truncation_is_utf8_safe_and_bounded() {
        let value = "é".repeat(MAX_RECORD_BYTES);
        let truncated = truncate_record(&value);
        assert!(truncated.len() <= MAX_RECORD_BYTES);
        assert!(truncated.is_char_boundary(truncated.len()));
    }
}
