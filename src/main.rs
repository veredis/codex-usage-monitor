#![windows_subsystem = "windows"]

mod build_info;
mod codex_mcp;
mod diagnose;
mod localization;
mod models;
mod native_interop;
mod poller;
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
                build_info::BUILD_NUMBER,
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

    if let Some(exit_code) = updater::handle_cli_mode(&args) {
        diagnose::log(format!("cli mode exited with code {exit_code}"));
        std::process::exit(exit_code);
    }

    diagnose::log("entering window::run");
    window::run();
}
