# Codex Usage Monitor

A lightweight native Windows taskbar monitor for Codex usage limits.

<p align="center">
  <img src=".github/codex-usage-icon.png" alt="Codex Usage Monitor icon" width="96">
</p>

![Codex Usage Monitor preview](.github/animation.gif)

This repository is the `veredis` fork of [upstream-ray/codex-usage-monitor](https://github.com/upstream-ray/codex-usage-monitor). It keeps the original project's Windows taskbar monitor and adds the fork features described below.

## Download

Download the latest fork release from [veredis/codex-usage-monitor](https://github.com/veredis/codex-usage-monitor/releases/latest).

The release is portable: download `codex-usage.exe` and run it directly.

To keep **Start with Windows** stable across updates, install it to `%LOCALAPPDATA%\Programs\CodexUsage` with `scripts\install.ps1`. `-SourcePath <exe>` installs a local build, and `-NoLaunch` and `-NoDesktopShortcut` are optional.

**Start with Windows** always registers that installed copy when it exists. The installed copy also repairs a registration left pointing at another `codex-usage.exe`, so a development or older build you launched by hand never takes over sign-in startup.

## Features

- Compact 5-hour and weekly Codex usage in the Windows taskbar.
- Default (Mixed), Remaining or Used display modes.
- Dynamic sizing that follows the displayed quota text without wasting space.
- Local reset countdowns, including day-and-hour values such as `1d23h`.
- Selectable remaining-quota alerts at 2%, 5%, 10%, 20%, and 30%; multiple thresholds can be enabled together.
- A separate one-time 0% exhaustion alert for enabled quota windows.
- Optional Credits display with Left or Right placement.
- Credits or approximate USD display (`~$13.40`) using the raw balance for conversion.
- Upstream's appearance:
  - a provider mark beside each provider's bars (**Appearance > Show provider icons** hides them and reclaims their space)
  - antialiased rounded bars, continuous or segmented, standard or slim
  - standard or larger text
  - high-contrast dark and light palettes with a solid backdrop for translucent taskbars

  The widget also shrinks to fit shorter taskbars.
- Default per-provider colors (Claude orange, Codex green, Antigravity blue), or your own: one color for every bar (custom or the Windows accent), or a color per provider, chosen with the native color picker. **Appearance > Colors > Default provider colors** restores the defaults.
- Hover a quota value to see its exact local reset date and time, or what `--` (not reported) and `!--` (reset since the last refresh) mean. The popup and the right-click menu follow the widget's dark or light palette.
- Fixed polling intervals from 30 seconds to 1 hour, plus optional Adaptive polling.
- Optional compact 2x3 drag handle and persistent widget positioning.
- Automatic, manual and dragged positions all stay left of the notification area (Show hidden icons, tray icons and clock); taskbar layout changes are always reconciled, even when they arrive in bursts.
- Multi-monitor taskbar placement, notification-area integration, and Start with Windows support.
- Optional Claude Code and Google Antigravity monitoring where their local applications and credentials are available.

## Usage

1. Download and run `codex-usage.exe`.
2. Right-click the taskbar widget or notification-area icon to open Settings.
3. Configure display, polling, alert, provider, Credits, and positioning options from the menus.

Important defaults and settings:

- **Usage display:** **Default (Mixed)** shows Codex as Remaining (falling toward 0%, like Codex `/status` "% left") and Claude as Used (rising toward 100%, like Claude's "% used"); Antigravity shows Remaining. **Remaining** and **Used** apply one direction to every provider. New installations start with Default (Mixed); existing settings keep their saved choice, and settings saved before this option existed keep Remaining. The setting changes only text and bars. Alerts, Adaptive polling and MCP always use the underlying quota values.
- **Update frequency:** Fixed choices are 30 seconds, 1 minute, 5 minutes, 15 minutes, and 1 hour. Adaptive uses 5 minutes above 30% remaining, 1 minute from above 10% through 30%, and 30 seconds at 10% or below. Claude usage requests are spaced at least 2 minutes apart at every setting (a current design decision, see below). Countdown movement is local and does not create extra provider requests.
- **Quota alerts:** Off by default. Select any combination of 2%, 5%, 10%, 20%, and 30% remaining. Each selected threshold alerts once per genuine quota window; an enabled window can also send one separate alert at 0% remaining.
- **Credits:** Credits visibility is **Always** by default, with **Always**, **When needed**, and **Off** options. **When needed** shows the auxiliary area when an included Codex quota reaches 0%. The area can be placed **Left** or **Right**, independently of visibility. Choose **Credits** or **USD** value format. Finite Credits use the precise underlying balance for USD estimates and display whole Credits rounded to the nearest whole credit; known zero, unlimited, and unknown data remain distinct.
- **Appearance:** **Settings > Appearance** offers:
  - two profiles: **Solid Dark** (a solid dark backdrop with slim bars and larger text, for translucent taskbars) and **Taskbar Blend** (the standard look, blending into the taskbar). Both keep your colors and provider-icon choice; the active one is ticked.
  - contrast, bar style and thickness, text size, provider icons and colors

  An explicitly chosen color always wins over a palette, and a color saved by an earlier version keeps applying to every bar.
- **Taskbar placement:** **Show drag handle** is off by default. Reset Position computes a safe default without forgetting a manually saved position.
- **MCP integration:** Enable **Enable MCP integration** to expose the local read-only usage tools described below.

## Polling and cached data

Each provider is polled on its own schedule and its own worker, so one provider's slow response, rate limit or outage never delays another provider or marks it stale. Failures back off per provider, never faster than the selected frequency, and a server `Retry-After` deadline is honored even by **Refresh**.

Claude's usage endpoint is shared with every Claude Code session's status line and rate-limits aggressively, so the monitor currently spaces Claude requests at least 120 seconds apart, even at the 30-second or Adaptive settings. **Refresh** may bypass this spacing but not a server deadline. This is the Build 6 design decision and may be revised.

After the computer resumes from sleep or hibernation, providers refresh about 10 seconds later. When internet access returns after an outage, failing or stale providers refresh at once. Both recovery refreshes skip backoff that built up while offline, but never Claude's request spacing, a server `Retry-After` deadline or a request already in flight.

A window the provider does not report is shown as `--`, never as 0% used or 100% remaining, and it never triggers an alert.

Provider results feed one normalized monitor cache. The taskbar UI and MCP server read that cache; MCP requests do not trigger another usage request. During transient failures, the last successful percentages remain visible with a leading `!` while reset countdowns continue from their known timestamps and freshness is reported honestly. A value is marked stale only when its own provider failed or has not succeeded within twice its own interval.

## Local MCP integration

When enabled in Settings, the monitor hosts a loopback-only MCP server at:

```text
http://127.0.0.1:46827/mcp
```

It exposes two separate read-only tools:

- `get_codex_usage`
- `get_claude_usage`

The tools return the latest cached provider snapshot, including quota windows, freshness, polling metadata, and Credits where available. A tool call never starts a provider poll, authentication flow, model task, or credential operation.

For Codex, add the following to the Codex configuration if you want Codex to discover the server:

```toml
[mcp_servers.codex_usage_monitor]
url = "http://127.0.0.1:46827/mcp"
enabled = true
```

For Claude Code, register it once at user scope (the monitor never edits client configuration itself):

```powershell
claude mcp add --transport http --scope user codex_usage_monitor http://127.0.0.1:46827/mcp
```

Start a new Claude Code session afterwards. In the Claude desktop app's Code tab, `/mcp` opens the account Connectors screen, which does not list user-config servers; the tools are still available to new sessions.

Each result includes `data_source`: `live` (this monitor's request), `claude_code_cache` (Claude Code's own cached reading), `restored_snapshot` (the last values saved before a restart, always reported `stale` until refreshed) or `null` when nothing is known. A window whose reset has passed since its values were observed is reported as `null` rather than as its old percentage.

For an agent using the tools, query usage at meaningful milestones rather than continuously. Check before starting another large work phase, and if a quota is low, finish or preserve the current coherent unit instead of blindly starting a large new phase.

## Claude support and authentication

Claude usage comes from the same read-only usage endpoint Claude Code uses. Each Claude tick uses the first of these that works, without ever refreshing, rotating or writing a credential:

1. **Claude Code's own cached reading** (`cachedUsageUtilization` in `~/.claude.json`) when the CLI fetched it within the last two minutes and it belongs to the CLI's signed-in account. No request is made.
2. **The Claude Code CLI sign-in** (`~/.claude/.credentials.json`).
3. **The Claude desktop app's own sign-in**, read-only, when the CLI sign-in is expired, missing or rejected. Claude Desktop keeps this sign-in fresh while you use it (including its Code tab), whereas the CLI file only renews when the CLI itself runs. Only an entry for the CLI's signed-in account is used, decrypted with the current Windows user's DPAPI key; the token is held in memory for the request and never logged or saved. This fallback is **off unless you turn it on**. **Settings > Use Claude Desktop sign-in (read-only)** explains what it reads and sends, and asks before enabling it. The same menu item turns it off again; it is saved as `use_claude_desktop_sign_in` in `settings.json`.

If no sign-in is usable, the monitor keeps the last values marked stale (`!`) and waits; as soon as Claude Code or Claude Desktop refreshes its own sign-in, the next tick resumes without restarting the widget. A rejected token is never resent until it changes. The monitor never runs a model request, `claude -p`, or an OAuth refresh to force renewal. Unattended renewal of the CLI sign-in is not automated; see `docs/troubleshooting.md`.

The last successful values of each provider are saved to `%APPDATA%\CodexUsage\quota-*.json` (percentages, reset and update times and an opaque account fingerprint only) and restored at startup, shown stale until the first successful refresh. A snapshot from another account, a corrupt file, or one older than eight days is ignored.

## Codex authentication

Normal Codex usage polling is passive and reads quota data from the Codex usage endpoint. Authentication recovery uses credential watching and the official model-free Codex app-server account refresh. Automatic model-consuming recovery (`codex exec`) is disabled.

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

The settings file includes display modes, bar color, polling, alerts, provider selection, Credits visibility and placement, language, widget position, startup behavior, and the MCP preference. Older settings files remain compatible; missing newer fields use safe defaults.

## Privacy

The monitor reads local authentication data for enabled providers and requests usage information directly from their services. It has no separate telemetry, analytics service, or application backend, and it does not upload project files. The optional MCP server is local, loopback-only, read-only usage telemetry.

## Upstream, attribution, and license

This is a modified fork of [upstream-ray/codex-usage-monitor](https://github.com/upstream-ray/codex-usage-monitor), which carries forward work from [CodeZeno/Claude-Code-Usage-Monitor](https://github.com/CodeZeno/Claude-Code-Usage-Monitor). Original attribution and copyright notices are preserved.

This fork is not an official upstream release. The upstream maintainers do not maintain or endorse this fork unless they explicitly state otherwise.

Released under the [MIT License](LICENSE).

## Current version

The public fork release is `1.9.1-veredis.7`. The current development binary identifies itself as `1.9.1-veredis.7 (Build 16)`; the Build number is an informational candidate identifier and does not change update ordering.
