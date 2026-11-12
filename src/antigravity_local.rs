//! Antigravity's own language server, reached on loopback.
//!
//! Antigravity's quota panel (Gemini Models, Claude and GPT models, Gemini 4
//! Argon, each with its 5-hour and weekly windows) comes from the language
//! server it starts. That server answers `RetrieveUserQuotaSummary` on
//! 127.0.0.1 when a request carries the CSRF token Antigravity passed on its
//! command line. Approved by the owner on 2026-11-12.
//!
//! Read-only and model-free: one quota request per poll, only to 127.0.0.1,
//! only to a process whose image is Antigravity's bundled language server.
//! The token is read from that process's command line for each poll, sent
//! only in that loopback request, and never logged, cached or stored.

use crate::diagnose;
use std::ffi::c_void;
use std::sync::Mutex;
use std::time::Duration;

const METHOD: &str = "exa.language_server_pb.LanguageServerService/RetrieveUserQuotaSummary";
const MAX_RESPONSE_BYTES: u64 = 1 << 20;

const TH32CS_SNAPPROCESS: u32 = 0x2;
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
const PROCESS_COMMAND_LINE_INFORMATION: u32 = 60;
const AF_INET: u32 = 2;
const TCP_TABLE_OWNER_PID_LISTENER: u32 = 3;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
/// 127.0.0.1 as `MIB_TCPROW_OWNER_PID::dwLocalAddr` stores it.
const LOOPBACK_V4: u32 = u32::from_ne_bytes([127, 0, 0, 1]);

#[repr(C)]
struct ProcessEntry32W {
    size: u32,
    usage: u32,
    process_id: u32,
    default_heap_id: usize,
    module_id: u32,
    threads: u32,
    parent_process_id: u32,
    priority_class_base: i32,
    flags: u32,
    exe_file: [u16; 260],
}

#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *const u16,
}

#[link(name = "kernel32")]
extern "system" {
    fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> isize;
    fn Process32FirstW(snapshot: isize, entry: *mut ProcessEntry32W) -> i32;
    fn Process32NextW(snapshot: isize, entry: *mut ProcessEntry32W) -> i32;
    fn OpenProcess(access: u32, inherit: i32, process_id: u32) -> isize;
    fn QueryFullProcessImageNameW(
        process: isize,
        flags: u32,
        name: *mut u16,
        size: *mut u32,
    ) -> i32;
    fn CloseHandle(handle: isize) -> i32;
}

#[link(name = "ntdll")]
extern "system" {
    fn NtQueryInformationProcess(
        process: isize,
        class: u32,
        information: *mut c_void,
        length: u32,
        return_length: *mut u32,
    ) -> i32;
}

#[link(name = "iphlpapi")]
extern "system" {
    fn GetExtendedTcpTable(
        table: *mut c_void,
        size: *mut u32,
        order: i32,
        family: u32,
        class: u32,
        reserved: u32,
    ) -> u32;
}

/// The port that answered last time, tried first.
static LAST_PORT: Mutex<Option<(u32, u16)>> = Mutex::new(None);
static LAST_STATE: Mutex<Option<String>> = Mutex::new(None);

/// The raw `RetrieveUserQuotaSummary` response body from a running
/// Antigravity, or `None` when Antigravity (or its server) is not available.
pub fn retrieve_user_quota_summary() -> Option<String> {
    let servers = antigravity_servers();
    if servers.is_empty() {
        log_state("Antigravity language server not running; using Google's quota service");
        return None;
    }
    let agent = ureq::AgentBuilder::new()
        .try_proxy_from_env(false)
        .timeout_connect(Duration::from_millis(1_000))
        .timeout(Duration::from_secs(4))
        .redirects(0)
        .build();
    let cached = *LAST_PORT.lock().unwrap_or_else(|e| e.into_inner());

    for (pid, token) in &servers {
        let mut ports = loopback_listen_ports(*pid);
        if let Some((_, port)) = cached.filter(|(cached_pid, _)| cached_pid == pid) {
            if let Some(index) = ports.iter().position(|p| *p == port) {
                ports.swap(0, index);
            }
        }
        for port in ports {
            let response = agent
                .post(&format!("http://127.0.0.1:{port}/{METHOD}"))
                .set("X-Codeium-Csrf-Token", token)
                .set("Connect-Protocol-Version", "1")
                .set("Content-Type", "application/json")
                .send_string("{}");
            let Ok(response) = response else { continue };
            let mut body = String::new();
            use std::io::Read;
            if response
                .into_reader()
                .take(MAX_RESPONSE_BYTES)
                .read_to_string(&mut body)
                .is_err()
            {
                continue;
            }
            *LAST_PORT.lock().unwrap_or_else(|e| e.into_inner()) = Some((*pid, port));
            log_state(&format!(
                "Antigravity quota from Antigravity's language server (loopback port {port})"
            ));
            return Some(body);
        }
    }
    log_state("Antigravity language server did not answer; using Google's quota service");
    None
}

