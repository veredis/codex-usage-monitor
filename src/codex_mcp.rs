//! Optional, loopback-only MCP view of the latest Codex quota snapshot.
//! This module never polls the upstream usage endpoint or reads credentials.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::diagnose;
use crate::models::{CreditBalance, LunaReserveUsage, UsageData, UsageSection};
use crate::poller::remaining_percentage;

pub const MCP_PORT: u16 = 46827;
const MCP_PATH: &str = "/mcp";
const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_BODY_BYTES: usize = 64 * 1024;
const SOCKET_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
struct CachedSnapshot {
    data: UsageData,
    updated_at: SystemTime,
}

#[derive(Clone, Debug)]
struct PollingMetadata {
    mode: &'static str,
    effective_interval_seconds: u32,
}

impl Default for PollingMetadata {
    fn default() -> Self {
        Self {
            mode: "fixed",
            effective_interval_seconds: 900,
        }
    }
}

#[derive(Default)]
struct SharedUsage {
    snapshot: Option<CachedSnapshot>,
    claude_snapshot: Option<CachedSnapshot>,
    polling: PollingMetadata,
    codex_enabled: bool,
    claude_enabled: bool,
}

impl SharedUsage {
    fn set_monitoring_enabled(&mut self, codex: bool, claude: bool) {
        self.codex_enabled = codex;
        self.claude_enabled = claude;
        if !codex {
            self.snapshot = None;
        }
        if !claude {
            self.claude_snapshot = None;
        }
    }

    fn publish(&mut self, data: UsageData, claude: bool) {
        // A poll already in flight must not repopulate a disabled provider.
        if if claude {
            self.claude_enabled
        } else {
            self.codex_enabled
        } {
            let snapshot = Some(CachedSnapshot {
                data,
                updated_at: SystemTime::now(),
            });
            if claude {
                self.claude_snapshot = snapshot;
            } else {
                self.snapshot = snapshot;
            }
        }
    }
}

pub fn set_monitoring_enabled(codex: bool, claude: bool) {
    if let Ok(mut shared) = shared_usage().write() {
        shared.set_monitoring_enabled(codex, claude);
    }
}

fn shared_usage() -> &'static RwLock<SharedUsage> {
    static SHARED: OnceLock<RwLock<SharedUsage>> = OnceLock::new();
    SHARED.get_or_init(|| RwLock::new(SharedUsage::default()))
}

pub fn publish_snapshot(data: UsageData, adaptive: bool, interval_ms: u32) {
    let metadata = PollingMetadata {
        mode: if adaptive { "adaptive" } else { "fixed" },
        effective_interval_seconds: interval_ms.saturating_add(999) / 1000,
    };
    if let Ok(mut shared) = shared_usage().write() {
        shared.polling = metadata;
        shared.publish(data, false);
    }
}

pub fn publish_claude_snapshot(data: UsageData) {
    if let Ok(mut shared) = shared_usage().write() {
        shared.publish(data, true);
    }
}

pub fn update_polling_metadata(adaptive: bool, interval_ms: u32) {
    if let Ok(mut shared) = shared_usage().write() {
        shared.polling = PollingMetadata {
            mode: if adaptive { "adaptive" } else { "fixed" },
            effective_interval_seconds: interval_ms.saturating_add(999) / 1000,
        };
    }
}

pub fn start() -> Result<(), String> {
    let server = server_slot();
    let mut server = server
        .lock()
        .map_err(|_| "MCP state lock poisoned".to_string())?;
    if server.is_some() {
        return Ok(());
    }
    let listener = bind_loopback_listener(MCP_PORT)
        .map_err(|error| format!("could not bind 127.0.0.1:{MCP_PORT}: {error}"))?;
    *server = Some(start_listener(listener, MCP_PORT)?);
    diagnose::log(format!(
        "Codex MCP server listening at 127.0.0.1:{MCP_PORT}{MCP_PATH}"
    ));
    Ok(())
}

