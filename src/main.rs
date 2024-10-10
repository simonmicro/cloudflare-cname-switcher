use cloudflare_cname_switcher::backend::Backend;
use log::{error, info, warn};
use notify::{self, Watcher};

#[tokio::main]
async fn main() {
    // initialize logging
    if std::env::var("RUST_LOG").is_err() {
        // Set log level to info if not otherwise specified
        std::env::set_var("RUST_LOG", "info");
    }
    env_logger::init();

    let config_file_path = std::path::Path::new("config.yml");

    // setup config file watcher (sending events into a tokio-channel)
    let (watcher_tx, mut watcher_rx) = tokio::sync::mpsc::channel::<()>(10);
    let mut watcher =
        match notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
            Ok(event) => {
                if event.kind.is_modify() {
                    if watcher_tx.try_send(()).is_err() {
                        warn!("Failed to send file change event to main task?!");
                    }
                }
            }
            Err(e) => {
                warn!("Failed to watch configuration file: {}", e);
            }
        }) {
            Ok(v) => Some(v),
            Err(e) => {
                warn!("Failed to create file watcher: {}", e);
                None
            }
        };

    let mut first_run = true;
    loop {
        // load configuration
        let mut backend;
        {
            if first_run {
                info!("Loading configuration...");
            } else {
                info!("Reloading configuration...");
            }
            let yaml_str = match std::fs::read_to_string(config_file_path) {
                Ok(v) => v,
                Err(e) => {
                    error!("Failed to read configuration file: {}", e);
                    if first_run {
                        std::process::exit(1);
                    } else {
                        std::thread::sleep(std::time::Duration::from_secs(1));
                        continue;
                    }
                }
            };
            backend = match Backend::from_config(&yaml_str) {
                Ok(v) => v,
                Err(e) => {
                    error!("Failed to parse configuration file: {}", e);
                    if first_run {
                        std::process::exit(1);
                    } else {
                        std::thread::sleep(std::time::Duration::from_secs(1));
                        continue;
                    }
                }
            };
        }

        // setup file change handler
        if let Some(watcher) = watcher.as_mut() {
            if let notify::Result::Err(e) =
                watcher.watch(config_file_path, notify::RecursiveMode::NonRecursive)
            {
                warn!("Failed to watch configuration file: {}", e);
            }
        }

        // process events leading to config reload or shutdown
        let mut hup_listener =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()).unwrap();
        tokio::select! {
            _ = backend.run() => {
                error!("Backend-run task terminated unexpectedly?!");
                std::process::exit(2);
            },
            _ = hup_listener.recv() => {
                // on SIGHUP, reload configuration
                // just let the loop continue
            }
            _ = watcher_rx.recv() => {
                // on file change, reload configuration
                // just let the loop continue
            }
            _ = tokio::signal::ctrl_c() => {
                // the backend-run task was already cancelled at this point
                info!("Shutting down...");
                return;
            }
        }

        // stop watching the file (in case it got moved or deleted, so the handle broke)
        if let Some(watcher) = watcher.as_mut() {
            if let notify::Result::Err(e) = watcher.unwatch(config_file_path) {
                warn!("Failed to unwatch configuration file: {}", e);
            }
        }

        first_run = false;
    }
}
