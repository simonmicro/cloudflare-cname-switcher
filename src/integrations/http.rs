use http_body_util::BodyExt;
use log::error;

#[derive(Debug)]
pub enum HyperHttpClientPhase {
    Connect,
    Handshake,
    Tls,
    Send,
    Receive,
}

#[derive(Debug)]
pub enum HyperHttpClientError {
    ConnectError(std::io::Error),
    HandshakeError(hyper::Error),
    TlsError(std::io::Error),
    SendError(hyper::Error),
    ReceiveError(hyper::Error),
    ReceiveStatus(hyper::Response<hyper::body::Incoming>),
    DecodeBodyError(std::string::FromUtf8Error),
    Timeout(HyperHttpClientPhase, tokio::time::error::Elapsed),
}

/// a http client with more fine-control and automatic https support
pub struct HyperHttpClient {
    uri: hyper::Uri,
    address_override: Option<std::net::IpAddr>,
    timeout: std::time::Duration,
}

impl HyperHttpClient {
    pub fn new(uri: hyper::Uri, address_override: Option<std::net::IpAddr>) -> Self {
        assert!(uri.scheme_str().is_some(), "URI has no scheme");
        assert!(uri.host().is_some(), "URI has no host");
        Self {
            uri,
            address_override,
            timeout: std::time::Duration::from_secs(10),
        }
    }

    /// get a pre-configured builder with the URI and HOST header set
    pub fn builder(&self) -> hyper::http::request::Builder {
        // create host header with port if necessary
        let mut host = self.uri.host().unwrap().to_string();
        if self.uri.port_u16().is_some() {
            host.push_str(":");
            host.push_str(&self.uri.port_u16().unwrap().to_string());
        }
        let location = match self.uri.path_and_query() {
            Some(pq) => pq.as_str(),
            None => "/",
        };
        hyper::Request::builder()
            .uri(location.parse::<hyper::Uri>().unwrap())
            .header(hyper::header::HOST, host)
    }

    /// after https://hyper.rs/guides/1/client/basic/, with tokio-rustls documentation
    pub async fn perform<T: hyper::body::Body>(
        &self,
        request: hyper::Request<T>,
    ) -> Result<String, HyperHttpClientError>
    where
        T: Send + 'static,
        <T as hyper::body::Body>::Data: Send,
        <T as hyper::body::Body>::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        // determine ssl mode
        let enable_ssl = match self.uri.scheme_str() {
            Some("https") => true,
            _ => false,
        };

        // determine host/port
        let host = match self.address_override.as_ref() {
            Some(addr) => addr.to_string(),
            None => self.uri.host().unwrap().to_string(),
        };
        let port = self
            .uri
            .port()
            .map(|p| p.as_u16())
            .or(Some(match enable_ssl {
                true => 443,
                false => 80,
            }))
            .unwrap();

        // connect basic tcp stream
        let stream = tokio::time::timeout(
            self.timeout,
            tokio::net::TcpStream::connect(format!("{}:{}", host, port)),
        )
        .await
        .map_err(|e| HyperHttpClientError::Timeout(HyperHttpClientPhase::Connect, e))?
        .map_err(|e| HyperHttpClientError::ConnectError(e))?;