fn bind_loopback_listener(port: u16) -> std::io::Result<TcpListener> {
    TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
}

pub fn stop() {
    let slot = server_slot();
    let handle = slot.lock().ok().and_then(|mut server| server.take());
    if let Some(mut handle) = handle {
        handle.stop.store(true, Ordering::Release);
        if let Some(join) = handle.join.take() {
            let _ = join.join();
        }
        diagnose::log("Codex MCP server stopped");
    }
}

struct ServerHandle {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

fn server_slot() -> &'static Mutex<Option<ServerHandle>> {
    static SERVER: OnceLock<Mutex<Option<ServerHandle>>> = OnceLock::new();
    SERVER.get_or_init(|| Mutex::new(None))
}

fn start_listener(listener: TcpListener, port: u16) -> Result<ServerHandle, String> {
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("could not configure MCP listener: {error}"))?;
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let join = thread::Builder::new()
        .name("codex-usage-mcp".to_string())
        .spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => handle_connection(stream, port),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(50));
                    }
                    Err(error) => {
                        diagnose::log_error("Codex MCP listener stopped after accept error", error);
                        break;
                    }
                }
            }
        })
        .map_err(|error| format!("could not start MCP listener thread: {error}"))?;
    Ok(ServerHandle {
        stop,
        join: Some(join),
    })
}

struct HttpRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn handle_connection(mut stream: TcpStream, port: u16) {
    let _ = stream.set_read_timeout(Some(SOCKET_TIMEOUT));
    let _ = stream.set_write_timeout(Some(SOCKET_TIMEOUT));
    match read_http_request(&mut stream) {
        Ok(request) => handle_http_request(&mut stream, request, port),
        Err(()) => write_http_response(
            &mut stream,
            400,
            "Bad Request",
            "text/plain",
            b"bad request",
        ),
    }
}

