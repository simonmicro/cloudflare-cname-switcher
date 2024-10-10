use crate::endpoints::{ChangeReason, Endpoint, EndpointArc};
use crate::integrations::{cloudflare::CloudflareConfiguration, telegram::TelegramConfiguration};
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
    pub fn from_yaml(yaml: &yaml_rust2::Yaml) -> Result<Self, String> {
        let record = match yaml["record"].as_str() {
            Some(v) => v.to_string(),
            None => {
                return Err("Missing record".to_string());
            }
        };
        let endpoints = match yaml["endpoints"].as_vec() {
            Some(v) => {
                let mut endpoints = std::collections::HashSet::new();
                for endpoint in v {
                    let endpoint = match Endpoint::from_yaml(&endpoint) {
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
        let cloudflare = match CloudflareConfiguration::from_yaml(&yaml["cloudflare"]) {
            Ok(v) => v,
            Err(e) => {
                return Err(format!("Failed to parse cloudflare: {}", e));
            }
        };
        let telegram = match yaml["telegram"].is_null() {
            true => None,
            false => match TelegramConfiguration::from_yaml(&yaml["telegram"]) {
                Ok(v) => Some(v),
                Err(e) => {
                    return Err(format!("Failed to parse telegram: {}", e));
                }
            },
        };
        Ok(Self {
            record,
            endpoints,
            cloudflare,
            telegram,
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

    pub async fn run(&mut self) {
        // create change-event channel MPSC for ChangeReason-items
        let (change_tx, mut change_rx) = tokio::sync::mpsc::unbounded_channel::<ChangeReason>();
        // tokio-spawn the monitor() for each endpoint
        // tokio::JoinSet all endpoints -> if any of those exit, we crash
        let mut endpoint_tasks = tokio::task::JoinSet::new();
        for endpoint in &self.endpoints {
            let endpoint = endpoint.clone();
            let change_tx = change_tx.clone();
            endpoint_tasks.spawn(async move {
                endpoint.monitor(endpoint.clone(), change_tx).await;
            });
        }
        type EndpointWithTimestampAndPrimary = (EndpointArc, std::time::SystemTime, bool);
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
                    let now = std::time::SystemTime::now();
                    // is the sticky duration already expired?
                    if now.duration_since(*timestamp).unwrap() <= *sticky_duration {
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
                                *sticky_duration - now.duration_since(*timestamp).unwrap()
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
                // for each primary sticky endpoint, select it too
                if *primary {
                    // → re-add them to the list of selected endpoints with current timestamp
                    new_active_endpoints.insert((
                        endpoint.clone(),
                        std::time::SystemTime::now(),
                        false,
                    ));
                    debug!("Selected sticky, primary endpoint: {:?}", endpoint);
                } else
                // for each non-primary check if their sticky duration expired, if so ignore
                if *timestamp + *sticky_duration > std::time::SystemTime::now() {
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
            info!("Updated backend to new endpoints: {:?}", endpoints);

            if let Some(telegram) = self.telegram.as_ref() {
                // queue telegram notification IF primary endpoint changed (not the address, but the record-names of the selected endpoints)
                if last_prioritized_endpoint.is_none()
                    || last_prioritized_endpoint.as_ref().unwrap().0 != new_prioritized_endpoint.0
                {
                    telegram
                        .queue_and_send(&format!(
                            "Primary endpoint changed to {}",
                            new_prioritized_endpoint.0.dns.record
                        ))
                        .await;
                }
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
