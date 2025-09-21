use crate::endpoints::EndpointArc;
use crate::integrations::dns::DnsError;
use crate::integrations::http::{HyperHttpClient, HyperHttpClientError};
use log::debug;
use std::collections::LinkedList;

#[derive(Debug)]
pub enum CloudflareApiError {
    Http(HyperHttpClientError),
    JsonParseError(serde_json::Error),
    SchemaParseError,
}

#[derive(Debug)]
pub enum CloudflareUpdateError {
    ApiError(CloudflareApiError),
    DnsError(DnsError),
    Conflict,
}

enum CloudflareDnsValues {
    CName(String),
    CNameWithSticky(std::collections::HashSet<std::net::IpAddr>),
}

impl std::cmp::PartialEq for CloudflareDnsValues {
    fn eq(&self, other: &Self) -> bool {
        if !self.same_type(other) {
            return false;
        }
        match (self, other) {
            (CloudflareDnsValues::CName(a), CloudflareDnsValues::CName(b)) => a == b,
            (CloudflareDnsValues::CNameWithSticky(a), CloudflareDnsValues::CNameWithSticky(b)) => {
                a == b
            }
            _ => false,
        }
    }
}

impl CloudflareDnsValues {
    pub fn same_type(&self, other: &Self) -> bool {
        match (self, other) {
            (CloudflareDnsValues::CName(_), CloudflareDnsValues::CName(_)) => true,
            (CloudflareDnsValues::CNameWithSticky(_), CloudflareDnsValues::CNameWithSticky(_)) => {
                true
            }
            _ => false,
        }
    }
}

/// NEVER allow debug output of this struct, as it contains sensitive information
pub struct CloudflareConfiguration {
    zone_id: String,
    token: String,
    status_cache: std::sync::Mutex<Option<CloudflareDnsValues>>,
    gauge_update_duration: Option<Box<prometheus::Gauge>>,
}

impl CloudflareConfiguration {
    pub fn from_yaml(
        yaml: &yaml_rust2::Yaml,
        registry: &prometheus::Registry,
    ) -> Result<Self, String> {
        let zone_id = yaml["zone_id"]
            .as_str()
            .ok_or("zone_id is not a string")?
            .to_string();
        let token = yaml["token"]
            .as_str()
            .ok_or("token is not a string")?
            .to_string();
        let gauge_update_duration = Box::new(
            prometheus::Gauge::new(
                "cloudflare_update_seconds",
                "Duration of last cloudflare update",
            )
            .unwrap(),
        );
        registry.register(gauge_update_duration.clone()).unwrap();
        Ok(Self {
            zone_id,
            token,
            status_cache: None.into(),
            gauge_update_duration: Some(gauge_update_duration),
        })
    }

    async fn name_to_record_ids(
        &self,
        name: &str,
    ) -> Result<LinkedList<String>, CloudflareApiError> {
        let uri = format!(
            "https://api.cloudflare.com/client/v4/zones/{}/dns_records?name={}",
            self.zone_id, name
        )
        .parse::<hyper::Uri>()
        .unwrap();
        let client = HyperHttpClient::new(uri, std::time::Duration::from_secs(10), 0, None);
        let request = client
            .builder()
            .header(
                hyper::header::AUTHORIZATION,
                format!("Bearer {}", self.token),
            )
            .header(hyper::header::CONTENT_TYPE, "application/json")
            .body(http_body_util::Empty::<bytes::Bytes>::new())
            .unwrap();
        let response = client
            .perform(request)
            .await
            .map_err(CloudflareApiError::Http)?;

        let json: serde_json::Value =
            serde_json::from_str(&response).map_err(CloudflareApiError::JsonParseError)?;
        let res_array = json
            .as_object()
            .ok_or(CloudflareApiError::SchemaParseError)?
            .get("result")
            .ok_or(CloudflareApiError::SchemaParseError)?
            .as_array()
            .ok_or(CloudflareApiError::SchemaParseError)?;
        let mut result = LinkedList::new();
        for record in res_array {
            let r_id = record
                .as_object()
                .ok_or(CloudflareApiError::SchemaParseError)?
                .get("id")
                .ok_or(CloudflareApiError::SchemaParseError)?
                .as_str()
                .ok_or(CloudflareApiError::SchemaParseError)?;
            result.push_back(r_id.to_string());
        }
        Ok(result)
    }

