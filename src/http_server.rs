use std::str::FromStr;

use log::{debug, info, warn};

type SharedRegistry =
    std::sync::Arc<tokio::sync::Mutex<Option<std::sync::Arc<prometheus::Registry>>>>;

pub struct HttpServer {
    pub registry: SharedRegistry,
}

impl HttpServer {
    pub fn new() -> Self {
        Self {
            registry: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    pub async fn run(&self) -> Result<(), String> {
        let addr = std::env::var("BIND_ADDRESS").unwrap_or_else(|_| "[::]:3000".to_string());
        let addr = std::net::SocketAddr::from_str(&addr).map_err(|e| e.to_string())?;
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| e.to_string())?;
        info!("Listening on http://{}", addr);

        loop {
            let (stream, _) = listener.accept().await.map_err(|e| e.to_string())?;
            debug!("New connection from: {:?}", stream.peer_addr());
            let io = hyper_util::rt::TokioIo::new(stream);

            // for each client spawn a new task
            let registry = self.registry.clone();
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
        registry: SharedRegistry,
    ) -> Result<hyper::Response<http_body_util::Full<bytes::Bytes>>, std::convert::Infallible> {
        let registry = registry.lock().await;
        if registry.is_none() {
            return Ok(hyper::Response::builder()
                .status(hyper::http::StatusCode::INTERNAL_SERVER_ERROR)
                .body(http_body_util::Full::new(bytes::Bytes::from(
                    "No metric registry available",
                )))
                .unwrap());
        }
        match (req.method(), req.uri().path()) {
            (&hyper::http::Method::GET, "/healthz") => Self::serve_healthz().await,
            (&hyper::http::Method::GET, "/metrics") => Self::serve_metrics(&registry).await,
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
        registry: &Option<std::sync::Arc<prometheus::Registry>>,
    ) -> Result<hyper::Response<http_body_util::Full<bytes::Bytes>>, std::convert::Infallible> {
        // create the buffer
        let encoder = prometheus::TextEncoder::new();
        let metric_families = match registry {
            Some(registry) => registry.gather(),
            None => {
                return Ok(hyper::Response::builder()
                    .status(hyper::http::StatusCode::INTERNAL_SERVER_ERROR)
                    .body(http_body_util::Full::new(bytes::Bytes::from(
                        "No registry available",
                    )))
                    .unwrap())
            }
        };
        let response_str = encoder.encode_to_string(&metric_families).unwrap();
        // create the response
        Ok(hyper::Response::new(http_body_util::Full::new(
            bytes::Bytes::from(response_str),
        )))
    }
}
