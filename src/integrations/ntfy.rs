use crate::integrations::http::HyperHttpClient;
use log::{debug, warn};

/// NEVER allow debug output of this struct, as it contains sensitive information
pub struct NtfyConfiguration {
    send_client: HyperHttpClient,
    token: Option<String>,
    priority_bad: u8,
    priority_good: u8,
    queue: std::sync::Mutex<std::collections::LinkedList<(String, bool, std::time::SystemTime)>>,
    gauge_send_duration: Option<Box<prometheus::Gauge>>,
    gauge_queue_amount: Option<Box<prometheus::IntGauge>>,
    silence_until: Option<std::time::SystemTime>,
}

impl NtfyConfiguration {
    pub fn from_yaml(
        yaml: &yaml_rust::Yaml,
        registry: &prometheus::Registry,
    ) -> Result<Self, String> {
        let silence_until = match yaml["initial_silence"].as_i64() {
            Some(x) => {
                if x < 0 {
                    return Err("initial_silence must be a positive integer".to_string());
                }
                Some(std::time::SystemTime::now() + std::time::Duration::from_secs(x as u64))
            }
            None => None,
        };
        let uri = yaml["uri"]
            .as_str()
            .ok_or("uri is not a string")?
            .to_string();
        let token = yaml["token"].as_str().map(|s| s.to_string());
        let priority_bad = match yaml["priority_bad"].as_i64() {
            Some(x) => {
                if !(1..5).contains(&x) {
                    return Err("priority_bad must be an integer in range [1, 5]".to_string());
                }
                x as u8
            }
            None => 4, // high
        };
        let priority_good = match yaml["priority_good"].as_i64() {
            Some(x) => {
                if !(1..5).contains(&x) {
                    return Err("priority_good must be an integer in range [1, 5]".to_string());
                }
                x as u8
            }
            None => 3, // default
        };
        let gauge_send_duration = Box::new(
            prometheus::Gauge::new("ntfy_send_seconds", "Duration of last message send").unwrap(),
        );
        registry.register(gauge_send_duration.clone()).unwrap();
        let gauge_queue_amount = Box::new(
            prometheus::IntGauge::new("ntfy_queue_amount", "Amount of messages in the queue")
                .unwrap(),
        );
        registry.register(gauge_queue_amount.clone()).unwrap();
        Ok(Self::new(
            uri,
            token,
            priority_bad,
            priority_good,
            silence_until,
            Some(gauge_send_duration),
            Some(gauge_queue_amount),
        ))
    }

    pub fn new(
        uri: String,
        token: Option<String>,
        priority_bad: u8,
        priority_good: u8,
        silence_until: Option<std::time::SystemTime>,
        gauge_send_duration: Option<Box<prometheus::Gauge>>,
        gauge_queue_amount: Option<Box<prometheus::IntGauge>>,
    ) -> Self {
        Self {
            send_client: HyperHttpClient::new(
                uri.parse().unwrap(),
                std::time::Duration::from_secs(10),
                0,
                None,
            ),
            token,
            priority_bad,
            priority_good,
            queue: std::sync::Mutex::new(std::collections::LinkedList::new()),
            gauge_send_duration,
            gauge_queue_amount,
            silence_until,
        }
    }

    pub fn escape(message: &str) -> String {
        let mut buffer = String::new();
        for c in message.chars() {
            // taken from https://docs.ntfy.sh/publish/#markdown-formatting
            match c {
                '_' | '*' | '[' | ']' | '(' | ')' | '!' | '`' | '#' | '-' | '+' | '.' | '>' => {
                    buffer.push('\\')
                }
                _ => (),
            }
            buffer.push(c);
        }
        buffer
    }

