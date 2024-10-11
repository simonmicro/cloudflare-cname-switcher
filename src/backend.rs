use crate::endpoints::{ChangeReason, Endpoint, EndpointArc, EndpointMetrics};
use crate::integrations::{cloudflare::CloudflareConfiguration, telegram::TelegramConfiguration};
use log::{debug, error, info, warn};
use yaml_rust2;

pub struct Backend {
    /// FQDN
    pub record: String,
    pub endpoints: std::collections::HashSet<EndpointArc>,
    gauge_endpoint_selected: Box<prometheus::IntGaugeVec>,
    cloudflare: CloudflareConfiguration,
    telegram: Option<TelegramConfiguration>,
    registry: std::sync::Arc<prometheus::Registry>,
}

impl Backend {
    pub fn from_yaml(yaml: &yaml_rust2::Yaml) -> Result<Self, String> {
        let registry = prometheus::Registry::new();
        let record = match yaml["record"].as_str() {
            Some(v) => v.to_string(),
            None => {
                return Err("Missing record".to_string());
            }
        };
        let endpoints = match yaml["endpoints"].as_vec() {
            Some(v) => {
                let metrics = std::sync::Arc::new(EndpointMetrics::new(&registry));
                // parse endpoints
                let mut endpoints = std::collections::HashSet::new();
                for endpoint in v {
                    let endpoint = match Endpoint::from_yaml(&endpoint, metrics.clone()) {
                        Ok(v) => v,
                        Err(e) => {
                            return Err(format!("Failed to parse endpoint: {}", e));
                        }
                    };
                    endpoints.insert(EndpointArc::new(endpoint));
                }
                endpoints
            }
            None => {
                return Err("Missing endpoints".to_string());
            }
        };
        let gauge_endpoint_selected = {
            let gauge_endpoints_health_opts =
                prometheus::Opts::new("endpoint_selected", "Is the backend using this endpoint?");
            let gauge_endpoints_health = Box::new(
                prometheus::IntGaugeVec::new(gauge_endpoints_health_opts, &["name"]).unwrap(),
            );
            registry.register(gauge_endpoints_health.clone()).unwrap();
            gauge_endpoints_health
        };
        let cloudflare = match CloudflareConfiguration::from_yaml(&yaml["cloudflare"], &registry) {
            Ok(v) => v,
            Err(e) => {
                return Err(format!("Failed to parse cloudflare: {}", e));
            }
        };
        let telegram = match yaml["telegram"].is_null() {
            true => None,
            false => match TelegramConfiguration::from_yaml(&yaml["telegram"], &registry) {
                Ok(v) => Some(v),
                Err(e) => {
                    return Err(format!("Failed to parse telegram: {}", e));
                }
            },
        };
        Ok(Self {
            record,
            endpoints,
            gauge_endpoint_selected,
            cloudflare,
            telegram,
            registry: registry.into(),
        })
    }

    pub fn from_config(yaml_str: &str) -> Result<Self, String> {
        let yaml = match yaml_rust2::YamlLoader::load_from_str(&yaml_str) {
            Ok(v) => v,
            Err(e) => {
                return Err(format!("{}", e));
            }
        };
        if yaml.len() < 1 {
            return Err("Empty configuration file found".to_string());
        }
        let yaml = &yaml[0];
        // error if v1 configuration was found; show error and crash
        if yaml["general"]["timeout"].as_i64().is_some() {
            error!("==================================================");
            error!("            INCOMPATIBLE CONFIGURATION");
            error!("This version of the program will not work with the");
            error!("given configuration file. Either switch to the old");
            error!("version of the program (see Docker tags) or update");
            error!("the configuration file to the new format.");
            error!("==================================================");
            std::process::exit(1);
        }

        Self::from_yaml(yaml)
    }

    pub fn has_telegram(&self) -> bool {
        self.telegram.is_some()
    }

