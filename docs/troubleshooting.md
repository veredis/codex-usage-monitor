# Troubleshooting Codex Usage

## Taskbar error labels

Codex Usage keeps authentication failures separate from transient service failures:

| Simplified Chinese | Other languages | Meaning | Recommended action |
|---|---|---|---|
| `!` | `!` | Enabled service credentials are missing or expired | Sign in with the relevant CLI/app, then refresh Codex Usage. Claude Desktop login does not count as Claude Code CLI login. |
| `网络` | `NET` | Network or TLS connection failed | Check connectivity, VPN, proxy, and firewall settings |
| `限流` | `429` | Provider rate limit | Wait for the provider retry window; Codex Usage retries that provider with backoff and honors `Retry-After` |
| `服务` | `5XX` | Provider service failure | Wait and retry; check provider status if it persists |
| `错误` | `ERR` | Invalid or unsupported response | Enable diagnostics and inspect the log |

A leading `!` before a percentage (for example `!88% 1h04m`) means that provider's last value is stale: its latest request failed, or it has not succeeded within twice its own polling interval. Other providers are unaffected; each provider has its own schedule and backoff.

Rejected Claude credentials are not resent until the credential file changes, preventing repeated login notifications. Transient failures back off per provider (never faster than the selected frequency, up to 30 minutes) and honor any server `Retry-After` deadline, including on **Refresh**.

### Claude shows `!` or `429` frequently

Claude's usage endpoint is shared with every running Claude Code session's status line and rate-limits aggressively. In Build 6 the monitor spaces Claude requests at least 120 seconds apart regardless of the selected frequency; this is a current design decision and may be revised. Many concurrent Claude Code sessions can still exhaust the shared limit; the monitor then backs off Claude alone while Codex keeps updating.

## Diagnostic log

Operational logging is always on and is written to:

```text
%LOCALAPPDATA%\CodexUsage\logs\codex-usage.log
```

`codex-usage.exe --diagnose` adds extra-verbose records. The log includes:

- application version, build and executable path, and the process ID on every record
- each provider request with elapsed time, failure category, HTTP status and `Retry-After`
- each provider's schedule (time until ready, retry count, server wait)
- window creation, taskbar placement (with the notification-area boundary and applied offset), and relaunch events
- `layout reconcile corrected widget ...` when a delayed check had to move the widget after a burst or a suppressed shell layout event

The log does not include access tokens, refresh tokens, credential file contents, or API response bodies.

### The log seems not to update

Tools that run inside an MSIX-packaged app (for example the Codex desktop app or an agent running in it) may see a private, virtualized copy at `%LOCALAPPDATA%\Packages\<package>\LocalCache\Local\CodexUsage\logs\` instead of the real file. That copy stops at whatever an earlier packaged launch wrote. Open the log with **Open log file** from the monitor's menu, or read it from a normal (unpackaged) PowerShell or Explorer window. Launch the monitor from Explorer, not from inside a packaged app, so its own writes are not redirected.

## The widget covers Show hidden icons, the tray or the clock

Every placement path (automatic, manual offset, dragging, dropping on another taskbar, width changes) stays left of the notification area's left edge. When the tray grows (a new tray icon, for example), the widget moves left immediately. A delayed check re-reads the final layout after bursts of shell events and after the monitor's own tray-icon updates. Each countdown tick runs it again as a periodic safety net. On taskbars that expose no notification window, such as some Windows 11 secondary taskbars, a clock-sized area at the right edge is reserved.

If an overlap still appears, note the time and look in the log for `positioned embedded widget ... boundary=` and `layout reconcile corrected` lines around it. A saved manual offset is kept even when a smaller taskbar cannot hold it; the widget is clamped for that taskbar and returns to the saved offset when there is room again.

## Update failures

Direct installations and portable copies download only the exact `codex-usage.exe` asset and verify it against `codex-usage.exe.sha256` from the same GitHub Release. The updater keeps the previous EXE until the downloaded version has been installed and restarted successfully. If restart fails, the old EXE is restored.

WinGet-managed installations delegate upgrades to WinGet:

```powershell
winget upgrade --id Ray.CodexUsage --exact
```

## Reset local position without deleting settings

Right-click the taskbar component and choose **Settings > Reset Position**. Settings are stored at `%APPDATA%\CodexUsage\settings.json`.

## Reinstall while preserving settings

Normal uninstall keeps the settings file. Reinstalling restores the saved language, refresh interval, provider selection, widget visibility, and taskbar position. Use `uninstall.ps1 -RemoveSettings` only when a full reset is intended.
