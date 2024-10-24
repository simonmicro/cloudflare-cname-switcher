use crate::integrations::http::HyperHttpClient;
use crate::integrations::{dns::DnsConfiguration, telegram::TelegramConfiguration};
use log::{debug, error, warn};

#[derive(Debug)]
pub struct EndpointMetrics {
    endpoints_health: Box<prometheus::IntGaugeVec>,
    endpoint_durations: Box<prometheus::GaugeVec>,
}

impl EndpointMetrics {
    pub fn new(registry: &prometheus::Registry) -> Self {
        let opts = prometheus::Opts::new("endpoint_health", "Is the endpoint marked as healthy?");
        let endpoints_health = Box::new(prometheus::IntGaugeVec::new(opts, &["name"]).unwrap());
        registry.register(endpoints_health.clone()).unwrap();
        let opts =
            prometheus::Opts::new("endpoint_durations_seconds", "How long took which phase?");
        let endpoint_durations =
            Box::new(prometheus::GaugeVec::new(opts, &["name", "phase"]).unwrap());
        registry.register(endpoint_durations.clone()).unwrap();
        Self {
            endpoints_health,
            endpoint_durations,
        }
    }
}

#[derive(Debug)]
pub struct MonitoringConfiguration {
    /// if the URI-host matches the DNS-configuration record, then the host part of the URI is only being used for the SNI (server name indication) sent in the HTTP request, the actual IP is being taken from the DNS configuration (because its value would be used for the DNS entry and we want to monitor the actual reachability of the endpoint DNS values)
    pub uri: hyper::Uri,
    pub interval: std::time::Duration,
    /// if given, the HTTP reqponse must not only be 200, but also contain this secret
    pub marker: Option<String>,
    /// amount of consecutive successful requests required to mark the endpoint as healthy
    pub confidence: u8,
    /// how long to wait for http requests
    pub timeout: std::time::Duration,
    /// how often to retry the HTTP request
    pub retry: u8,
    /// will be set to the last reason why the endpoint was marked as unhealthy
    last_problem: std::sync::Mutex<Option<String>>,
}

impl MonitoringConfiguration {
    fn from_yaml(yaml: &yaml_rust2::Yaml) -> Result<Self, String> {
        let uri = match yaml["uri"].as_str() {
            Some(v) => match v.parse() {
                Ok(v) => v,
                Err(e) => return Err(format!("Failed to parse URI: {:?}", e)),
            },
            None => return Err("Missing 'uri' key".to_string()),
        };
        let interval = match yaml["interval"].as_i64() {
            Some(v) => std::time::Duration::from_secs(v as u64),
            None => return Err("Missing 'interval' key".to_string()),
        };
        let marker = match yaml["marker"].as_str() {
            Some(v) => Some(v.to_string()),
            None => None,
        };
        let confidence = match yaml["confidence"].as_i64() {
            Some(v) => {
                if v < 1 || v > std::u8::MAX as i64 {
                    return Err("Confidence is out of bounds".to_string());
                }
                v as u8
            }
            None => return Err("Missing 'confidence' key".to_string()),
        };
        let timeout = match yaml["timeout"].as_i64() {
            Some(v) => std::time::Duration::from_secs(v as u64),
            None => std::time::Duration::from_secs(5),
        };
        let retry = match yaml["retry"].as_i64() {
            Some(v) => {
                if v < 0 || v > std::u8::MAX as i64 {
                    return Err("Retry is out of bounds".to_string());
                }
                v as u8
            }
            None => 0,
        };
        Ok(Self {
            uri,
            interval,
            marker,
            confidence,
            timeout,
            retry,
            last_problem: std::sync::Mutex::new(None),
        })
    }
}

#[derive(Debug)]
pub struct Endpoint {
    pub healthy: std::sync::atomic::AtomicBool,
    pub dns: DnsConfiguration,
    pub monitoring: Option<MonitoringConfiguration>,
    pub name: String,
    /// lower values mean higher priority
    pub weight: u8,
    /// if enabled, the endpoint will be removed after the specified time, if a higher priority endpoint is available
    pub sticky_duration: Option<std::time::Duration>,
    metrics: std::sync::Arc<EndpointMetrics>,
}

