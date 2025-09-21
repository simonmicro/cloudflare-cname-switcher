use crate::integrations::http::HyperHttpClient;
use log::{debug, warn};

/// NEVER allow debug output of this struct, as it contains sensitive information
pub struct TelegramConfiguration {
    send_client: HyperHttpClient,
    chat_id: i64,
    queue: std::sync::Mutex<std::collections::LinkedList<(String, std::time::SystemTime)>>,
    gauge_send_duration: Option<Box<prometheus::Gauge>>,
    gauge_queue_amount: Option<Box<prometheus::IntGauge>>,
    silence_until: Option<std::time::SystemTime>,
}

impl TelegramConfiguration {
    pub fn from_yaml(
        yaml: &yaml_rust2::Yaml,
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
        let token = yaml["token"]
            .as_str()
            .ok_or("token is not a string")?
            .to_string();
        let chat_id = yaml["chat_id"]
            .as_i64()
            .ok_or("chat_id is not an integer")?;
        let gauge_send_duration = Box::new(
            prometheus::Gauge::new("telegram_send_seconds", "Duration of last message send")
                .unwrap(),
        );
        registry.register(gauge_send_duration.clone()).unwrap();
        let gauge_queue_amount = Box::new(
            prometheus::IntGauge::new("telegram_queue_amount", "Amount of messages in the queue")
                .unwrap(),
        );
        registry.register(gauge_queue_amount.clone()).unwrap();
        Ok(Self::new(
            token,
            chat_id,
            silence_until,
            Some(gauge_send_duration),
            Some(gauge_queue_amount),
        ))
    }

    pub fn new(
        token: String,
        chat_id: i64,
        silence_until: Option<std::time::SystemTime>,
        gauge_send_duration: Option<Box<prometheus::Gauge>>,
        gauge_queue_amount: Option<Box<prometheus::IntGauge>>,
    ) -> Self {
        Self {
            send_client: HyperHttpClient::new(
                format!("https://api.telegram.org/bot{}/sendMessage", token)
                    .parse()
                    .unwrap(),
                std::time::Duration::from_secs(10),
                0,
                None,
            ),
            chat_id,
            queue: std::sync::Mutex::new(std::collections::LinkedList::new()),
            gauge_send_duration,
            gauge_queue_amount,
            silence_until,
        }
    }

    pub fn escape(message: &str) -> String {
        let mut buffer = String::new();
        for c in message.chars() {
            // taken from https://core.telegram.org/bots/api#markdownv2-style
            match c {
                '_' | '*' | '[' | ']' | '(' | ')' | '~' | '`' | '>' | '#' | '+' | '-' | '='
                | '|' | '{' | '}' | '.' => buffer.push('\\'),
                _ => (),
            }
            buffer.push(c);
        }
        buffer
    }

    pub async fn queue_and_send(&self, message: &str) {
        // check if we are in silence mode
        if let Some(silence_until) = &self.silence_until {
            if *silence_until > std::time::SystemTime::now() {
                return;
            }
        }
        // add message to buffer
        {
            let mut queue = self.queue.lock().unwrap();
            queue.push_back((message.to_string(), std::time::SystemTime::now()));
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
            panic!("Telegram queue is too long... Something is really wrong!");
        }

        // while buffer not empty, try to send the message
        while !queue.is_empty() {
            // prepare the message
            let (mut content, timestamp) = queue.front().unwrap().clone(); // take a copy, because we only pop it after sending
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
            debug!("Sending a message to {}: {}", self.chat_id, content);

            // build the request JSON
            let data = serde_json::Value::Object(serde_json::Map::from_iter([
                (
                    "chat_id".to_string(),
                    serde_json::Value::Number(self.chat_id.into()),
                ),
                (
                    "parse_mode".to_string(),
                    serde_json::Value::String("MarkdownV2".to_string()),
                ),
                (
                    "text".to_string(),
                    serde_json::Value::String(content.clone()),
                ),
            ]));

            // create the body
            let builder = self.send_client.builder();
            let request = builder
                .header(hyper::header::CONTENT_TYPE, "application/json")
                .method(hyper::http::Method::POST)
                .body(http_body_util::Full::<bytes::Bytes>::from(
                    serde_json::to_vec(&data).unwrap(),
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