fn read_http_request(stream: &mut TcpStream) -> Result<HttpRequest, ()> {
    let mut bytes = Vec::with_capacity(4096);
    let header_end = loop {
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
        if bytes.len() >= MAX_HEADER_BYTES {
            return Err(());
        }
        let mut buffer = [0u8; 2048];
        let count = stream.read(&mut buffer).map_err(|_| ())?;
        if count == 0 {
            return Err(());
        }
        bytes.extend_from_slice(&buffer[..count]);
    };
    if header_end > MAX_HEADER_BYTES {
        return Err(());
    }

    let header_text = std::str::from_utf8(&bytes[..header_end]).map_err(|_| ())?;
    let mut lines = header_text.split("\r\n");
    let mut request_line = lines.next().ok_or(())?.split_whitespace();
    let method = request_line.next().ok_or(())?.to_string();
    let path = request_line.next().ok_or(())?.to_string();
    if request_line.next().is_none() {
        return Err(());
    }
    let mut headers = Vec::new();
    let mut content_length = None;
    for line in lines {
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').ok_or(())?;
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim().to_string();
        if name == "transfer-encoding" {
            return Err(());
        }
        if name == "content-length" {
            content_length = Some(value.parse::<usize>().map_err(|_| ())?);
        }
        headers.push((name, value));
    }

    let content_length = content_length.unwrap_or(0);
    if content_length > MAX_BODY_BYTES {
        return Err(());
    }
    let target_len = header_end.checked_add(content_length).ok_or(())?;
    while bytes.len() < target_len {
        let mut buffer = [0u8; 4096];
        let count = stream.read(&mut buffer).map_err(|_| ())?;
        if count == 0 {
            return Err(());
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    Ok(HttpRequest {
        method,
        path,
        headers,
        body: bytes[header_end..target_len].to_vec(),
    })
}

fn handle_http_request(stream: &mut TcpStream, request: HttpRequest, port: u16) {
    let header = |name: &str| {
        request
            .headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };

    if let Some(host) = header("host") {
        if !valid_loopback_host(host, port) {
            write_http_response(stream, 403, "Forbidden", "text/plain", b"invalid host");
            return;
        }
    }
    if let Some(origin) = header("origin") {
        if !valid_loopback_origin(origin, port) {
            write_http_response(stream, 403, "Forbidden", "text/plain", b"invalid origin");
            return;
        }
    }
    if request.method != "POST" {
        write_http_response(
            stream,
            405,
            "Method Not Allowed",
            "text/plain",
            b"POST required",
        );
        return;
    }
    if request.path != MCP_PATH {
        write_http_response(stream, 404, "Not Found", "text/plain", b"not found");
        return;
    }
    if !header("content-type")
        .is_some_and(|value| value.to_ascii_lowercase().starts_with("application/json"))
    {
        write_http_response(
            stream,
            415,
            "Unsupported Media Type",
            "text/plain",
            b"JSON required",
        );
        return;
    }
    if !header("accept").is_some_and(|value| {
        let value = value.to_ascii_lowercase();
        value.contains("application/json") || value.contains("text/event-stream")
    }) {
        write_http_response(
            stream,
            406,
            "Not Acceptable",
            "text/plain",
            b"MCP Accept required",
        );
        return;
    }

    let Ok(request_value) = serde_json::from_slice::<Value>(&request.body) else {
        write_http_response(stream, 400, "Bad Request", "text/plain", b"invalid JSON");
        return;
    };
    match handle_rpc(&request_value) {
        None => write_http_response(stream, 202, "Accepted", "application/json", b""),
        Some(response) => {
            let body = serde_json::to_vec(&response).unwrap_or_else(|_| b"{}".to_vec());
            write_http_response(stream, 200, "OK", "application/json", &body);
        }
    }
}

fn valid_loopback_host(host: &str, port: u16) -> bool {
    host.eq_ignore_ascii_case(&format!("127.0.0.1:{port}"))
        || host.eq_ignore_ascii_case(&format!("localhost:{port}"))
}

fn valid_loopback_origin(origin: &str, port: u16) -> bool {
    origin.eq_ignore_ascii_case(&format!("http://127.0.0.1:{port}"))
        || origin.eq_ignore_ascii_case(&format!("http://localhost:{port}"))
}

fn write_http_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
) {
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn handle_rpc(request: &Value) -> Option<Value> {
    handle_rpc_at(request, SystemTime::now())
}

fn read_only_tool(name: &str, title: &str, description: &str) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {"type":"object","properties":{},"additionalProperties":false},
        "annotations": {
            "title": title,
            "readOnlyHint": true,
            "destructiveHint": false,
            "openWorldHint": false
        }
    })
}