impl Endpoint {
    pub fn from_yaml(
        yaml: &yaml_rust2::Yaml,
        metrics: std::sync::Arc<EndpointMetrics>,
    ) -> Result<Self, String> {
        let dns = match DnsConfiguration::from_yaml(&yaml["dns"]) {
            Ok(v) => v,
            Err(e) => return Err(format!("Failed to parse DNS configuration: {:?}", e)),
        };
        let monitoring = match yaml["monitoring"].is_null() {
            true => None,
            false => Some(
                match MonitoringConfiguration::from_yaml(&yaml["monitoring"]) {
                    Ok(v) => v,
                    Err(e) => {
                        return Err(format!("Failed to parse monitoring configuration: {:?}", e))
                    }
                },
            ),
        };
        let name = yaml["alias"]
            .as_str()
            .or_else(|| Some(&dns.record))
            .unwrap()
            .to_string();
        let healthy = std::sync::atomic::AtomicBool::new(false);
        let weight = match yaml["weight"].as_i64() {
            Some(v) => {
                if v < 0 || v > 255 {
                    return Err("Weight must be between 0 and 255".to_string());
                }
                v as u8
            }
            None => 0,
        };
        let sticky_duration = match yaml["sticky_duration"].as_i64() {
            Some(v) => Some(std::time::Duration::from_secs(v as u64)),
            None => None,
        };
        Ok(Self {
            healthy,
            dns,
            monitoring,
            name,
            weight,
            sticky_duration,
            metrics,
        })
    }

    pub async fn monitor(
        &self,
        self_arc: EndpointArc,
        change_tx: tokio::sync::mpsc::UnboundedSender<ChangeReason>,
    ) {
        let monitoring = match self.monitoring.as_ref() {
            Some(v) => v,
            None => {
                // if no monitoring is configured, we assume the endpoint is healthy
                self.change_health(&self_arc, Some(&change_tx), true).await;
                tokio::time::sleep(std::time::Duration::MAX).await; // sleep forever
                unreachable!("Sleeping forever should never return");
            }
        };
        assert!(monitoring.confidence > 0, "Confidence must be greater than 0, otherwise the endpoint will never be marked as unhealthy");
        assert!(monitoring.uri.host().is_some(), "URI must have a host");
        self.change_health(&self_arc, None, false).await; // initial unhealthy state

        // initial resolve
        debug!("Resolving initial DNS values for endpoint {}", self);
        let mut last_dns_values = match self.resolve_dns().await {
            Ok(v) => v,
            Err(e) => {
                error!(
                    "Failed to resolve initial DNS values for endpoint {}: {:?}",
                    self, e
                );
                return;
            }
        };

        let mut confidence = 0;
        let mut first_run = true;
        loop {
            // apply current confidence to health status
            if confidence >= monitoring.confidence {
                self.change_health(&self_arc, Some(&change_tx), true).await;
                confidence = monitoring.confidence; // prevent overflow
            } else {
                self.change_health(&self_arc, Some(&change_tx), false).await;
            }

            // sleep for the monitoring interval
            if !first_run {
                tokio::time::sleep(monitoring.interval).await;
            }
            first_run = false;

            // always resolve DNS-records values, if changed trigger update
            let new_dns_values = match self.resolve_dns().await {
                Ok(v) => v,
                Err(e) => {
                    warn!(
                        "Failed to resolve DNS values for endpoint {}: {:?}",
                        self, e
                    );
                    continue;
                }
            };
            if last_dns_values != new_dns_values {
                change_tx
                    .send(ChangeReason::EndpointDnsValuesChanged {
                        endpoint: self_arc.clone(),
                    })
                    .unwrap();
            }

            // update last_dns_values
            last_dns_values = new_dns_values;

            // no values, no monitoring
            if last_dns_values.len() == 0 {
                warn!("No DNS values for endpoint \"{}\"", self);
                monitoring
                    .last_problem
                    .lock()
                    .unwrap()
                    .replace("no DNS values".to_string());
                confidence = 0;
                continue;
            }

            // determine socket address: if uri.host==dns.record, then use ip from before; otherwise ask OS
            let address_override = match self.dns.record == monitoring.uri.host().unwrap() {
                true => {
                    debug!("Monitoring {} via address-override", monitoring.uri);
                    Some(*last_dns_values.iter().next().unwrap())
                }
                false => {
                    debug!("Monitoring {} via DNS resolution", monitoring.uri);
                    None
                }
            };

            // then check the endpoint
            let client = HyperHttpClient::new(
                monitoring.uri.clone(),
                monitoring.timeout,
                monitoring.retry,
                address_override,
            );
            {
                let request = client
                    .builder()
                    .body(http_body_util::Empty::<bytes::Bytes>::new())
                    .unwrap();

                let response = {
                    let start = std::time::Instant::now();
                    let res = client.perform(request).await;
                    let duration = start.elapsed().as_secs_f64();
                    self.metrics
                        .endpoint_durations
                        .with_label_values(&[&self.name, "request"])
                        .set(duration);
                    res
                };
                let response = match response {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("Failed to perform request for endpoint {}: {:?}", self, e);
                        monitoring
                            .last_problem
                            .lock()
                            .unwrap()
                            .replace(format!("HTTP error: {}", e));
                        confidence = 0;
                        continue;
                    }
                };

                if monitoring.marker.is_some() {
                    // Stream the body, writing each frame to stdout as it arrives
                    if response.contains(monitoring.marker.as_ref().unwrap()) {
                        confidence += 1;
                    } else {
                        confidence = 0;
                        debug!("Marker not found in response body for endpoint {}", self);
                    }
                } else {
                    // no further checks, we got an OK response
                    confidence += 1;
                }
            }
        }
    }

    async fn change_health(
        &self,
        self_arc: &EndpointArc,
        change_tx: Option<&tokio::sync::mpsc::UnboundedSender<ChangeReason>>,
        healthy: bool,
    ) {
        if change_tx.is_some() && self.healthy.load(std::sync::atomic::Ordering::Relaxed) == healthy
        {
            return; // no change
        }
        self.healthy
            .store(healthy, std::sync::atomic::Ordering::Relaxed);
        self.metrics
            .endpoints_health
            .with_label_values(&[&self.name])
            .set(healthy as i64);
        if let Some(change_tx) = change_tx {
            change_tx
                .send(ChangeReason::EndpointHealthChanged {
                    endpoint: self_arc.clone(),
                })
                .unwrap();
        }
    }

    pub async fn resolve_dns(
        &self,
    ) -> Result<std::collections::HashSet<std::net::IpAddr>, crate::integrations::dns::DnsError>
    {
        let start = std::time::Instant::now();
        let res = self.dns.resolve().await;
        let duration = start.elapsed().as_secs_f64();
        self.metrics
            .endpoint_durations
            .with_label_values(&[&self.name, "dns"])
            .set(duration);
        res
    }

    pub fn to_telegram_string(&self) -> String {
        let healthy = self.healthy.load(std::sync::atomic::Ordering::Relaxed);
        let mut res = match healthy {
            true => format!("✅ `{}`", TelegramConfiguration::escape(&self.name)),
            false => format!("❌ `{}`", TelegramConfiguration::escape(&self.name)),
        };
        if let Some(monitoring) = self.monitoring.as_ref() {
            res += &TelegramConfiguration::escape(&format!(
                " (every {}s",
                monitoring.interval.as_secs(),
            ));
            if monitoring.confidence > 1 {
                res += &TelegramConfiguration::escape(&format!(
                    ", confidence of {}",
                    monitoring.confidence
                ));
            }
            if !healthy {
                if let Some(detail) = monitoring.last_problem.lock().unwrap().as_ref() {
                    res += &TelegramConfiguration::escape(&detail);
                }
            }
            res += &TelegramConfiguration::escape(&format!(")",));
        }
        res
    }
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "\"{}\"", self.name)
    }
}

