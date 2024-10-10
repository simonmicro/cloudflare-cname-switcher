use crate::integrations::http::HyperHttpClient;
use log::{debug, warn};

/// NEVER allow debug output of this struct, as it contains sensitive information
pub struct TelegramConfiguration {
    send_client: HyperHttpClient,
    chat_id: i64,
    _queue: std::sync::Mutex<std::collections::LinkedList<(String, std::time::SystemTime)>>,
}

impl TelegramConfiguration {
    pub fn from_yaml(yaml: &yaml_rust2::Yaml) -> Result<Self, String> {
        let token = yaml["token"]
            .as_str()
            .ok_or("token is not a string")?
            .to_string();
        let chat_id = yaml["chat_id"]
            .as_i64()
            .ok_or("chat_id is not an integer")?;
        Ok(Self::new(token, chat_id))
    }

    pub fn new(token: String, chat_id: i64) -> Self {
        Self {
            send_client: HyperHttpClient::new(
                format!("https://api.telegram.org/bot{}/sendMessage", token)
                    .parse()
                    .unwrap(),
                None,
            ),
            chat_id,
            _queue: std::sync::Mutex::new(std::collections::LinkedList::new()),
        }
    }

    pub async fn queue_and_send(&self, message: &str) {
        // add message to buffer
        let mut queue = self._queue.lock().unwrap();
        queue.push_back((message.to_string(), std::time::SystemTime::now()));
    }

    pub async fn send(&self) {
        let mut queue = self._queue.lock().unwrap();
        if queue.len() == 0 {
            return;
        }

        // while buffer not empty, try to send the message
        while queue.len() > 0 {
            // prepare the message
            let (mut content, timestamp) = queue.front().unwrap().clone(); // take a copy, because we only pop it after sending
            let elapsed = timestamp.elapsed().unwrap().as_secs();
            let timestamp: chrono::DateTime<chrono::Utc> = timestamp.into();
            if elapsed > 10 {
                warn!("Message older than 10 seconds: {}", content);
                content = format!(
                    "{}\n\n_This is a delayed message from `{}`._",
                    content,
                    timestamp.to_rfc3339()
                );
            }

            // escape the content
            {
                let mut buffer = String::new();
                for c in content.chars() {
                    // taken from https://core.telegram.org/bots/api#markdownv2-style
                    match c {
                        '_' | '*' | '[' | ']' | '(' | ')' | '~' | '`' | '>' | '#' | '+' | '-'
                        | '=' | '|' | '{' | '}' | '.' => buffer.push('\\'),
                        _ => (),
                    }
                    buffer.push(c);
                }
                content = buffer;
            }

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
            match self.send_client.perform(request).await {
                Ok(_) => {
                    debug!("Sent message to {}: {:?}", self.chat_id, content);
                }
                Err(e) => {
                    warn!("Failed to send message: {:?}", e);
                    return;
                }
            };

            // pop the message
            queue.pop_front();
        }
    }

    pub fn has_pending(&self) -> bool {
        self._queue.lock().unwrap().len() > 0
    }
}
