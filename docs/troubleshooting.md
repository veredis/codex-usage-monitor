# Troubleshooting Codex Usage

## Taskbar error labels

Codex Usage keeps authentication failures separate from transient service failures:

| Simplified Chinese | Other languages | Meaning | Recommended action |
|---|---|---|---|
| `!` | `!` | Enabled service credentials are missing or expired | Sign in with the relevant CLI/app, then refresh Codex Usage. For Claude, an active Claude desktop app sign-in for the same account is also used (read-only). |
| `网络` | `NET` | Network or TLS connection failed | Check connectivity, VPN, proxy, and firewall settings |
| `限流` | `429` | Provider rate limit | Wait for the provider retry window; Codex Usage retries that provider with backoff and honors `Retry-After` |
| `服务` | `5XX` | Provider service failure | Wait and retry; check provider status if it persists |
| `错误` | `ERR` | Invalid or unsupported response | Enable diagnostics and inspect the log |

A leading `!` before a percentage (for example `!88% 1h04m`) means that provider's last value is stale: its latest request failed, it has not succeeded within twice its own polling interval, or it was restored from the snapshot saved before the last restart and has not been refreshed yet. Other providers are unaffected; each provider has its own schedule and backoff.

When a window's countdown reaches zero, its last confirmed value stays visible for up to five minutes, marked `!` and without a countdown, until the next refresh brings the new value (Claude is refreshed at most every two minutes). Hovering it shows the reset time. If no fresh value arrives in that time, the row shows `!--`: the reset has passed, so the old percentage no longer applies and the new one is not known yet, and the bar stays empty until the next successful refresh. Plain `--` always means the provider does not report that window. The MCP tools report a window whose reset has passed as `null` immediately.

Rejected Claude credentials are not resent until the credential file changes, preventing repeated login notifications. Transient failures back off per provider (never faster than the selected frequency, up to 30 minutes) and honor any server `Retry-After` deadline, including on **Refresh**.

### Claude shows `!` or `429` frequently

Claude's usage endpoint is shared with every running Claude Code session's status line and rate-limits aggressively. In Build 6 the monitor spaces Claude requests at least 120 seconds apart regardless of the selected frequency; this is a current design decision and may be revised. Many concurrent Claude Code sessions can still exhaust the shared limit; the monitor then backs off Claude alone while Codex keeps updating.

### Claude shows `!` after the CLI sign-in expires

The Claude Code CLI keeps its sign-in in `~/.claude/.credentials.json` and renews it only while the CLI runs; its access token lasts about eight hours. The Claude desktop app, including its Code tab, keeps a **separate** sign-in that it renews itself, so using Desktop does not renew the CLI file.

Codex Usage therefore falls back, read-only, to the Claude Desktop sign-in for the same account when the CLI sign-in has expired. The log shows which source is in use:

- `Claude using the Claude Desktop sign-in (read-only); CLI sign-in category=token_expired`
- `Claude Desktop sign-in unavailable reason=…`, where the reason is one of:
  - `not_installed`, `no_token_cache` or `decrypt_failed`: Desktop's store is missing or unreadable.
  - `account_unparsed`: no Desktop entry names its account, so it cannot be attributed.
  - `account_mismatch`: Desktop is signed in to a different account than the CLI.
  - `organization_mismatch`: the same account, but a different organization.
  - `ambiguous`: several accounts or organizations fit, so none is guessed.
  - `no_profile_scope`: the entry cannot read usage.
- `Claude sign-in expired; no verified model-free renewal is enabled, waiting for Claude Code or Claude Desktop to refresh its own sign-in`
- `Claude credential change detected; resuming usage requests` and `Claude sign-in available` once a refreshed sign-in works
- `Claude usage served from Claude Code's local cache age_s=N; no request` when the CLI fetched usage itself within two minutes

To check the sources without opening the widget, run (no tokens, emails or account identifiers are written):

```powershell
codex-usage.exe --probe-claude-sign-in "$env:TEMP\claude-sign-in.txt"
```

