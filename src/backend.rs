use crate::endpoints::{ChangeReason, Endpoint, EndpointArc, MonitoringConfiguration};
use crate::integrations::{
    cloudflare::CloudflareConfiguration, dns::DnsConfiguration, telegram::TelegramConfiguration,
};
use log::{debug, error, info, warn};
use yaml_rust2;

pub struct Backend {
    /// FQDN
    record: String,
    endpoints: std::collections::HashSet<EndpointArc>,
    cloudflare: CloudflareConfiguration,
    telegram: Option<TelegramConfiguration>,
}

impl Backend {
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
        // TODO parse the configuration
        Ok(Self {
            record: "_backend.example.com".to_string(),
            endpoints: std::collections::HashSet::from([
                EndpointArc::new(Endpoint {
                    healthy: std::sync::atomic::AtomicBool::new(false),
                    dns: DnsConfiguration {
                        record: "_service_1.example.com".to_string(),
                        ttl: 60,
                        resolver: "1.1.1.1".to_string(),
                    },
                    monitoring: Some(MonitoringConfiguration {
                        confidence: 3,
                        uri: "http://_service_1.example.com/"
                            .parse::<hyper::Uri>()
                            .unwrap(),
                        interval: std::time::Duration::from_secs(10),
                        marker: None,
                    }),
                    sticky_duration: None,
                    weight: 10,
                }),
                EndpointArc::new(Endpoint {
                    healthy: std::sync::atomic::AtomicBool::new(false),
                    dns: DnsConfiguration {
                        record: "_service_2.example.com".to_string(),
                        ttl: 300,
                        resolver: "1.1.1.1".to_string(),
                    },
                    monitoring: None,
                    sticky_duration: None,
                    weight: 20,
                }),
            ]),
            cloudflare: CloudflareConfiguration::new("test".to_string(), "test".to_string()),
            telegram: Some(TelegramConfiguration::new("test".to_string(), 123456789)),
        })
    }

    pub async fn run(&mut self) {
        // create change-event channel MPSC for ChangeReason-items
        let (change_tx, mut change_rx) = tokio::sync::mpsc::unbounded_channel::<ChangeReason>();
        // tokio-spawn the monitor() for each endpoint
        // tokio::JoinSet all endpoints -> if any of those exit, we crash
        let mut endpoint_tasks = tokio::task::JoinSet::new();
        for endpoint in &self.endpoints {
            let endpoint = endpoint.clone();
            let change_tx = change_tx.clone();
            endpoint_tasks.spawn(tokio::spawn(async move {
                endpoint.monitor(endpoint.clone(), change_tx).await;
            }));
        }
        type EndpointWithTimestampAndPrimary = (EndpointArc, std::time::SystemTime, bool);
        let mut last_active_endpoints =
            std::collections::HashSet::<EndpointWithTimestampAndPrimary>::new();
        let mut first_run = true;

        loop {
            // IF first run, instantly update
            if !first_run {
                // IF stickyness was active in last selected endpoints, we will wakeup on expired stickyness+1s (adding small delay to avoid not expireing stickyness)
                let mut due_to_sticky_expiring_wakeup_in = None;
                for (endpoint, timestamp, primary) in &last_active_endpoints {
                    if *primary {
                        continue; // primary endpoints stickiness is not relevant
                    }
                    if let Some(sticky_duration) = endpoint.sticky_duration.as_ref() {
                        let now = std::time::SystemTime::now();
                        // is the sticky duration already expired?
                        if now.duration_since(*timestamp).unwrap() <= *sticky_duration {
                            // not expired yet
                            if let Some(scheduled_wakeup_duration) =
                                due_to_sticky_expiring_wakeup_in.as_ref()
                            {
                                // if already set, take the minimum of the two
                                due_to_sticky_expiring_wakeup_in = Some(std::cmp::min(
                                    *scheduled_wakeup_duration,
                                    *sticky_duration - now.duration_since(*timestamp).unwrap()
                                        + std::time::Duration::from_secs(1),
                                ));
                            } else {
                                // if not set, set it
                                due_to_sticky_expiring_wakeup_in = Some(
                                    now.duration_since(*timestamp).unwrap() - *sticky_duration
                                        + std::time::Duration::from_secs(1),
                                );
                            }
                        } else {
                            // expired, schedule wakeup as soon as possible
                            due_to_sticky_expiring_wakeup_in =
                                Some(std::time::Duration::from_secs(0));
                        }
                    }
                }

                tokio::select! {
                    // IF any change-reason was given, we will wakeup on that
                    change_event = change_rx.recv() => {
                        info!("Change event: {:?}", change_event);
                        // → IF changed due to dns value change, ignore event if the causing endpoint is not in selected list
                        if let Some(ChangeReason::EndpointDnsValuesChanged { endpoint }) = change_event {
                            if !last_active_endpoints.iter().any(|(e, _, _)| *e == endpoint) {
                                info!("Ignoring DNS change event for non-selected endpoint");
                                continue;
                            }
                        }
                    }
                    // IF sticky duration expired, we will wakeup on that
                    _ = tokio::time::sleep(due_to_sticky_expiring_wakeup_in.unwrap_or(std::time::Duration::MAX)) => {
                        info!("Stickyness of a non-primary selected endpoint expired");
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
                    // IF any endpoint exited, we will wakeup on that
                    _ = endpoint_tasks.join_next() => {
                        error!("Endpoint-monitor task exited unexpectedly?!");
                        break;
                    }
                }
            } else {
                info!("Performing initial update after start...");
                first_run = false;
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
                                Some((endpoint.clone(), std::time::SystemTime::now(), true));
                        }
                    } else {
                        found_endpoint =
                            Some((endpoint.clone(), std::time::SystemTime::now(), true));
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
            let mut new_active_endpoints = std::collections::HashSet::<
                EndpointWithTimestampAndPrimary,
            >::from([new_prioritized_endpoint.clone()]);

            // if previous selected endpoints contains sticky, healthy endpoints...
            let last_prioritized_endpoint =
                last_active_endpoints.iter().find(|(_, _, p)| *p).cloned();
            for (endpoint, timestamp, primary) in &last_active_endpoints {
                // → for each non-primary check if their sticky duration expired, if so ignore
                if !primary
                    && *timestamp + *endpoint.sticky_duration.as_ref().unwrap()
                        <= std::time::SystemTime::now()
                {
                    // → re-add them to the list of selected endpoints with old timestamp and primary false
                    new_active_endpoints.insert((endpoint.clone(), *timestamp, false));
                }
            }

            if let Some(telegram) = self.telegram.as_ref() {
                // queue telegram notification IF primary endpoint changed (not the address, but the record-names of the selected endpoints)
                if last_prioritized_endpoint.is_none()
                    || last_prioritized_endpoint.unwrap().0 != new_prioritized_endpoint.0
                {
                    telegram
                        .queue_and_send(&format!(
                            "Primary endpoint changed to {}",
                            new_prioritized_endpoint.0.dns.record
                        ))
                        .await;
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
}