impl std::cmp::PartialEq for Endpoint {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl std::cmp::Eq for Endpoint {
    // we only compare the DNS record, as this is the unique identifier for an endpoint
}

impl std::hash::Hash for Endpoint {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.name.hash(state);
    }
}
#[derive(Debug, Clone, std::cmp::Eq)]
pub struct EndpointArc(std::sync::Arc<Endpoint>);

impl EndpointArc {
    pub fn new(endpoint: Endpoint) -> Self {
        Self(std::sync::Arc::new(endpoint))
    }
}

impl std::cmp::PartialEq for EndpointArc {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl std::ops::Deref for EndpointArc {
    type Target = Endpoint;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref()
    }
}

impl std::hash::Hash for EndpointArc {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.as_ref().name.hash(state);
    }
}

#[derive(Debug)]
pub enum ChangeReason {
    EndpointHealthChanged { endpoint: EndpointArc },
    EndpointDnsValuesChanged { endpoint: EndpointArc },
}

impl std::fmt::Display for ChangeReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EndpointHealthChanged { endpoint } => {
                write!(f, "EndpointHealthChanged: {}", endpoint.to_string())
            }
            Self::EndpointDnsValuesChanged { endpoint } => {
                write!(f, "EndpointDnsValuesChanged: {}", endpoint.to_string())
            }
        }
    }
}