fn log_state(message: &str) {
    let mut last = LAST_STATE.lock().unwrap_or_else(|e| e.into_inner());
    if last.as_deref() != Some(message) {
        diagnose::log(message);
        *last = Some(message.to_string());
    }
}

/// Running Antigravity language servers with their CSRF tokens.
fn antigravity_servers() -> Vec<(u32, String)> {
    let mut servers = Vec::new();
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == -1 || snapshot == 0 {
            return servers;
        }
        let mut entry: ProcessEntry32W = std::mem::zeroed();
        entry.size = std::mem::size_of::<ProcessEntry32W>() as u32;
        let mut more = Process32FirstW(snapshot, &mut entry) != 0;
        while more {
            let len = entry.exe_file.iter().position(|c| *c == 0).unwrap_or(260);
            let name = String::from_utf16_lossy(&entry.exe_file[..len]);
            if is_language_server_name(&name) {
                if let Some(token) = server_token(entry.process_id) {
                    servers.push((entry.process_id, token));
                }
            }
            more = Process32NextW(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
    }
    servers
}

fn is_language_server_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.starts_with("language_server") && name.ends_with(".exe")
}

/// Whether an image path is Antigravity's bundled language server (and not,
/// for example, another Codeium-based editor's).
fn is_antigravity_server_path(path: &str) -> bool {
    let path = path.replace('/', "\\").to_ascii_lowercase();
    let Some((dir, file)) = path.rsplit_once('\\') else {
        return false;
    };
    is_language_server_name(file) && dir.ends_with("\\antigravity\\resources\\bin")
}

unsafe fn server_token(pid: u32) -> Option<String> {
    let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
    if process == 0 {
        return None;
    }
    let token = (|| {
        let mut path = [0u16; 1024];
        let mut size = path.len() as u32;
        if QueryFullProcessImageNameW(process, 0, path.as_mut_ptr(), &mut size) == 0 {
            return None;
        }
        if !is_antigravity_server_path(&String::from_utf16_lossy(&path[..size as usize])) {
            return None;
        }
        csrf_token_from_command_line(&command_line(process)?)
    })();
    CloseHandle(process);
    token
}

unsafe fn command_line(process: isize) -> Option<String> {
    let mut needed = 0u32;
    NtQueryInformationProcess(
        process,
        PROCESS_COMMAND_LINE_INFORMATION,
        std::ptr::null_mut(),
        0,
        &mut needed,
    );
    if needed < std::mem::size_of::<UnicodeString>() as u32 || needed > 1 << 20 {
        return None;
    }
    // u64 storage keeps the UNICODE_STRING header aligned.
    let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
    let status = NtQueryInformationProcess(
        process,
        PROCESS_COMMAND_LINE_INFORMATION,
        buffer.as_mut_ptr().cast(),
        (buffer.len() * 8) as u32,
        &mut needed,
    );
    if status < 0 {
        return None;
    }
    let header = &*(buffer.as_ptr() as *const UnicodeString);
    let start = buffer.as_ptr() as usize;
    let end = start + buffer.len() * 8;
    let text = header.buffer as usize;
    let bytes = header.length as usize;
    if header.buffer.is_null() || text < start || text + bytes > end {
        return None;
    }
    Some(String::from_utf16_lossy(std::slice::from_raw_parts(
        header.buffer,
        bytes / 2,
    )))
}

/// The `--csrf_token` value (`--csrf_token X` or `--csrf_token=X`), only
/// when it is a plain identifier.
fn csrf_token_from_command_line(command_line: &str) -> Option<String> {
    let mut parts = command_line.split_whitespace();
    while let Some(part) = parts.next() {
        let value = if part == "--csrf_token" {
            parts.next()?
        } else if let Some(value) = part.strip_prefix("--csrf_token=") {
            value
        } else {
            continue;
        };
        let value = value.trim_matches('"');
        let plain = !value.is_empty()
            && value.len() <= 256
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        return plain.then(|| value.to_string());
    }
    None
}

