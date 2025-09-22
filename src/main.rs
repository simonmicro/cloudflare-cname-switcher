use cloudflare_cname_switcher::http_server::HttpServer;
use cloudflare_cname_switcher::ingress::Ingress;
use log::{error, info, warn};
use notify::{self, Watcher};

#[tokio::main]
async fn main() {
    // initialize logging
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    info!(
        "Starting {} v{}...",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION")
    );

    let config_file_path = std::path::Path::new("config.yml");

    // setup config file watcher (sending events into a tokio-channel)
    let (watcher_tx, mut watcher_rx) = tokio::sync::mpsc::channel::<()>(10);
    let mut watcher =
        match notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
            Ok(event) => {
                if event.kind.is_modify() && watcher_tx.try_send(()).is_err() {
                    warn!("Failed to send file change event to main task?!");
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

    // start http-server
    let http_server = HttpServer::new();
    let server_task = http_server.run();
    tokio::pin!(server_task);

    let mut first_run = true;
    loop {
        // load configuration
        *http_server.registry.lock().await = None;
        let ingress = {
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
            match Ingress::from_config(&yaml_str) {
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
            }
        };

        // store the registry in the shared state with the http-server, so this instance will be marked as alive
        *http_server.registry.lock().await = Some(ingress.registry.clone());

        info!(
            "Configuration for ingress \"{}\" loaded: {:?}",
            ingress.record, ingress.endpoints
        );
        if ingress.has_telegram() {
            info!("Telegram notifications are enabled.");
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
            _ = Box::pin(ingress.run()) => {
                error!("Ingress-run task terminated unexpectedly?!");
                std::process::exit(2);
            },
            _ = Box::pin(hup_listener.recv()) => {
                // on SIGHUP, reload configuration
                // just let the loop continue
            }
            _ = Box::pin(watcher_rx.recv()) => {
                // on file change, reload configuration
                // just let the loop continue
            }
            e = &mut server_task => {
                error!("Server task terminated unexpectedly: {:?}", e);
                return;
            }
            _ = tokio::signal::ctrl_c() => {
                // the ingress-run task was already cancelled at this point
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