    fn record_comment(&self) -> String {
        format!(
            "Managed by {} v{}",
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION")
        )
    }

    async fn create_record(
        &self,
        name: &str,
        r#type: &str,
        content: &str,
        ttl: &u16,
    ) -> Result<String, CloudflareApiError> {
        let data = serde_json::Value::Object(serde_json::Map::from_iter([
            (
                "type".to_string(),
                serde_json::Value::String(r#type.to_string()),
            ),
            (
                "name".to_string(),
                serde_json::Value::String(name.to_string()),
            ),
            (
                "content".to_string(),
                serde_json::Value::String(content.to_string()),
            ),
            (
                "ttl".to_string(),
                serde_json::Value::Number(serde_json::Number::from(*ttl)),
            ),
            (
                "comment".to_string(),
                serde_json::Value::String(self.record_comment()),
            ),
        ]));

        let uri = format!(
            "https://api.cloudflare.com/client/v4/zones/{}/dns_records",
            self.zone_id
        )
        .parse::<hyper::Uri>()
        .unwrap();
        let client = HyperHttpClient::new(uri, std::time::Duration::from_secs(10), 0, None);
        let request = client
            .builder()
            .method(hyper::Method::POST)
            .header(
                hyper::header::AUTHORIZATION,
                format!("Bearer {}", self.token),
            )
            .header(hyper::header::CONTENT_TYPE, "application/json")
            .body(http_body_util::Full::<bytes::Bytes>::from(
                serde_json::to_vec(&data).unwrap(),
            ))
            .unwrap();
        let response = client
            .perform(request)
            .await
            .map_err(CloudflareApiError::Http)?;

        let json: serde_json::Value =
            serde_json::from_str(&response).map_err(CloudflareApiError::JsonParseError)?;
        let id = json
            .as_object()
            .ok_or(CloudflareApiError::SchemaParseError)?
            .get("result")
            .ok_or(CloudflareApiError::SchemaParseError)?
            .as_object()
            .ok_or(CloudflareApiError::SchemaParseError)?
            .get("id")
            .ok_or(CloudflareApiError::SchemaParseError)?
            .as_str()
            .ok_or(CloudflareApiError::SchemaParseError)?;
        Ok(id.to_string())
    }

    async fn create_record_cname(
        &self,
        name: &str,
        content: &str,
        ttl: &u16,
    ) -> Result<String, CloudflareApiError> {
        self.create_record(name, "CNAME", content, ttl).await
    }

    async fn create_record_a_or_aaaa(
        &self,
        name: &str,
        content: &std::net::IpAddr,
        ttl: &u16,
    ) -> Result<String, CloudflareApiError> {
        match content {
            std::net::IpAddr::V4(ip) => self.create_record(name, "A", &ip.to_string(), ttl).await,
            std::net::IpAddr::V6(ip) => {
                self.create_record(name, "AAAA", &ip.to_string(), ttl).await
            }
        }
    }

    async fn update_record_cname(
        &self,
        name: &str,
        record_id: &str,
        content: &str,
        ttl: &u16,
    ) -> Result<String, CloudflareApiError> {
        let data = serde_json::Value::Object(serde_json::Map::from_iter([
            (
                "type".to_string(),
                serde_json::Value::String("CNAME".to_string()),
            ),
            (
                "name".to_string(),
                serde_json::Value::String(name.to_string()),
            ),
            (
                "content".to_string(),
                serde_json::Value::String(content.to_string()),
            ),
            (
                "ttl".to_string(),
                serde_json::Value::Number(serde_json::Number::from(*ttl)),
            ),
            (
                "comment".to_string(),
                serde_json::Value::String(self.record_comment()),
            ),
        ]));

        let uri = format!(
            "https://api.cloudflare.com/client/v4/zones/{}/dns_records/{}",
            self.zone_id, record_id
        )
        .parse::<hyper::Uri>()
        .unwrap();
        let client = HyperHttpClient::new(uri, std::time::Duration::from_secs(10), 0, None);
        let request = client
            .builder()
            .method(hyper::Method::PATCH)
            .header(
                hyper::header::AUTHORIZATION,
                format!("Bearer {}", self.token),
            )
            .header(hyper::header::CONTENT_TYPE, "application/json")
            .body(http_body_util::Full::<bytes::Bytes>::from(
                serde_json::to_vec(&data).unwrap(),
            ))
            .unwrap();
        let response = client
            .perform(request)
            .await
            .map_err(CloudflareApiError::Http)?;

        let json: serde_json::Value =
            serde_json::from_str(&response).map_err(CloudflareApiError::JsonParseError)?;
        let id = json
            .as_object()
            .ok_or(CloudflareApiError::SchemaParseError)?
            .get("result")
            .ok_or(CloudflareApiError::SchemaParseError)?
            .as_object()
            .ok_or(CloudflareApiError::SchemaParseError)?
            .get("id")
            .ok_or(CloudflareApiError::SchemaParseError)?
            .as_str()
            .ok_or(CloudflareApiError::SchemaParseError)?;
        Ok(id.to_string())
    }

    async fn delete_record(&self, record_id: &str) -> Result<(), CloudflareApiError> {
        let uri = format!(
            "https://api.cloudflare.com/client/v4/zones/{}/dns_records/{}",
            self.zone_id, record_id
        )
        .parse::<hyper::Uri>()
        .unwrap();
        let client = HyperHttpClient::new(uri, std::time::Duration::from_secs(10), 0, None);
        let request = client
            .builder()
            .method(hyper::Method::DELETE)
            .header(
                hyper::header::AUTHORIZATION,
                format!("Bearer {}", self.token),
            )
            .header(hyper::header::CONTENT_TYPE, "application/json")
            .body(http_body_util::Empty::<bytes::Bytes>::new())
            .unwrap();
        let response = client
            .perform(request)
            .await
            .map_err(CloudflareApiError::Http)?;

        let json: serde_json::Value =
            serde_json::from_str(&response).map_err(CloudflareApiError::JsonParseError)?;
        let id = json
            .as_object()
            .ok_or(CloudflareApiError::SchemaParseError)?
            .get("result")
            .ok_or(CloudflareApiError::SchemaParseError)?
            .as_object()
            .ok_or(CloudflareApiError::SchemaParseError)?
            .get("id")
            .ok_or(CloudflareApiError::SchemaParseError)?
            .as_str()
            .ok_or(CloudflareApiError::SchemaParseError)?;
        assert!(id == record_id);
        Ok(())
    }

    pub fn new(token: String, zone_id: String) -> Self {
        Self {
            zone_id,
            token,
            status_cache: None.into(),
            gauge_update_duration: None,
        }
    }

    /// if multiple endpoints are given, they will result in multiple A/AAAA records (set their TTL to lowest of all endpoints), otherwise just a single CNAME record with endpoints TTL will be applied
    pub async fn inner_update(
        &self,
        record: &str,
        selected_endpoints: std::collections::HashSet<EndpointArc>,
        ttl: u16,
    ) -> Result<(), CloudflareUpdateError> {
        assert!(
            !selected_endpoints.is_empty(),
            "You must provide at least one endpoint"
        );
        // calculate the new state
        let state;
        if selected_endpoints.len() == 1 {
            state = CloudflareDnsValues::CName(
                selected_endpoints.iter().next().unwrap().dns.record.clone(),
            );
        } else {
            let mut ips = std::collections::HashSet::<std::net::IpAddr>::new();
            for endpoint in selected_endpoints {
                let resolved = endpoint
                    .resolve_dns()
                    .await
                    .map_err(CloudflareUpdateError::DnsError)?;
                ips.extend(resolved);
            }
            state = CloudflareDnsValues::CNameWithSticky(ips);
        }

        // did the state change?
        let full_cleanup;
        let just_update;
        let mut cache = self.status_cache.lock().unwrap();
        if let Some(cache) = &*cache {
            if cache == &state {
                debug!("No change requested for {}", record);
                return Ok(());
            }

            match (cache.same_type(&state), &state) {
                (true, CloudflareDnsValues::CName(_)) => {
                    // ONLY if we were cname before and are now again, we can skip the full cleanup and just update the record
                    just_update = true;
                    full_cleanup = false;
                }
                _ => {
                    just_update = false;
                    full_cleanup = true;
                }
            }
        } else {
            full_cleanup = true; // if no cache is present, we assume the type changed
            just_update = false; // ...and cannot update
        }

        if full_cleanup {
            let record_ids = self
                .name_to_record_ids(record)
                .await
                .map_err(CloudflareUpdateError::ApiError)?;
            for record_id in record_ids {
                self.delete_record(&record_id)
                    .await
                    .map_err(CloudflareUpdateError::ApiError)?;
            }
        }

        if just_update {
            match &state {
                CloudflareDnsValues::CName(cname) => {
                    let record_ids = self
                        .name_to_record_ids(record)
                        .await
                        .map_err(CloudflareUpdateError::ApiError)?;
                    if record_ids.len() != 1 {
                        // something must have changed, while this does not recognize a single A-record, it will trigger on multiple (non-CNAME) records
                        return Err(CloudflareUpdateError::Conflict);
                    }
                    self.update_record_cname(record, record_ids.front().unwrap(), cname, &ttl)
                        .await
                        .map_err(CloudflareUpdateError::ApiError)?;
                }
                _ => unreachable!(),
            }
        } else {
            match &state {
                CloudflareDnsValues::CName(cname) => {
                    self.create_record_cname(record, cname, &ttl)
                        .await
                        .map_err(CloudflareUpdateError::ApiError)?;
                }
                CloudflareDnsValues::CNameWithSticky(ips) => {
                    for ip in ips {
                        self.create_record_a_or_aaaa(record, ip, &ttl)
                            .await
                            .map_err(CloudflareUpdateError::ApiError)?;
                    }
                }
            }
        }

        *cache = Some(state);
        Ok(())
    }

    pub async fn update(
        &self,
        record: &str,
        selected_endpoints: std::collections::HashSet<EndpointArc>,
        ttl: u16,
    ) -> Result<(), CloudflareUpdateError> {
        let start = std::time::Instant::now();
        let res = match self.inner_update(record, selected_endpoints, ttl).await {
            Ok(v) => Ok(v),
            Err(e) => {
                // on error also reset the cache
                debug!("Resetting cache due to error: {:?}", e);
                *self.status_cache.lock().unwrap() = None;
                Err(e)
            }
        };
        let duration = start.elapsed().as_secs_f64();
        if let Some(gauge) = &self.gauge_update_duration {
            gauge.set(duration);
        }
        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get_test_config_from_env() -> (CloudflareConfiguration, String) {
        (
            CloudflareConfiguration::new(
                std::env::var("CLOUDFLARE_TOKEN").expect("CLOUDFLARE_TOKEN not set"),
                std::env::var("CLOUDFLARE_ZONE_ID").expect("CLOUDFLARE_ZONE_ID not set"),
            ),
            std::env::var("CLOUDFLARE_TLD").expect("CLOUDFLARE_TLD not set"),
        )
    }

    #[tokio::test]
    async fn test_name_to_record_ids() {
        let (config, tld) = get_test_config_from_env();
        let result = config.name_to_record_ids(&format!("_test.{}", tld)).await;
        assert!(result.unwrap().len() == 0); // the test record should not exist
    }

    #[tokio::test]
    async fn test_create_record_cname() {
        let (config, tld) = get_test_config_from_env();
        let result = config
            .create_record_cname(&format!("_create._test.{}", tld), "example.com", &60)
            .await
            .unwrap();

        // try to cleanup, but ignore the result
        let _ = config.delete_record(&result).await;
    }

    #[tokio::test]
    async fn test_delete_record() {
        let (config, _) = get_test_config_from_env();
        let result = config.delete_record("1234567890").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_create_and_destroy_cname() {
        let (config, tld) = get_test_config_from_env();
        let record = format!("_cname._cd._test.{}", tld);
        let result = config.name_to_record_ids(&record).await.unwrap();
        assert!(result.len() == 0); // the test record should not exist yet

        config
            .create_record_cname(&record, "example.com", &60)
            .await
            .unwrap();

        let result = config.name_to_record_ids(&record).await.unwrap();
        assert!(result.len() == 1); // the test record should exist now

        config.delete_record(result.front().unwrap()).await.unwrap();
    }

    #[tokio::test]
    async fn test_create_and_update_and_destroy_cname() {
        let (config, tld) = get_test_config_from_env();
        let record = format!("_cname._cud._test.{}", tld);
        let result = config.name_to_record_ids(&record).await.unwrap();
        assert!(result.len() == 0); // the test record should not exist yet

        config
            .create_record_cname(&record, "example.com", &60)
            .await
            .unwrap();

        let result = config.name_to_record_ids(&record).await.unwrap();
        assert!(result.len() == 1); // the test record should exist now
        let resord_id = result.front().unwrap();

        config
            .update_record_cname(&record, resord_id, "example.org", &60)
            .await
            .unwrap();

        config.delete_record(resord_id).await.unwrap();
    }
}