/// IPv4 loopback ports the process listens on.
fn loopback_listen_ports(pid: u32) -> Vec<u16> {
    for _ in 0..3 {
        let mut size = 0u32;
        unsafe {
            GetExtendedTcpTable(
                std::ptr::null_mut(),
                &mut size,
                0,
                AF_INET,
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            );
        }
        if size == 0 || size > 16 << 20 {
            return Vec::new();
        }
        let mut table = vec![0u32; (size as usize).div_ceil(4) + 6];
        let mut size = (table.len() * 4) as u32;
        let result = unsafe {
            GetExtendedTcpTable(
                table.as_mut_ptr().cast(),
                &mut size,
                0,
                AF_INET,
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            )
        };
        match result {
            0 => return ports_from_tcp_table(&table, pid),
            ERROR_INSUFFICIENT_BUFFER => continue,
            _ => return Vec::new(),
        }
    }
    Vec::new()
}

/// `MIB_TCPTABLE_OWNER_PID`: a count, then rows of state, local address,
/// local port (network order), remote address, remote port and owner PID.
fn ports_from_tcp_table(table: &[u32], pid: u32) -> Vec<u16> {
    let Some((&count, rows)) = table.split_first() else {
        return Vec::new();
    };
    let mut ports: Vec<u16> = rows
        .chunks_exact(6)
        .take(count as usize)
        .filter(|row| row[5] == pid && row[1] == LOOPBACK_V4)
        .map(|row| u16::from_be(row[2] as u16))
        .filter(|port| *port != 0)
        .collect();
    ports.sort_unstable();
    ports.dedup();
    ports
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_csrf_token_is_read_from_either_flag_form_and_must_be_plain() {
        let id = "0a1b2c3d-0000-4000-8000-123456789abc";
        assert_eq!(
            csrf_token_from_command_line(&format!(
                "language_server.exe --standalone --csrf_token {id} --app_data_dir x"
            ))
            .as_deref(),
            Some(id)
        );
        assert_eq!(
            csrf_token_from_command_line(&format!("x --csrf_token=\"{id}\"")).as_deref(),
            Some(id)
        );
        assert_eq!(csrf_token_from_command_line("x --csrf_token"), None);
        assert_eq!(csrf_token_from_command_line("x --csrf_token a;b"), None);
        assert_eq!(csrf_token_from_command_line("x --csrf_tokens y"), None);
        assert_eq!(
            csrf_token_from_command_line("x --host_bridge_token=abc"),
            None,
            "other tokens are never used"
        );
    }

    #[test]
    fn only_antigravitys_bundled_language_server_qualifies() {
        assert!(is_antigravity_server_path(
            r"C:\Users\u\AppData\Local\Programs\Antigravity\resources\bin\language_server.exe"
        ));
        assert!(is_antigravity_server_path(
            r"D:\Apps\antigravity\resources\bin\language_server_windows_x64.exe"
        ));
        assert!(!is_antigravity_server_path(
            r"C:\Users\u\AppData\Local\Programs\Windsurf\resources\bin\language_server.exe"
        ));
        assert!(!is_antigravity_server_path(
            r"C:\Temp\antigravity\resources\bin\other.exe"
        ));
        assert!(!is_antigravity_server_path("language_server.exe"));
    }

    #[test]
    fn listening_ports_are_the_processs_loopback_ones() {
        let port = |p: u16| u32::from(p.to_be());
        let any = u32::from_ne_bytes([0, 0, 0, 0]);
        #[rustfmt::skip]
        let table = [
            4,
            2, LOOPBACK_V4, port(55975), 0, 0, 42,
            2, LOOPBACK_V4, port(55974), 0, 0, 42,
            2, any, port(8080), 0, 0, 42,
            2, LOOPBACK_V4, port(9000), 0, 0, 7,
            // Beyond the count: ignored.
            2, LOOPBACK_V4, port(1234), 0, 0, 42,
        ];
        assert_eq!(ports_from_tcp_table(&table, 42), vec![55974, 55975]);
        assert!(ports_from_tcp_table(&[], 42).is_empty());
    }
}
