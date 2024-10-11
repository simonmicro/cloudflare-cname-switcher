use log::debug;

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

#[derive(Debug)]
pub struct DnsConfiguration {
    /// FQDN, used for resolving the endpoints A/AAAA entries for stickyness
    pub record: String,
    /// if this endpoint is selected, the TTL will be applied to the entries part of the ingress record
    pub ttl: u16,
    /// the DNS record will be resolved by this resolver
    pub resolver: String,
}

impl DnsConfiguration {
    pub fn from_yaml(yaml: &yaml_rust2::Yaml) -> Result<Self, String> {
        let record = yaml["record"]
            .as_str()
            .ok_or("record is not a string")?
            .to_string();
        let ttl = yaml["ttl"].as_i64().ok_or("ttl is not an integer")? as u16;
        let resolver = yaml["resolver"]
            .as_str()
            .ok_or("resolver is not a string")?
            .to_string();
        Ok(Self {
            record,
            ttl,
            resolver,
        })
    }

    /// send two queries against the resolver (since not multiple at once are always supported -> https://stackoverflow.com/a/4083071)
    pub async fn resolve(&self) -> Result<std::collections::HashSet<std::net::IpAddr>, DnsError> {
        let mut returnme = std::collections::HashSet::<std::net::IpAddr>::new();

        // connect using UDP
        let sock = tokio::net::UdpSocket::bind("0.0.0.0:0")
            .await
            .map_err(|e| DnsError::BindError(e))?;
        sock.connect(format!("{}:{}", self.resolver, 53))
            .await
            .map_err(|e| DnsError::ConnectError(e))?;
        debug!("resolving \"{}\" using {}", self.record, self.resolver);

        // create message for ipv4-records
        {
            let mut request = rustdns::Message::default();
            request.add_question(
                &self.record,
                rustdns::types::Type::A,
                rustdns::types::Class::Internet,
            );
            let request_bytes = request.to_vec().map_err(|e| DnsError::SerializeError(e))?;

            // send the request and...
            let len = sock
                .send(&request_bytes)
                .await
                .map_err(|e| DnsError::SendError(e))?;
            if len != request_bytes.len() {
                return Err(DnsError::SendLengthTooShort);
            }

            // ...wait for the response
            let mut resp = [0; 4096];
            let len = tokio::time::timeout(std::time::Duration::new(10, 0), sock.recv(&mut resp))
                .await
                .map_err(|e| DnsError::ReceiveTimeout(e))?
                .map_err(|e| DnsError::ReceiveError(e))?;
            let answer = rustdns::types::Message::from_slice(&resp[0..len])
                .map_err(|e| DnsError::ReceivedUnexpectedType(e))?;
            if answer.rcode != rustdns::types::Rcode::NoError {
                return Err(DnsError::ReceiveParseError(answer.rcode));
            }

            // parse the response
            for dns_record in answer.answers {
                match dns_record.resource {
                    rustdns::types::Resource::A(a) => {
                        returnme.insert(std::net::IpAddr::V4(a));
                    }
                    _ => {}
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
            let request_bytes = request.to_vec().map_err(|e| DnsError::SerializeError(e))?;

            // send the request and...
            let len = sock
                .send(&request_bytes)
                .await
                .map_err(|e| DnsError::SendError(e))?;
            if len != request_bytes.len() {
                return Err(DnsError::SendLengthTooShort);
            }

            // ...wait for the response
            let mut resp = [0; 4096];
            let len = tokio::time::timeout(std::time::Duration::new(10, 0), sock.recv(&mut resp))
                .await
                .map_err(|e| DnsError::ReceiveTimeout(e))?
                .map_err(|e| DnsError::ReceiveError(e))?;
            let answer = rustdns::types::Message::from_slice(&resp[0..len])
                .map_err(|e| DnsError::ReceivedUnexpectedType(e))?;
            if answer.rcode != rustdns::types::Rcode::NoError {
                return Err(DnsError::ReceiveParseError(answer.rcode));
            }

            // parse the response
            let mut returnme = std::collections::HashSet::<std::net::IpAddr>::new();
            for dns_record in answer.answers {
                match dns_record.resource {
                    rustdns::types::Resource::AAAA(aaaa) => {
                        returnme.insert(std::net::IpAddr::V6(aaaa));
                    }
                    _ => {}
                }
            }
        }

        debug!("resolved \"{}\" to {:?}", self.record, returnme);
        Ok(returnme)
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
        };
        let result = config.resolve().await.unwrap();
        assert!(result.len() > 0);
    }
}
