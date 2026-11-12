#![windows_subsystem = "windows"]

mod antigravity_local;
mod build_info;
mod claude_desktop;
mod claude_local;
mod codex_mcp;
mod diagnose;
mod localization;
mod menu_theme;
mod models;
mod native_interop;
mod poller;
mod provider_health;
mod provider_icons;
mod provider_schedule;
mod quota_cache;
mod quota_popup;
mod recovery_events;
mod retry_after;
mod rounded_bars;
mod taskbar_layout;
mod theme;
mod tray_icon;
mod updater;
mod window;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let diagnose_enabled = args.iter().any(|arg| arg == "--diagnose");
    match diagnose::init(diagnose_enabled) {
        Ok(path) => {
            diagnose::log(format!("startup log_path={}", path.display()));
            diagnose::log(format!(
                "version={} build={} identity={} install_channel={:?} executable={}",
                build_info::VERSION,
                build_info::build_label(),
                build_info::identity(),
                updater::current_install_channel(),
                std::env::current_exe()
                    .map(|value| value.display().to_string())
                    .unwrap_or_else(|error| format!("unavailable:{error}"))
            ));
        }
        Err(error) => {
            eprintln!("Codex Usage Monitor logging unavailable: {error}");
        }
    }

    // Read-only sign-in diagnosis; never opens the widget or the MCP server.
    if let Some(index) = args.iter().position(|arg| arg == "--probe-claude-sign-in") {
        let normal = args.iter().any(|arg| arg == "--with-request");
        let desktop = args.iter().any(|arg| arg == "--with-desktop-request");
        let report = match (normal, desktop) {
            (true, true) => "probe=refused reason=choose_one_request_option\n".to_string(),
            (true, false) => poller::probe_claude_sign_in(poller::ProbeRequest::NormalPoll),
            (false, true) => poller::probe_claude_sign_in(poller::ProbeRequest::DesktopOnly),
            (false, false) => poller::probe_claude_sign_in(poller::ProbeRequest::None),
        };
        let code = match args.get(index + 1).filter(|path| !path.starts_with("--")) {
            Some(path) => i32::from(std::fs::write(path, report).is_err()),
            None => 2,
        };
        diagnose::log(format!("claude sign-in probe exited with code {code}"));
        std::process::exit(code);
    }

    if let Some(exit_code) = updater::handle_cli_mode(&args) {
        diagnose::log(format!("cli mode exited with code {exit_code}"));
        std::process::exit(exit_code);
    }

    diagnose::log("entering window::run");
    window::run();
}