fn handle_rpc_at(request: &Value, now: SystemTime) -> Option<Value> {
    let id = request.get("id")?;
    let id_copy = id.clone();
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let params = request.get("params").unwrap_or(&Value::Null);
    let result = match method {
        "initialize" => {
            let requested = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("2025-03-26");
            let protocol = match requested {
                "2025-11-25" | "2025-06-18" | "2025-03-26" => requested,
                _ => "2025-03-26",
            };
            json!({
                "protocolVersion": protocol,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {
                    "name": "codex-usage-monitor",
                    "version": env!("CARGO_PKG_VERSION")
                }
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({"tools": [
            read_only_tool("get_codex_usage", "Get Codex usage", "Read the latest cached Codex quota and credit snapshot. This tool never triggers a usage poll."),
            read_only_tool("get_claude_usage", "Get Claude usage", "Read the latest cached Claude quota snapshot. This tool never triggers a usage poll.")
        ]}),
        "tools/call" => {
            let arguments_supported = params.get("arguments").is_none_or(|arguments| {
                arguments.as_object().is_some_and(serde_json::Map::is_empty)
            });
            let tool_name = params.get("name").and_then(Value::as_str);
            if !matches!(tool_name, Some("get_codex_usage" | "get_claude_usage"))
                || !arguments_supported
            {
                return Some(json!({
                    "jsonrpc":"2.0",
                    "id":id_copy,
                    "error":{"code":-32602,"message":"Unknown tool or arguments are not supported"}
                }));
            }
            let (snapshot, polling) = shared_usage()
                .read()
                .map(|shared| {
                    (
                        if tool_name == Some("get_claude_usage") {
                            shared.claude_snapshot.clone()
                        } else {
                            shared.snapshot.clone()
                        },
                        shared.polling.clone(),
                    )
                })
                .unwrap_or((None, PollingMetadata::default()));
            let data = usage_snapshot_result(snapshot.as_ref(), &polling, now);
            let text = serde_json::to_string(&data).unwrap_or_else(|_| "{}".to_string());
            json!({
                "content":[{"type":"text","text":text}],
                "structuredContent":data,
                "isError":false
            })
        }
        _ => {
            return Some(json!({
                "jsonrpc":"2.0",
                "id":id_copy,
                "error":{"code":-32601,"message":"Method not found"}
            }));
        }
    };
    Some(json!({"jsonrpc":"2.0","id":id_copy,"result":result}))
}

fn usage_snapshot_result(
    snapshot: Option<&CachedSnapshot>,
    polling: &PollingMetadata,
    now: SystemTime,
) -> Value {
    let effective_interval = polling.effective_interval_seconds.max(1);
    let Some(snapshot) = snapshot else {
        return json!({
            "five_hour": null,
            "weekly": null,
            "credits": {"state":"unknown"},
            "luna_reserve": null,
            "lowest_remaining_percent": null,
            "limiting_window": null,
            "updated_at": null,
            "age_seconds": null,
            "polling_mode": polling.mode,
            "effective_poll_interval_seconds": effective_interval,
            "status":"unavailable"
        });
    };

    let age = now
        .duration_since(snapshot.updated_at)
        .unwrap_or_default()
        .as_secs();
    let stale_after_seconds = effective_interval.saturating_mul(2).max(60);
    let (lowest, limiting_window) = lowest_remaining(&snapshot.data);
    let credits = match snapshot.data.credits.as_ref() {
        Some(CreditBalance::Amount(amount)) => json!({"state":"amount","balance":amount}),
        Some(CreditBalance::Unlimited) => json!({"state":"unlimited"}),
        None => json!({"state":"unknown"}),
    };
    let luna_reserve = snapshot
        .data
        .luna_reserve
        .as_ref()
        .map(luna_reserve_json)
        .unwrap_or(Value::Null);
    let updated_at = snapshot
        .updated_at
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs());
    json!({
        "five_hour": section_json(&snapshot.data.session),
        "weekly": section_json(&snapshot.data.weekly),
        "credits": credits,
        "luna_reserve": luna_reserve,
        "lowest_remaining_percent": lowest,
        "limiting_window": limiting_window,
        "updated_at": updated_at,
        "age_seconds": age,
        "polling_mode": polling.mode,
        "effective_poll_interval_seconds": effective_interval,
        "status": if age > stale_after_seconds as u64 { "stale" } else { "ok" }
    })
}

fn luna_reserve_json(reserve: &LunaReserveUsage) -> Value {
    if !reserve.available || !reserve.section.available {
        return json!({"available":false,"active":reserve.active});
    }
    let used = reserve.section.percentage.clamp(0.0, 100.0);
    let reset = reserve
        .section
        .resets_at
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs());
    json!({
        "available": true,
        "active": reserve.active,
        "used_percent": used,
        "remaining_percent": remaining_percentage(used),
        "resets_at": reset
    })
}

fn section_json(section: &UsageSection) -> Value {
    if !section.available || !section.percentage.is_finite() {
        return Value::Null;
    }
    let used = section.percentage.clamp(0.0, 100.0);
    let reset = section
        .resets_at
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs());
    json!({
        "used_percent": used,
        "remaining_percent": remaining_percentage(used),
        "resets_at": reset
    })
}