    pub async fn run(&mut self) {
        // create change-event channel MPSC for ChangeReason-items
        let (change_tx, mut change_rx) = tokio::sync::mpsc::unbounded_channel::<ChangeReason>();
        // tokio::JoinSet all endpoints -> if any of those exit, we crash
        let mut endpoint_tasks = tokio::task::JoinSet::new();
        for endpoint in &self.endpoints {
            let endpoint = endpoint.clone();
            let change_tx = change_tx.clone();
            endpoint_tasks.spawn(async move {
                endpoint.monitor(endpoint.clone(), change_tx).await;
            });
        }
        let server_task = Self::server(self.registry.clone());
        tokio::pin!(server_task);
        type EndpointWithTimestampAndPrimary = (EndpointArc, std::time::Instant, bool);
        let mut last_active_endpoints =
            std::collections::HashSet::<EndpointWithTimestampAndPrimary>::new();

        loop {
            // IF stickyness was active in last selected endpoints, we will wakeup on expired stickyness+1s (adding small delay to avoid not expireing stickyness)
            let mut due_to_sticky_expiring_wakeup_in = None;
            for (endpoint, timestamp, primary) in &last_active_endpoints {
                if *primary {
                    continue; // primary endpoints stickiness is not relevant
                }
                if let Some(sticky_duration) = endpoint.sticky_duration.as_ref() {
                    let now = std::time::Instant::now();
                    // is the sticky duration already expired?
                    if now.duration_since(*timestamp) <= *sticky_duration {
                        if let Some(scheduled_wakeup_duration) =
                            due_to_sticky_expiring_wakeup_in.as_ref()
                        {
                            // if already set, take the minimum of the two
                            due_to_sticky_expiring_wakeup_in = Some(std::cmp::min(
                                *scheduled_wakeup_duration,
                                *sticky_duration - now.duration_since(*timestamp)
                                    + std::time::Duration::from_secs(1),
                            ));
                        } else {
                            // if not set, set it
                            due_to_sticky_expiring_wakeup_in = Some(
                                *sticky_duration - now.duration_since(*timestamp)
                                    + std::time::Duration::from_secs(1),
                            );
                        }
                    } else {
                        // expired, schedule wakeup as soon as possible
                        due_to_sticky_expiring_wakeup_in = Some(std::time::Duration::from_secs(0));
                    }
                }
            }

            tokio::select! {
                // IF any change-reason was given, we will wakeup on that
                change_event = change_rx.recv() => {
                    let change_event = match change_event {
                        Some(v) => v,
                        None => {
                            error!("Change event channel closed unexpectedly?!");
                            break;
                        }
                    };
                    info!("Triggered by {}", change_event);
                    // → IF changed due to dns value change, ignore event if the causing endpoint is not in selected list
                    if let ChangeReason::EndpointDnsValuesChanged { endpoint } = change_event {
                        if !last_active_endpoints.iter().any(|(e, _, _)| *e == endpoint) {
                            info!("...ignoring DNS change event for non-selected endpoint");
                            continue;
                        }
                    }
                }
                // IF sticky duration expired, we will wakeup on that
                _ = tokio::time::sleep(due_to_sticky_expiring_wakeup_in.unwrap_or(std::time::Duration::MAX)) => {
                    info!("Triggered by stickyness of a non-primary selected endpoint expired");
                }
                // IF telegram has pending messages, sleep 30 seconds and then wake up
                _ = match self.telegram.as_ref() {
                    Some(ref telegram) => {
                        if telegram.has_pending() {
                            tokio::time::sleep(std::time::Duration::from_secs(30))
                        } else {
                            tokio::time::sleep(std::time::Duration::MAX)
                        }
                    },
                    None => tokio::time::sleep(std::time::Duration::MAX)
                } => {
                    debug!("Telegram has pending messages");
                    self.telegram.as_ref().unwrap().send().await;
                    continue;
                }
                _ = &mut server_task => {
                    error!("Server task terminated unexpectedly?!");
                    break;
                }
                // IF any endpoint exited, we will wakeup on that
                _ = endpoint_tasks.join_next() => {
                    error!("An endpoint-monitor task terminated unexpectedly?!");
                    break;
                }
            }

            // filter available enpoints to only healthy ones
            let healthy_endpoints: Vec<EndpointArc> = self
                .endpoints
                .iter()
                .filter(|e| e.healthy.load(std::sync::atomic::Ordering::Relaxed))
                .cloned()
                .collect();
            // select one of these endpoints with the lowest weight and add it to the list of new selected endpoints with timestamp now and primary true
            let new_prioritized_endpoint: EndpointWithTimestampAndPrimary;
            {
                let mut found_endpoint: Option<EndpointWithTimestampAndPrimary> = None;
                for endpoint in &healthy_endpoints {
                    if let Some((current_endpoint, _, _)) = found_endpoint.as_ref() {
                        if endpoint.weight < (*current_endpoint).weight {
                            found_endpoint =
                                Some((endpoint.clone(), std::time::Instant::now(), true));
                        }
                    } else {
                        found_endpoint = Some((endpoint.clone(), std::time::Instant::now(), true));
                    }
                }
                new_prioritized_endpoint = match found_endpoint {
                    Some(v) => v,
                    None => {
                        warn!("No healthy endpoints available, skipping update");
                        continue;
                    }
                };
            }
            debug!("Selected primary endpoint: {:?}", new_prioritized_endpoint);
            let mut new_active_endpoints = std::collections::HashSet::<
                EndpointWithTimestampAndPrimary,
            >::from([new_prioritized_endpoint.clone()]);

            // if previous selected endpoints contains sticky, healthy endpoints...
            let last_prioritized_endpoint =
                last_active_endpoints.iter().find(|(_, _, p)| *p).cloned();
            for (endpoint, timestamp, primary) in &last_active_endpoints {
                // check if the endpoint is still healthy
                if !endpoint.healthy.load(std::sync::atomic::Ordering::Relaxed) {
                    continue;
                }
                // check if the endpoint is sticky at all
                let sticky_duration = match endpoint.sticky_duration.as_ref() {
                    Some(v) => v,
                    None => continue, // no sticky duration, ignore
                };
                // check if this endpoint is already selected
                if new_active_endpoints.iter().any(|(e, _, _)| *e == *endpoint) {
                    continue;
                }
                // for each primary sticky endpoint, select it too
                if *primary {
                    // → re-add them to the list of selected endpoints with current timestamp
                    new_active_endpoints.insert((
                        endpoint.clone(),
                        std::time::Instant::now(),
                        false,
                    ));
                    debug!("Selected sticky, primary endpoint: {:?}", endpoint);
                } else
                // for each non-primary check if their sticky duration expired, if so ignore
                if *timestamp + *sticky_duration > std::time::Instant::now() {
                    // → re-add them to the list of selected endpoints with old timestamp
                    new_active_endpoints.insert((endpoint.clone(), *timestamp, false));
                    debug!("Selected sticky, non-primary endpoint: {:?}", endpoint);
                }
            }

            // update cloudflare
            let mut ok = false;
            let endpoints: std::collections::HashSet<EndpointArc> = new_active_endpoints
                .iter()
                .map(|(e, _, _)| e.clone())
                .collect();
            let ttl = new_active_endpoints
                .iter()
                .map(|(e, _, _)| e.dns.ttl)
                .min()
                .unwrap();
            for _ in 0..3 {
                let result = self
                    .cloudflare
                    .update(&self.record, endpoints.clone(), ttl)
                    .await;
                if result.is_ok() {
                    ok = true;
                    break;
                }
            }
            if !ok {
                error!("Failed multiple times to update Cloudflare, skipping update");
                continue;
            }

            {
                info!(
                    "Updated backend to new endpoints: {:?}",
                    endpoints
                        .iter()
                        .map(|e| &e.dns.record)
                        .collect::<Vec<&String>>()
                );
            }

            if let Some(telegram) = self.telegram.as_ref() {
                // queue telegram notification IF primary endpoint changed (not the address, but the record-names of the selected endpoints)
                if last_prioritized_endpoint.is_some()
                    && last_prioritized_endpoint.as_ref().unwrap().0 != new_prioritized_endpoint.0
                {
                    debug!("Sending telegram notification due to primary endpoint change");
                    telegram
                        .queue_and_send(&format!(
                            "Primary endpoint changed to {}",
                            TelegramConfiguration::escape(&new_prioritized_endpoint.0.dns.record)
                        ))
                        .await;
                }
            }

            // update gauge to reflect selected endpoints
            for endpoint in &self.endpoints {
                let selected = new_active_endpoints.iter().any(|(e, _, _)| *e == *endpoint);
                self.gauge_endpoint_selected
                    .with_label_values(&[&endpoint.dns.record])
                    .set(if selected { 1 } else { 0 });
            }

            // update last_active_endpoints
            last_active_endpoints = new_active_endpoints;
        }

        endpoint_tasks.abort_all(); // *abort* all other tasks

        // DOC three scenario →→ move this into doc-string
        // #1 primary non-stick, secondary stick
        // → 1 get unhealthy, 2 will be elected as only primary, 1 get back healthy, 1 will be elected as primary with 2 as stick until expire, 2 sticky expires, 1 will be elected as only primary
        // #2 primary non-stick, secondary stick, tertiary stick
        // → 1&2 get unhealthy, 3 will be elected as only primary, 2 get back healthy, 2 will be elected as primary with 3 as stick until expire, 1 get back healthy, 1 will be elected as primary with 2&3 as stick until expire, 2&3 sticky expire: 1 will be elected as only primary
        // #3 pimary non-stick, secondary stick
        // → 1 get unhealthy, 2 will be elected as only primary, 1 get back healthy, 1 will be elected as primary with 2 as stick until expire, 1 get unhealthy, 2 will be elected as only primary (not sticky with itself...)
    }