    pub async fn queue_and_send(&self, message: &str, good: bool) {
        // check if we are in silence mode
        if let Some(silence_until) = &self.silence_until {
            if *silence_until > std::time::SystemTime::now() {
                return;
            }
        }
        // add message to buffer
        {
            let mut queue = self.queue.lock().unwrap();
            queue.push_back((message.to_string(), good, std::time::SystemTime::now()));
            if let Some(gauge) = &self.gauge_queue_amount {
                gauge.set(queue.len() as i64);
            }
        }
        self.send().await;
    }

    pub async fn send(&self) {
        let mut queue = self.queue.lock().unwrap();
        if queue.is_empty() {
            return;
        }
        if queue.len() > 128 {
            panic!("Ntfy queue is too long... Something is really wrong!");
        }

        // while buffer not empty, try to send the message
        while !queue.is_empty() {
            // prepare the message
            let (mut content, good, timestamp) = queue.front().unwrap().clone(); // take a copy, because we only pop it after sending
            let elapsed = timestamp.elapsed().unwrap().as_secs();
            let timestamp: chrono::DateTime<chrono::Utc> = timestamp.into();
            if elapsed > 10 {
                let timestamp_str = timestamp.to_rfc3339();
                warn!(
                    "Message older than 10 seconds (from {timestamp_str}): {}",
                    content
                );
                content = format!(
                    "{}\n\n_This is a delayed message from `{}`._",
                    content, timestamp_str
                );
            }
            debug!("Sending a message: {}", content);

            // create the http builder with header
            let builder = match &self.token.as_ref() {
                &Some(token) => self
                    .send_client
                    .builder()
                    .header(hyper::header::AUTHORIZATION, format!("Bearer {}", token)),
                None => self.send_client.builder(),
            };
            // create the body
            let request = builder
                .header(hyper::header::CONTENT_TYPE, "text/markdown")
                .header(
                    "X-Priority",
                    match good {
                        true => self.priority_good,
                        false => self.priority_bad,
                    }
                    .to_string(),
                )
                .method(hyper::http::Method::POST)
                .body(http_body_util::Full::<bytes::Bytes>::from(
                    //"test".to_string().into_bytes(),
                    content.into_bytes(),
                ))
                .unwrap();

            // send the message
            let result = {
                let start = std::time::Instant::now();
                let res = self.send_client.perform(request).await;
                let duration = start.elapsed().as_secs_f64();
                if let Some(gauge) = &self.gauge_send_duration {
                    gauge.set(duration);
                }
                res
            };
            if let Err(e) = result {
                warn!("Failed to send message: {:?}", e);
                return;
            };

            // pop the message
            queue.pop_front();
            if let Some(gauge) = &self.gauge_queue_amount {
                gauge.set(queue.len() as i64);
            }
        }
    }

    pub fn has_pending(&self) -> bool {
        !self.queue.lock().unwrap().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const PUBLIC_INSTANCE_URL: &str = "https://ntfy.sh/ccs_unit_test";
    const TEST_MESSAGE: &str = "# UNIT_TEST_IGNORE_THIS\n❌ `primary` (every 10s, confidence of 3, HTTP error: Timeout during Connect)\n✅ `failover`\n\n**bold** or _**combined italic**_ or [linked](https://example.com)";

    fn get_test_config_from_env() -> NtfyConfiguration {
        NtfyConfiguration::new(
            std::env::var("NTFY_URI").unwrap_or(PUBLIC_INSTANCE_URL.to_string()),
            std::env::var("NTFY_TOKEN").ok(),
            4, // priority: high
            3, // priority: default
            None,
            None,
            None,
        )
    }

    #[tokio::test]
    async fn test_good_push() {
        let config = get_test_config_from_env();
        config.queue_and_send(TEST_MESSAGE, true).await;
        assert!(config.queue.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_bad_push() {
        let config = get_test_config_from_env();
        config.queue_and_send(TEST_MESSAGE, false).await;
        assert!(config.queue.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_escaped_push() {
        let config = get_test_config_from_env();
        config
            .queue_and_send(&NtfyConfiguration::escape(TEST_MESSAGE), true)
            .await;
        assert!(config.queue.lock().unwrap().is_empty());
    }
}