Add `--with-request` to also make one normal usage request (CLI sign-in first). Add `--with-desktop-request` instead to make exactly one usage request with the matched Claude Desktop sign-in only. It refuses to run, sending nothing, if a monitor instance is running with Claude enabled, if Claude's last request in the log was under 120 seconds ago, or if a logged Retry-After deadline is still active. Exit the monitor, or turn Claude off in it, first. If neither sign-in is usable, start the Claude Code CLI once (`claude`, then `/exit`) or use Claude Desktop; the monitor picks up the renewed sign-in on its next tick.

**Why there is no automatic CLI renewal.** The monitor never refreshes OAuth tokens itself: refresh-token rotation races the CLI and can sign it out. Short-lived CLI commands such as `claude auth status` or `claude doctor` can start a refresh and exit before saving it, spending the refresh token (anthropics/claude-code#95822). `claude -p /usage` is reported to renew safely, because in `-p` mode `/usage` is a local command that waits for the refresh before fetching usage. This has not been validated for this installation and is not used.

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
- `system resumed: refreshing providers in 10000 ms ...`, `resume refresh starting` and `network restored: refreshing failing or stale providers` around sleep and connectivity changes
- `preferred taskbar unavailable; using the primary taskbar until it returns` and `preferred taskbar available again; reattaching widget` when the chosen display disconnects and reconnects

The log does not include access tokens, refresh tokens, credential file contents, or API response bodies.

### The log seems not to update

Tools that run inside an MSIX-packaged app (for example the Codex desktop app or an agent running in it) may see a private, virtualized copy at `%LOCALAPPDATA%\Packages\<package>\LocalCache\Local\CodexUsage\logs\` instead of the real file. That copy stops at whatever an earlier packaged launch wrote. Open the log with **Open log file** from the monitor's menu, or read it from a normal (unpackaged) PowerShell or Explorer window. Launch the monitor from Explorer, not from inside a packaged app, so its own writes are not redirected.

## The widget covers Show hidden icons, the tray or the clock

Every placement path (automatic, manual offset, dragging, dropping on another taskbar, width changes) stays left of the notification area's left edge. When the tray grows (a new tray icon, for example), the widget moves left immediately. A delayed check re-reads the final layout after bursts of shell events and after the monitor's own tray-icon updates. Each countdown tick runs it again as a periodic safety net. On taskbars that expose no notification window, such as some Windows 11 secondary taskbars, a clock-sized area at the right edge is reserved.

If an overlap still appears, note the time and look in the log for `positioned embedded widget ... boundary=` and `layout reconcile corrected` lines around it. A saved manual offset is kept even when a smaller taskbar cannot hold it; the widget is clamped for that taskbar and returns to the saved offset when there is room again.

The chosen taskbar is remembered by its display (`monitor_device` in `settings.json`), not only by its position in the taskbar list. If that display is disconnected, the widget moves to the primary taskbar without forgetting the choice, and returns once the display reconnects. Dragging the widget onto another taskbar changes the choice.

## An older version starts with Windows

**Start with Windows** is the `CodexUsage` value under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.

**Older versions:** builds before Build 13 wrote the path of *whichever executable turned the option on*, and never updated it. Starting a newer copy from another folder did not change it. That is how an old development build could launch at sign-in.

**Build 13 and later:**
- The value is quoted.
- When `%LOCALAPPDATA%\Programs\CodexUsage\codex-usage.exe` exists, the value always points at that installed copy.
- The installed copy repairs a value left pointing at another `codex-usage.exe`, and logs `startup registration repaired to launch the installed copy`.

To check the value and which build runs:

```powershell
Get-ItemPropertyValue 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' CodexUsage
Select-String "$env:LOCALAPPDATA\CodexUsage\logs\codex-usage.log" -Pattern 'identity=' | Select-Object -Last 1
```

Install or update the stable copy with `scripts\install.ps1 -SourcePath <exe> -NoLaunch`. If **Start with Windows** was already on, the installer points it at the stable copy, quoted.

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
