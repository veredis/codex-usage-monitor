# Codex Usage installation model

Codex Usage remains a single native Windows executable. Installation only places the executable and an uninstall helper in a stable per-user directory; it does not add a runtime, service, driver, telemetry component, or machine-wide dependency.

## Direct installation

- Install directory: `%LOCALAPPDATA%\Programs\CodexUsage`
- Executable: `%LOCALAPPDATA%\Programs\CodexUsage\codex-usage.exe`
- Uninstall helper: `%LOCALAPPDATA%\Programs\CodexUsage\uninstall.ps1`
- Start menu shortcut: `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Codex Usage.lnk`
- Add/Remove Programs key: `HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\CodexUsage`

The installer is per-user and does not request elevation. It verifies the release SHA256 before replacing an existing executable. Replacement uses a temporary file and keeps the previous executable until the new file has been placed successfully.

## Portable mode

`codex-usage.exe` can be run from any user-writable directory without installation. Portable mode uses the same `%APPDATA%\CodexUsage\settings.json` settings as a direct installation.

## Settings and startup behavior

- Upgrades preserve `%APPDATA%\CodexUsage\settings.json`.
- Normal uninstall preserves settings so a later reinstall restores preferences.
- `uninstall.ps1 -RemoveSettings` explicitly deletes the settings directory.
- The installer does not enable startup automatically. If startup was already enabled, installation preserves that choice and updates the registry value to the stable installed executable. Users otherwise control startup from the application's settings menu.
- Uninstall removes the `CodexUsage` startup entry because its executable no longer exists.

## WinGet

The WinGet package `Ray.CodexUsage` belongs to the upstream project and installs upstream's executable. This fork (`veredis/codex-usage-monitor`) is not published to WinGet: install it with `install.ps1` or run it portably. Its in-app updater and `install.ps1` download only from this fork's GitHub Releases and never delegate to WinGet.
