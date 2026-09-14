# Codex Usage Monitor

A lightweight native Windows taskbar monitor for Codex usage limits.

<p align="center">
  <img src=".github/codex-usage-icon.png" alt="Codex Usage Monitor icon" width="96">
</p>

![Codex Usage Monitor preview](.github/animation.gif)

## Download

Download the latest release from the [fork's Releases page](https://github.com/veredis/codex-usage-monitor/releases/latest).

The current release provides the portable `codex-usage.exe`. Download it and run it directly; no installer is included in this release.

## Features

- See 5-hour and weekly Codex usage at a glance.
- Display the Codex credit balance in a dedicated taskbar column with configurable visibility.
- Show either remaining or used quota, with Remaining as the default.
- Follow live reset countdowns, including compact values such as `3h14m`.
- Use the current Windows accent color for usage bars, or choose a custom color with the native color picker.
- Choose update intervals from 30 seconds to 1 hour.
- Enable low-quota alerts at 5%, 10%, 20%, or 30% remaining.
- Select which quota rows are visible.
- Integrate with the Windows taskbar and notification-area tray.
- Move the widget between taskbars on multi-monitor systems.
- Start with Windows when enabled in Settings.
- Optionally monitor supported Claude Code and Google Antigravity usage.

## Usage

1. Download `codex-usage.exe` from the [latest fork release](https://github.com/veredis/codex-usage-monitor/releases/latest).
2. Run the executable. The monitor appears in the taskbar and notification area.
3. Right-click the taskbar widget or tray icon to refresh data and open settings.

Important defaults and settings:

- **Usage display:** Remaining. Choose Used if you prefer to see the portion already consumed.
- **Bar color:** Windows accent. Choose **Custom...** to open the native color picker and save a custom color.
- **Update frequency:** Includes a 30-second interval, in addition to longer intervals.
- **Credit display:** Always by default. Choose **Always**, **When needed**, or **Off**. Always shows the Credits column when valid credit data is available; When needed shows it when either Codex 5-hour or weekly remaining usage reaches 0%; Off hides it. A known zero balance displays as `0`, while missing or unknown data is not treated as zero. Finite balances are shown as whole credits using floor display, and no dollar value is shown.
- **Credit position:** Left by default. Choose **Left** or **Right** independently of Credit display. Turning Credits Off keeps the selected side, which is restored when the column is shown again.
- **Quota alerts:** Disabled by default; thresholds are based on remaining quota. When enabled at any threshold, the app also sends a separate one-time alert if that quota window reaches 0% remaining. Turning alerts Off disables both notifications.

Drag the widget's divider to reposition it. On multi-monitor systems, drag it to the taskbar where you want it displayed.

## Requirements

- Windows 10 or Windows 11
- Codex CLI or the Codex app installed and signed in
- An internet connection for provider usage requests

Claude Code and Google Antigravity monitoring are optional and require their respective local applications and credentials.

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

This includes display mode, bar color, polling interval, alert threshold, credit-display and credit-position preferences, visible rows, provider selection, language, widget position, and startup preference. Existing settings files remain compatible; missing newer fields use their defaults.

For installation and upgrade details, see [docs/installation.md](docs/installation.md).

## Privacy

The monitor reads local authentication data for the enabled providers and requests usage information directly from their services. It does not use a separate backend, telemetry, or analytics service, and it does not upload project files.

For diagnostic logging and troubleshooting, see [docs/troubleshooting.md](docs/troubleshooting.md).

## Upstream and credits

This repository is a modified fork of [upstream-ray/codex-usage-monitor](https://github.com/upstream-ray/codex-usage-monitor). It preserves the original project attribution and MIT license. The upstream project carries forward work from [CodeZeno/Claude-Code-Usage-Monitor](https://github.com/CodeZeno/Claude-Code-Usage-Monitor).

This fork is not an official upstream release, and the upstream maintainers do not maintain or endorse it.

## License

Released under the [MIT License](LICENSE).
