# Codex Usage Monitor

A lightweight native Windows taskbar monitor for Codex usage limits.

<p align="center">
  <img src=".github/codex-usage-icon.png" alt="Codex Usage Monitor icon" width="96">
</p>

![Codex Usage Monitor preview](.github/animation.gif)

This repository is the `veredis` fork of [upstream-ray/codex-usage-monitor](https://github.com/upstream-ray/codex-usage-monitor). It keeps the original project's Windows taskbar monitor and adds the fork features described below.

## Download

Download the latest fork release from [veredis/codex-usage-monitor](https://github.com/veredis/codex-usage-monitor/releases/latest).

The release is portable: download `codex-usage.exe` and run it directly. No installer is required.

## Features

- Compact 5-hour and weekly Codex usage in the Windows taskbar.
- Remaining or Used display modes, with Remaining as the default.
- Dynamic sizing that follows the displayed quota text without wasting space.
- Local reset countdowns, including day-and-hour values such as `1d23h`.
- Selectable remaining-quota alerts at 2%, 5%, 10%, 20%, and 30%; multiple thresholds can be enabled together.
- A separate one-time 0% exhaustion alert for enabled quota windows.
- Optional Credits / Luna Reserve auxiliary display with Left or Right placement.
- Credits or approximate USD display (`~$13.40`) using the raw balance for conversion.
- Windows accent-colored bars or a custom color selected with the native color picker.
- Fixed polling intervals from 30 seconds to 1 hour, plus optional Adaptive polling.
- Optional compact 2x3 drag handle and persistent widget positioning.
- Safer default and Reset position placement around discoverable taskbar widgets and occupied areas.
- Multi-monitor taskbar placement, notification-area integration, and Start with Windows support.
- Optional Claude Code and Google Antigravity monitoring where their local applications and credentials are available.

## Usage

1. Download and run `codex-usage.exe`.
2. Right-click the taskbar widget or notification-area icon to open Settings.
3. Configure display, polling, alert, provider, Credits / Luna Reserve, and positioning options from the menus.

Important defaults and settings:

- **Usage display:** Remaining. Used shows the portion already consumed.
- **Update frequency:** Fixed choices are 30 seconds, 1 minute, 5 minutes, 15 minutes, and 1 hour. Adaptive uses 5 minutes above 30% remaining, 1 minute from above 10% through 30%, and 30 seconds at 10% or below. Countdown movement is local and does not create extra provider requests.
- **Quota alerts:** Off by default. Select any combination of 2%, 5%, 10%, 20%, and 30% remaining. Each selected threshold alerts once per genuine quota window; an enabled window can also send one separate alert at 0% remaining.
- **Credits / Luna Reserve:** Credits visibility is **Always** by default, with **Always**, **When needed**, and **Off** options. **When needed** shows the auxiliary area when an included Codex quota reaches 0%. The area can be placed **Left** or **Right**, independently of visibility. Choose **Credits** or **USD** value format. Finite Credits use the precise underlying balance for USD estimates and display whole Credits rounded to the nearest whole credit; known zero, unlimited, and unknown data remain distinct. Luna Reserve is shown only when supported data indicates it is the appropriate fallback. It is optional and may be absent for an account.
- **Taskbar placement:** **Show drag handle** is off by default. Reset Position computes a safe default without forgetting a manually saved position.
- **MCP integration:** Enable **Enable MCP integration** to expose the local read-only usage tools described below.

## Polling and cached data

Provider polling feeds one normalized monitor cache. The taskbar UI and MCP server read that cache; MCP requests do not trigger another usage request. During transient failures, the last successful percentages remain visible while reset countdowns continue from their known timestamps and freshness is reported honestly.

## Local MCP integration

When enabled in Settings, the monitor hosts a loopback-only MCP server at:

```text
http://127.0.0.1:46827/mcp
```

It exposes two separate read-only tools:

- `get_codex_usage`
- `get_claude_usage`

The tools return the latest cached provider snapshot, including quota windows, freshness, polling metadata, Credits where available, and Luna Reserve data where the Codex backend exposes it. A tool call never starts a provider poll, authentication flow, model task, or credential operation.

For Codex, add the following to the Codex configuration if you want Codex to discover the server:

```toml
[mcp_servers.codex_usage_monitor]
url = "http://127.0.0.1:46827/mcp"
enabled = true
```

The monitor does not edit Codex or Claude configuration automatically. Claude client configuration varies by client; use that client's current MCP documentation when connecting it to the same loopback URL.

For an agent using the tools, query usage at meaningful milestones rather than continuously. Check before starting another large work phase, and if a quota is low, finish or preserve the current coherent unit instead of blindly starting a large new phase.

## Claude support and authentication

Claude monitoring uses the Claude Code CLI's credentials and usage interface where available. Claude Code CLI authentication is required; being signed in to the Claude desktop app alone does not necessarily provide the credentials used by this monitor.

Healthy monitoring is intended to be passive. If credentials expire, the monitor first attempts passive recovery. A heavily guarded emergency `claude -p .` refresh may still be used only after passive recovery fails, and is not part of ordinary polling.

## Codex authentication

Normal Codex usage polling is passive and reads quota data from the Codex usage endpoint. Authentication recovery prefers credential watching and the official model-free Codex app-server account refresh. A guarded `codex exec .` fallback is retained only as a last resort after passive recovery fails; it is rate-limited and limited to one attempt per recovery episode.

## Logging

Operational logging is enabled during normal launches and is stored at:

```text
%LOCALAPPDATA%\CodexUsage\logs\codex-usage.log
```

Logs are bounded and rotated for unattended use. They record useful startup, polling, recovery, and MCP diagnostics without intentionally recording access tokens, refresh tokens, authentication-file contents, prompts, or unrelated private data. Use **Open log file** in the menu to open the active log or its containing folder.

## Updates

The portable updater checks releases from [veredis/codex-usage-monitor](https://github.com/veredis/codex-usage-monitor), downloads the single `codex-usage.exe` asset, and requires the matching `codex-usage.exe.sha256` asset before replacing the local executable. Updates are verified with SHA-256 before installation.

## Requirements

- Windows 10 or Windows 11.
- Codex CLI or the Codex app installed and signed in for Codex monitoring.
- An internet connection for provider usage requests.

Claude Code and Google Antigravity monitoring are optional and require their respective local applications and supported credentials.

## Build from source

Install Rust with the MSVC toolchain, then run:

```powershell
cargo build --release
```

The executable is written to:

```text
target\release\codex-usage.exe
```

## Settings and data

Preferences are stored in:

```text
%APPDATA%\CodexUsage\settings.json
```

The settings file includes display modes, bar color, polling, alerts, provider selection, Credits / Luna Reserve visibility and placement, language, widget position, startup behavior, and the MCP preference. Older settings files remain compatible; missing newer fields use safe defaults.

## Privacy

The monitor reads local authentication data for enabled providers and requests usage information directly from their services. It has no separate telemetry, analytics service, or application backend, and it does not upload project files. The optional MCP server is local, loopback-only, read-only usage telemetry.

## Upstream, attribution, and license

This is a modified fork of [upstream-ray/codex-usage-monitor](https://github.com/upstream-ray/codex-usage-monitor), which carries forward work from [CodeZeno/Claude-Code-Usage-Monitor](https://github.com/CodeZeno/Claude-Code-Usage-Monitor). Original attribution and copyright notices are preserved.

This fork is not an official upstream release. The upstream maintainers do not maintain or endorse this fork unless they explicitly state otherwise.

Released under the [MIT License](LICENSE).

## Current version

The public fork release is `1.9.1-veredis.7`. The current binary identifies itself as `1.9.1-veredis.7 (Build 4)`; the Build number is an informational candidate identifier and does not change update ordering.