    async fn server(registry: std::sync::Arc<prometheus::Registry>) -> Result<(), String> {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 3000));
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| e.to_string())?;
        info!("Listening on http://{}", addr);

        loop {
            let (stream, _) = listener.accept().await.map_err(|e| e.to_string())?;
            let io = hyper_util::rt::TokioIo::new(stream);

            // for each client spawn a new task
            let registry = registry.clone();
            tokio::task::spawn(async move {
                // note that one client with one connection, may send multiple requests -> service_fn must be FN instead of FnOnce
                if let Err(err) = hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        io,
                        hyper::service::service_fn(
                            move |req: hyper::Request<hyper::body::Incoming>| {
                                let registry = registry.clone();
                                async move { Self::serve_client(req, registry).await }
                            },
                        ),
                    )
                    .await
                {
                    warn!("Error serving connection: {:?}", err);
                }
            });
        }
    }

    async fn serve_client(
        req: hyper::Request<hyper::body::Incoming>,
        registry: std::sync::Arc<prometheus::Registry>,
    ) -> Result<hyper::Response<http_body_util::Full<bytes::Bytes>>, std::convert::Infallible> {
        match (req.method(), req.uri().path()) {
            (&hyper::http::Method::GET, "/healthz") => Self::serve_healthz().await,
            (&hyper::http::Method::GET, "/metrics") => Self::serve_metrics(registry).await,
            _ => Ok(hyper::Response::builder()
                .status(hyper::http::StatusCode::NOT_FOUND)
                .body(http_body_util::Full::new(bytes::Bytes::from("Not Found")))
                .unwrap()),
        }
    }

    async fn serve_healthz(
    ) -> Result<hyper::Response<http_body_util::Full<bytes::Bytes>>, std::convert::Infallible> {
        // nothing to check, if the server is up, we are healthy
        Ok(hyper::Response::new(http_body_util::Full::new(
            bytes::Bytes::from("OK"),
        )))
    }

    async fn serve_metrics(
        registry: std::sync::Arc<prometheus::Registry>,
    ) -> Result<hyper::Response<http_body_util::Full<bytes::Bytes>>, std::convert::Infallible> {
        // create the buffer
        let encoder = prometheus::TextEncoder::new();
        let metric_families = registry.gather();
        let response_str = encoder.encode_to_string(&metric_families).unwrap();
        // create the response
        Ok(hyper::Response::new(http_body_util::Full::new(
            bytes::Bytes::from(response_str),
        )))
    }
}
