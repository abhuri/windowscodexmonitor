# Windows Codex Monitor

A lightweight native Windows tray monitor for Codex weekly **remaining** usage.

The tray icon is a gauge needle: its position and colour represent remaining weekly usage, rather than usage already spent.

## Current MVP

- Reads Codex `account/rateLimits/read` through the local Codex app server.
- Shows weekly remaining percentage and reset countdown in the tray tooltip.
- Draws a native 32px gauge/needle tray icon.
- Opens a compact native popup on left-click: gauge, remaining percentage, reset countdown plus exact local reset date/time, watchdog status, refresh action, and auto-start toggle.
- Provides `Refresh now`, `Start with Windows`, and `Exit` from the tray menu.
- Persists the auto-start preference under `%LOCALAPPDATA%\WindowsCodexMonitor` and, when enabled, writes only the current-user Windows Run entry.
- Uses read-only Codex session-log and process signals for `Working`, `Waiting`, `Idle`, `Offline`, and `Suspected Hung`.
- Does not read, copy, or transmit Codex authentication tokens.
- Does not show 5-hour usage while that usage window is unavailable.

## Gauge colours

| Weekly remaining | Gauge colour |
| --- | --- |
| 50–100% | Green |
| 25–49% | Yellow |
| 10–24% | Orange |
| 0–9% | Red |

## Build from source

Prerequisites:

- Windows 10/11
- [Rust stable](https://rustup.rs/)
- Microsoft C++ Build Tools with the Desktop development with C++ workload
- Codex CLI installed and signed in with a ChatGPT-backed Codex account

```powershell
cargo test
cargo run --release
```

Use the non-UI diagnostic mode to verify local Codex access:

```powershell
cargo run --release -- --diagnose
```

For a visual popup check during development:

```powershell
cargo run --release -- --preview
```

The Codex account endpoint is available to Codex-service authentication, not API-key-only sign-ins. If the usage endpoint is unavailable, the tray tooltip reports the monitor as offline and leaves Codex itself unaffected.

If Codex is installed in a non-standard location, set `CODEX_MONITOR_CODEX_PATH` to the absolute path of its native `codex.exe` before starting the monitor.

## Install from GitHub

Every push to `main` is built and tested on GitHub Actions. A version tag such as `v0.1.3` creates a GitHub Release containing both `WindowsCodexMonitor-win-x64.zip` and `WindowsCodexMonitor-Setup-win-x64.exe`. The setup installer installs only for the current Windows user under `%LOCALAPPDATA%\Programs` and can launch the monitor immediately.

## Privacy and security

The application invokes the local `codex app-server` process and consumes only its rate-limit response. It does not parse or export `auth.json`, credentials, prompts, or source files.

## License

[MIT](LICENSE)