fn lowest_remaining(data: &UsageData) -> (Option<f64>, Option<&'static str>) {
    let mut lowest: Option<(f64, &'static str)> = None;
    for (name, section) in [("five_hour", &data.session), ("weekly", &data.weekly)] {
        if !section.available || !section.percentage.is_finite() {
            continue;
        }
        let remaining = remaining_percentage(section.percentage);
        if lowest.is_none_or(|(current, _)| remaining < current) {
            lowest = Some((remaining, name));
        }
    }
    lowest
        .map(|(value, window)| (Some(value), Some(window)))
        .unwrap_or((None, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabling_provider_clears_cache_and_rejects_in_flight_publication() {
        let mut shared = SharedUsage::default();
        shared.set_monitoring_enabled(true, true);
        shared.publish(test_data(10.0, 20.0), false);
        shared.publish(test_data(30.0, 40.0), true);
        assert_eq!(
            shared
                .claude_snapshot
                .as_ref()
                .unwrap()
                .data
                .session
                .percentage,
            30.0
        );
        shared.set_monitoring_enabled(true, false);
        shared.publish(test_data(50.0, 60.0), true);
        assert!(shared.claude_snapshot.is_none());
        assert_eq!(
            shared.snapshot.as_ref().unwrap().data.session.percentage,
            10.0
        );
        assert_eq!(
            usage_snapshot_result(
                shared.claude_snapshot.as_ref(),
                &shared.polling,
                SystemTime::now()
            )["status"],
            "unavailable"
        );
        shared.set_monitoring_enabled(true, true);
        assert!(shared.claude_snapshot.is_none());
        shared.publish(test_data(70.0, 80.0), true);
        assert_eq!(
            shared
                .claude_snapshot
                .as_ref()
                .unwrap()
                .data
                .session
                .percentage,
            70.0
        );
        shared.set_monitoring_enabled(false, true);
        shared.publish(test_data(90.0, 90.0), false);
        assert!(shared.snapshot.is_none());
    }

    #[test]
    fn successful_snapshots_replace_reserve_object_with_null_and_back() {
        let mut shared = SharedUsage::default();
        shared.set_monitoring_enabled(true, false);
        for present in [false, true, false, true] {
            let mut data = test_data(10.0, 20.0);
            data.credits = Some(CreditBalance::Amount(333.704065));
            if present {
                data.luna_reserve = Some(LunaReserveUsage {
                    section: UsageSection {
                        percentage: 25.0,
                        available: true,
                        ..Default::default()
                    },
                    available: true,
                    active: None,
                });
            }
            shared.publish(data, false);
            let result =
                usage_snapshot_result(shared.snapshot.as_ref(), &shared.polling, SystemTime::now());
            assert_eq!(result["luna_reserve"].is_object(), present);
            assert_eq!(result["luna_reserve"].is_null(), !present);
            assert_eq!(result["credits"]["balance"], 333.704065);
            assert_eq!(result["five_hour"]["used_percent"], 10.0);
            assert_eq!(result["status"], "ok");
        }
    }

    fn cached(data: UsageData, age: Duration) -> CachedSnapshot {
        CachedSnapshot {
            data,
            updated_at: UNIX_EPOCH + Duration::from_secs(2_000_000_000) - age,
        }
    }

    fn test_data(session: f64, weekly: f64) -> UsageData {
        UsageData {
            session: UsageSection {
                percentage: session,
                resets_at: Some(UNIX_EPOCH + Duration::from_secs(2_000_100_000)),
                available: true,
            },
            weekly: UsageSection {
                percentage: weekly,
                resets_at: Some(UNIX_EPOCH + Duration::from_secs(2_000_200_000)),
                available: true,
            },
            credits: None,
            luna_reserve: None,
        }
    }

    #[test]
    fn tool_catalog_exposes_two_separate_read_only_tools() {
        let response = handle_rpc_at(
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
            SystemTime::now(),
        )
        .unwrap();
        let tools = response["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], "get_codex_usage");
        assert_eq!(tools[1]["name"], "get_claude_usage");
        assert_eq!(tools[0]["annotations"]["readOnlyHint"], true);
    }

    #[test]
    fn reports_unavailable_without_a_successful_snapshot() {
        let result = usage_snapshot_result(None, &PollingMetadata::default(), SystemTime::now());
        assert_eq!(result["status"], "unavailable");
        assert_eq!(result["five_hour"], Value::Null);
        assert_eq!(result["credits"]["state"], "unknown");
    }

    #[test]
    fn reports_fresh_stale_and_effective_polling_metadata() {
        let snapshot = cached(test_data(20.0, 60.0), Duration::from_secs(59));
        let polling = PollingMetadata {
            mode: "adaptive",
            effective_interval_seconds: 30,
        };
        let now = UNIX_EPOCH + Duration::from_secs(2_000_000_000);
        let result = usage_snapshot_result(Some(&snapshot), &polling, now);
        assert_eq!(result["status"], "ok");
        assert_eq!(result["polling_mode"], "adaptive");
        assert_eq!(result["effective_poll_interval_seconds"], 30);
        let old = cached(test_data(20.0, 60.0), Duration::from_secs(61));
        assert_eq!(
            usage_snapshot_result(Some(&old), &polling, now)["status"],
            "stale"
        );
    }

    #[test]
    fn maps_windows_remaining_percentages_and_limiting_window() {
        let data = test_data(18.0, 74.0);
        let result = usage_snapshot_result(
            Some(&cached(data, Duration::ZERO)),
            &PollingMetadata::default(),
            UNIX_EPOCH + Duration::from_secs(2_000_000_000),
        );
        assert_eq!(result["five_hour"]["used_percent"], 18.0);
        assert_eq!(result["five_hour"]["remaining_percent"], 82.0);
        assert_eq!(result["weekly"]["used_percent"], 74.0);
        assert_eq!(result["weekly"]["remaining_percent"], 26.0);
        assert_eq!(result["lowest_remaining_percent"], 26.0);
        assert_eq!(result["limiting_window"], "weekly");
    }

    #[test]
    fn preserves_finite_unlimited_and_unknown_credit_states() {
        for (credits, expected) in [
            (Some(CreditBalance::Amount(12.5)), "amount"),
            (Some(CreditBalance::Unlimited), "unlimited"),
            (None, "unknown"),
        ] {
            let mut data = test_data(0.0, 0.0);
            data.credits = credits;
            let result = usage_snapshot_result(
                Some(&cached(data, Duration::ZERO)),
                &PollingMetadata::default(),
                UNIX_EPOCH + Duration::from_secs(2_000_000_000),
            );
            assert_eq!(result["credits"]["state"], expected);
        }
    }

    #[test]
    fn exposes_luna_reserve_without_promoting_unknown_active_state() {
        let mut data = test_data(100.0, 100.0);
        data.luna_reserve = Some(LunaReserveUsage {
            section: UsageSection {
                percentage: 50.0,
                resets_at: Some(UNIX_EPOCH + Duration::from_secs(2_000_300_000)),
                available: true,
            },
            available: true,
            active: None,
        });
        let result = usage_snapshot_result(
            Some(&cached(data, Duration::ZERO)),
            &PollingMetadata::default(),
            UNIX_EPOCH + Duration::from_secs(2_000_000_000),
        );
        assert_eq!(result["luna_reserve"]["available"], true);
        assert_eq!(result["luna_reserve"]["used_percent"], 50.0);
        assert_eq!(result["luna_reserve"]["remaining_percent"], 50.0);
        assert_eq!(result["luna_reserve"]["active"], Value::Null);
    }

    #[test]
    fn absent_quota_window_is_not_fabricated_as_full_remaining() {
        let mut data = test_data(0.0, 0.0);
        data.session.available = false;
        let result = usage_snapshot_result(
            Some(&cached(data, Duration::ZERO)),
            &PollingMetadata::default(),
            UNIX_EPOCH + Duration::from_secs(2_000_000_000),
        );
        assert_eq!(result["five_hour"], Value::Null);
        assert_eq!(result["lowest_remaining_percent"], 100.0);
        assert_eq!(result["limiting_window"], "weekly");
    }

    #[test]
    fn rejects_non_loopback_origins_and_conflicting_ports() {
        assert!(valid_loopback_host("127.0.0.1:46827", 46827));
        assert!(!valid_loopback_host("0.0.0.0:46827", 46827));
        assert!(valid_loopback_origin("http://127.0.0.1:46827", 46827));
        assert!(!valid_loopback_origin("https://example.com", 46827));
        let occupied = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = occupied.local_addr().unwrap().port();
        assert!(bind_loopback_listener(port).is_err());
    }

    #[test]
    fn streamable_http_listener_serves_tool_catalog_and_stops_cleanly() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut server = start_listener(listener, port).unwrap();
        let body = json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}).to_string();
        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        write!(
            client,
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).unwrap();
        let response = String::from_utf8(response).unwrap();
        let (headers, response_body) = response.split_once("\r\n\r\n").unwrap();
        assert!(headers.starts_with("HTTP/1.1 200 OK"));
        let response_json: Value = serde_json::from_str(response_body).unwrap();
        assert_eq!(
            response_json["result"]["tools"][0]["name"],
            "get_codex_usage"
        );

        server.stop.store(true, Ordering::Release);
        if let Some(join) = server.join.take() {
            join.join().unwrap();
        }
    }

    #[test]
    fn tool_rejects_arguments_and_unknown_names() {
        for params in [
            json!({"name":"get_codex_usage","arguments":{"path":"C:/"}}),
            json!({"name":"other_tool","arguments":{}}),
            json!({"name":"get_codex_usage","arguments":[]}),
        ] {
            let response = handle_rpc_at(
                &json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":params}),
                SystemTime::now(),
            )
            .unwrap();
            assert_eq!(response["error"]["code"], -32602);
        }
    }

    #[test]
    fn mcp_tool_request_reads_only_the_snapshot_cache() {
        let response = handle_rpc_at(
            &json!({
                "jsonrpc":"2.0",
                "id":2,
                "method":"tools/call",
                "params":{"name":"get_codex_usage","arguments":{}}
            }),
            SystemTime::now(),
        )
        .unwrap();
        assert_eq!(
            response["result"]["structuredContent"]["status"],
            "unavailable"
        );
        assert_eq!(response["result"]["isError"], false);
        // The protocol handler depends on SharedUsage only; it has no poller/network call path.
    }

    #[test]
    fn claude_tool_reads_only_the_claude_snapshot_and_reports_unavailable_without_one() {
        let response = handle_rpc_at(
            &json!({
                "jsonrpc":"2.0",
                "id":3,
                "method":"tools/call",
                "params":{"name":"get_claude_usage","arguments":{}}
            }),
            SystemTime::now(),
        )
        .unwrap();
        assert_eq!(
            response["result"]["structuredContent"]["status"],
            "unavailable"
        );
        assert_eq!(response["result"]["isError"], false);
    }

    #[test]
    fn initialization_negotiates_a_supported_streamable_http_protocol() {
        let response = handle_rpc_at(
            &json!({
                "jsonrpc":"2.0","id":4,"method":"initialize",
                "params":{"protocolVersion":"2025-11-25"}
            }),
            SystemTime::now(),
        )
        .unwrap();
        assert_eq!(response["result"]["protocolVersion"], "2025-11-25");
        assert_eq!(
            response["result"]["capabilities"]["tools"]["listChanged"],
            false
        );
    }
}
