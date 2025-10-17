use log::{debug, warn};

#[derive(Debug)]
pub enum DnsError {
    SerializeError(std::io::Error),
    BindError(std::io::Error),
    ConnectError(std::io::Error),
    SendError(std::io::Error),
    SendLengthTooShort,
    ReceiveTimeout(tokio::time::error::Elapsed),
    ReceiveError(std::io::Error),
    ReceiveParseError(rustdns::types::Rcode),
    ReceivedUnexpectedType(std::io::Error),
}

impl std::fmt::Display for DnsError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            DnsError::ReceiveTimeout(_) => write!(f, "Timeout during Receive"),
            other => write!(f, "{}", other),
        }
    }
}

#[derive(Debug)]
pub struct DnsConfiguration {
    /// FQDN, used for resolving the endpoints A/AAAA entries for stickyness
    pub record: String,
    /// if this endpoint is selected, the TTL will be applied to the entries part of the ingress record
    pub ttl: u16,
    /// the DNS record will be resolved by this resolver
    pub resolver: String,
    /// how often to retry the DNS resolution
    pub retry: u8,
}

impl DnsConfiguration {
    pub fn from_yaml(yaml: &yaml_rust::Yaml) -> Result<Self, String> {
        let record = yaml["record"]
            .as_str()
            .ok_or("record is not a string")?
            .to_string();
        let ttl = match yaml["ttl"].as_i64() {
            Some(t) => {
                if t < 0 || t > u16::MAX as i64 {
                    return Err("ttl is out of bounds".to_string());
                }
                t as u16
            }
            None => 0,
        };
        let resolver = yaml["resolver"]
            .as_str()
            .ok_or("resolver is not a string")?
            .to_string();
        let retry = match yaml["retry"].as_i64() {
            Some(r) => {
                if r < 0 || r > u8::MAX as i64 {
                    return Err("retry is out of bounds".to_string());
                }
                r as u8
            }
            None => 1,
        };
        Ok(Self {
            record,
            ttl,
            resolver,
            retry,
        })
    }

    /// send two queries against the resolver (since not multiple at once are always supported -> https://stackoverflow.com/a/4083071)
    async fn _resolve(&self) -> Result<std::collections::HashSet<std::net::IpAddr>, DnsError> {
        let mut returnme = std::collections::HashSet::<std::net::IpAddr>::new();

        // connect using UDP
        let sock = tokio::net::UdpSocket::bind("0.0.0.0:0")
            .await
            .map_err(DnsError::BindError)?;
        sock.connect(format!("{}:{}", self.resolver, 53))
            .await
            .map_err(DnsError::ConnectError)?;
        debug!("Resolving \"{}\" using {}", self.record, self.resolver);

        // create message for ipv4-records
        {
            let mut request = rustdns::Message::default();
            request.add_question(
                &self.record,
                rustdns::types::Type::A,
                rustdns::types::Class::Internet,
            );
            let request_bytes = request.to_vec().map_err(DnsError::SerializeError)?;

            // send the request and...
            let len = sock
                .send(&request_bytes)
                .await
                .map_err(DnsError::SendError)?;
            if len != request_bytes.len() {
                return Err(DnsError::SendLengthTooShort);
            }

            // ...wait for the response
            let mut resp = [0; 4096];
            let len = tokio::time::timeout(std::time::Duration::new(10, 0), sock.recv(&mut resp))
                .await
                .map_err(DnsError::ReceiveTimeout)?
                .map_err(DnsError::ReceiveError)?;
            let answer = rustdns::types::Message::from_slice(&resp[0..len])
                .map_err(DnsError::ReceivedUnexpectedType)?;
            if answer.rcode != rustdns::types::Rcode::NoError {
                return Err(DnsError::ReceiveParseError(answer.rcode));
            }

            // parse the response
            for dns_record in answer.answers {
                if let rustdns::types::Resource::A(a) = dns_record.resource {
                    returnme.insert(std::net::IpAddr::V4(a));
                }
            }
        }

        // create message for ipv6-records
        {
            let mut request = rustdns::Message::default();
            request.add_question(
                &self.record,
                rustdns::types::Type::AAAA,
                rustdns::types::Class::Internet,
            );
            let request_bytes = request.to_vec().map_err(DnsError::SerializeError)?;

            // send the request and...
            let len = sock
                .send(&request_bytes)
                .await
                .map_err(DnsError::SendError)?;
            if len != request_bytes.len() {
                return Err(DnsError::SendLengthTooShort);
            }

            // ...wait for the response
            let mut resp = [0; 4096];
            let len = tokio::time::timeout(std::time::Duration::new(10, 0), sock.recv(&mut resp))
                .await
                .map_err(DnsError::ReceiveTimeout)?
                .map_err(DnsError::ReceiveError)?;
            let answer = rustdns::types::Message::from_slice(&resp[0..len])
                .map_err(DnsError::ReceivedUnexpectedType)?;
            if answer.rcode != rustdns::types::Rcode::NoError {
                return Err(DnsError::ReceiveParseError(answer.rcode));
            }

            // parse the response
            let mut returnme = std::collections::HashSet::<std::net::IpAddr>::new();
            for dns_record in answer.answers {
                if let rustdns::types::Resource::AAAA(aaaa) = dns_record.resource {
                    returnme.insert(std::net::IpAddr::V6(aaaa));
                }
            }
        }

        debug!("Resolved \"{}\" to {:?}", self.record, returnme);
        Ok(returnme)
    }

    pub async fn resolve(&self) -> Result<std::collections::HashSet<std::net::IpAddr>, DnsError> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let last_attempt = attempt > self.retry;
            let result = self._resolve().await;
            break match result {
                Ok(r) => Ok(r),
                Err(e) => {
                    if !last_attempt {
                        warn!("Attempt {} failed: {:?}", attempt, e);
                        continue;
                    }
                    Err(e)
                }
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_resolve() {
        let config = DnsConfiguration {
            record: "example.com".to_string(),
            ttl: 0,
            resolver: "1.1.1.1".to_string(),
            retry: 1,
        };
        let result = config.resolve().await.unwrap();
        assert!(result.len() > 0);
    }
}
