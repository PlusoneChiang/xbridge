mod bridge;
mod config;
mod discovery;
mod log;
mod service;
mod shared;

fn main() {
    crate::log!("[xbridge] starting");
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str());

    let result = match cmd {
        Some("--install") => service::installer::install(),
        Some("--uninstall") => service::installer::uninstall(),
        Some("--enable") => service::installer::enable(),
        Some("--disable") => service::installer::disable(),
        Some("--run") => {
            service::host::run_foreground();
            Ok(())
        }
        _ => {
            // Default: attempt to start as a Windows service; fall back to foreground
            match service::host::start_as_service() {
                Ok(()) => Ok(()),
                Err(_) => {
                    service::host::run_foreground();
                    Ok(())
                }
            }
        }
    };

    if let Err(e) = result {
        crate::log!("[xbridge] error: {e}");
        std::process::exit(1);
    }
}
