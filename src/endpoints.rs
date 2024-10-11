use crate::integrations::dns::DnsConfiguration;
use crate::integrations::http::HyperHttpClient;
use log::{debug, error, info, warn};

#[derive(Debug)]
pub struct MonitoringConfiguration {
    /// if the URI-host matches the DNS-configuration record, then the host part of the URI is only being used for the SNI (server name indication) sent in the HTTP request, the actual IP is being taken from the DNS configuration (because its value would be used for the DNS entry and we want to monitor the actual reachability of the endpoint DNS values)
    pub uri: hyper::Uri,
    pub interval: std::time::Duration,
    /// if given, the HTTP reqponse must not only be 200, but also contain this secret
    pub marker: Option<String>,
    /// amount of consecutive successful requests required to mark the endpoint as healthy
    pub confidence: u8,
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
                if v < 1 || v > 255 {
                    return Err("Confidence must be between 1 and 255".to_string());
                }
                v as u8
            }
            None => return Err("Missing 'confidence' key".to_string()),
        };
        Ok(Self {
            uri,
            interval,
            marker,
            confidence,
        })
    }
}

#[derive(Debug)]
pub struct Endpoint {
    pub healthy: std::sync::atomic::AtomicBool,
    pub dns: DnsConfiguration,
    pub monitoring: Option<MonitoringConfiguration>,
    /// lower values mean higher priority
    pub weight: u8,
    /// if enabled, the endpoint will be removed after the specified time, if a higher priority endpoint is available
    pub sticky_duration: Option<std::time::Duration>,
}

impl Endpoint {
    pub fn from_yaml(yaml: &yaml_rust2::Yaml) -> Result<Self, String> {
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
            weight,
            sticky_duration,
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
                self._change_health(&self_arc, &change_tx, true).await;
                tokio::time::sleep(std::time::Duration::MAX).await; // sleep forever
                unreachable!("Sleeping forever should never return");
            }
        };
        assert!(monitoring.confidence > 0, "Confidence must be greater than 0, otherwise the endpoint will never be marked as unhealthy");
        assert!(monitoring.uri.host().is_some(), "URI must have a host");

        // initial resolve
        debug!("Resolving initial DNS values for endpoint {}", self);
        let mut last_dns_values = match self.dns.resolve().await {
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
                self._change_health(&self_arc, &change_tx, true).await;
            } else {
                self._change_health(&self_arc, &change_tx, false).await;
            }

            // sleep for the monitoring interval
            if !first_run {
                tokio::time::sleep(monitoring.interval).await;
            }
            first_run = false;

            // always resolve DNS-records values, if changed trigger update
            let new_dns_values = match self.dns.resolve().await {
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
                self._change_health(&self_arc, &change_tx, false).await;
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
            let client = HyperHttpClient::new(monitoring.uri.clone(), address_override);
            {
                let request = client
                    .builder()
                    .body(http_body_util::Empty::<bytes::Bytes>::new())
                    .unwrap();

                let response = match client.perform(request).await {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("Failed to perform request for endpoint {}: {:?}", self, e);
                        self._change_health(&self_arc, &change_tx, false).await;
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
                        info!("Marker not found in response body for endpoint {}", self);
                    }
                } else {
                    // no further checks, we got an OK response
                    confidence += 1;
                }
            }
        }
    }

    async fn _change_health(
        &self,
        self_arc: &EndpointArc,
        change_tx: &tokio::sync::mpsc::UnboundedSender<ChangeReason>,
        healthy: bool,
    ) {
        if self.healthy.load(std::sync::atomic::Ordering::Relaxed) == healthy {
            return; // no change
        }
        self.healthy
            .store(healthy, std::sync::atomic::Ordering::Relaxed);
        change_tx
            .send(ChangeReason::EndpointHealthChanged {
                endpoint: self_arc.clone(),
            })
            .unwrap();
    }
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "\"{}\"", self.dns.record)
    }
}

impl std::cmp::PartialEq for Endpoint {
    fn eq(&self, other: &Self) -> bool {
        self.dns.record == other.dns.record
    }
}

impl std::cmp::Eq for Endpoint {
    // we only compare the DNS record, as this is the unique identifier for an endpoint
}

impl std::hash::Hash for Endpoint {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.dns.record.hash(state);
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
        self.0.as_ref().dns.record.hash(state);
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