        let result = match enable_ssl {
            false => {
                // prepare sender and start task to handle communication
                let io = hyper_util::rt::tokio::TokioIo::new(stream);
                let (mut sender, conn) =
                    tokio::time::timeout(self.timeout, hyper::client::conn::http1::handshake(io))
                        .await
                        .map_err(|e| {
                            HyperHttpClientError::Timeout(HyperHttpClientPhase::Handshake, e)
                        })?
                        .map_err(|e| HyperHttpClientError::HandshakeError(e))?;
                tokio::spawn(async move {
                    // this task will terminate if the sender is dropped
                    if let Err(err) = conn.await {
                        error!("Connection failed: {:?}", err);
                    }
                });

                // send request (regardless of ssl or not the same code)
                let response = tokio::time::timeout(self.timeout, sender.send_request(request))
                    .await
                    .map_err(|e| HyperHttpClientError::Timeout(HyperHttpClientPhase::Send, e))?
                    .map_err(|e| HyperHttpClientError::SendError(e))?;
                if response.status() != hyper::StatusCode::OK {
                    return Err(HyperHttpClientError::ReceiveStatus(response));
                }
                let body = tokio::time::timeout(self.timeout, response.collect())
                    .await
                    .map_err(|e| HyperHttpClientError::Timeout(HyperHttpClientPhase::Receive, e))?
                    .map_err(|e| HyperHttpClientError::ReceiveError(e))?;
                String::from_utf8(body.to_bytes().to_vec())
                    .map_err(|e| HyperHttpClientError::DecodeBodyError(e))?
            }
            true => {
                // initialize ssl state machine
                let mut root_cert_store = tokio_rustls::rustls::RootCertStore::empty();
                root_cert_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
                let config = tokio_rustls::rustls::ClientConfig::builder()
                    .with_root_certificates(root_cert_store)
                    .with_no_client_auth();
                let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config));
                let dnsname = rustls_pki_types::ServerName::try_from(host).unwrap();
                let tls_stream =
                    tokio::time::timeout(self.timeout, connector.connect(dnsname, stream))
                        .await
                        .map_err(|e| HyperHttpClientError::Timeout(HyperHttpClientPhase::Tls, e))?
                        .map_err(|e| HyperHttpClientError::TlsError(e))?;

                // prepare sender and start task to handle communication
                let io = hyper_util::rt::tokio::TokioIo::new(tls_stream);
                let (mut sender, conn) =
                    tokio::time::timeout(self.timeout, hyper::client::conn::http1::handshake(io))
                        .await
                        .map_err(|e| {
                            HyperHttpClientError::Timeout(HyperHttpClientPhase::Handshake, e)
                        })?
                        .map_err(|e| HyperHttpClientError::HandshakeError(e))?;
                tokio::spawn(async move {
                    // this task will terminate if the sender is dropped
                    if let Err(err) = conn.await {
                        error!("Connection failed: {:?}", err);
                    }
                });

                // send request (regardless of ssl or not the same code)
                let response = tokio::time::timeout(self.timeout, sender.send_request(request))
                    .await
                    .map_err(|e| HyperHttpClientError::Timeout(HyperHttpClientPhase::Send, e))?
                    .map_err(|e| HyperHttpClientError::SendError(e))?;
                if response.status() != hyper::StatusCode::OK {
                    return Err(HyperHttpClientError::ReceiveStatus(response));
                }
                let body = tokio::time::timeout(self.timeout, response.collect())
                    .await
                    .map_err(|e| HyperHttpClientError::Timeout(HyperHttpClientPhase::Receive, e))?
                    .map_err(|e| HyperHttpClientError::ReceiveError(e))?;
                String::from_utf8(body.to_bytes().to_vec())
                    .map_err(|e| HyperHttpClientError::DecodeBodyError(e))?
            }
        };
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_http_client() {
        let uri = "http://example.com".parse::<hyper::Uri>().unwrap();
        let client = HyperHttpClient::new(uri, None);
        let request = client
            .builder()
            .body(http_body_util::Empty::<bytes::Bytes>::new())
            .unwrap();
        let response = client.perform(request).await.unwrap();
        assert!(response.contains("Example Domain"));
    }

    #[tokio::test]
    async fn test_https_client() {
        let uri = "https://example.com".parse::<hyper::Uri>().unwrap();
        let client = HyperHttpClient::new(uri, None);
        let request = client
            .builder()
            .body(http_body_util::Empty::<bytes::Bytes>::new())
            .unwrap();
        let response = client.perform(request).await.unwrap();
        assert!(response.contains("Example Domain"));
    }
}
