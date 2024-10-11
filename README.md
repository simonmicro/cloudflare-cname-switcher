# Cloudflare CNAME Switcher
An external tool to automatically update a dynamic CNAME on Cloudflare to point to another CNAME or (multiple-) A/AAAA records, based on the current endpoint reachability.

## How does this work?
You place this program on an external machine, which will test the endpoint reachability of the primary and secondary endpoints. Based on the results, it will update the CNAME to point to the currently best reachable endpoint. Every endpoint may have difffernt TTLs, so clients may stick longer to the secondary endpoint, even if the primary is reachable again.

### Typical application scenario
* Multiple _service_-records (`service1.example.com`, `service2.example.com`), hosted on the same reverse proxy, pointing to the same `ingress.example.com` record
* Central `ingress.example.com` _ingress_-record to point to the currently best reachable service
* Primary _endpoint_-record over `primary.example.com` (either CNAME or A/AAAA record) using a cable connection
* Secondary _endpoint_-record over `secondary.example.com` (either CNAME or A/AAAA record) using a mobile connection

### Typical workflow
1. CCS starts monitoring of all _endpoints_ (primary, secondary) and marks them as online (...)
   1. Also periodically updating the A/AAAA record-values-cache for stickiness operations
2. Every time an _endpoint_ state (online/offline) changes...
   1. Update the _ingress_ record to point to (either)...
      1. A reachable endpoint with the lowest configured priority
      2. A combination of reachable endpoints, if stickiness is enabled
   2. CCS queue a notification to be sent

### When is an endpoint considered online?
* Periodically fetching an HTTP/S response from an endpoint
* Checking the content for a specific string to prevent passing of internal server errors being masked by a reverse proxy as 200-OK
* Applying a cooldown: An endpoint must be reachable for a configurable amount of subsequent checks to be considered online

## Features
* Sticky endpoints: After the _ingress_ record is updated from a secondary to a primary endpoint again, the A/AAAA records of the secondary endpoint are kept in the _ingress_ record for a configurable amount of time
  * Instead of a CNAME to a single endpoint, you may also temporarily use multiple A/AAAA records pointing to different endpoints
  * TTL is then forced to the lowest TTL out of all mixed endpoints, so clients will continue to mix between all the endpoints (as none expire earlier)
* Cloudflare support :P
  * A/AAAA, CNAME records as _ingress_ or _endpoint_
* Telegram notifications
  * Automatic retry on failure
  * Details about _endpoint_ reachability and _ingress_ record
* Prometheus metrics (via `/metrics`)
  * Duration statistics (last iteration, endpoint checks, ...)
  * Current IPv4/IPv6/CNAME addresses of the _endpoint_-/_ingress_-records
* Liveness endpoint (via `/healthz`)
* Automatic configuration reload on file change

## Getting Started
Go ahead and install the required packages:
```bash
pip3 install -r requirements.txt
```

Then copy the `config.sample.yaml` to `config.yaml` and adjust the settings to your needs.

## Run it!
You may either start this directly on your machine, use the provided `Dockerfile` to build a container or use a pre-built container from one of the supported registires (see `.gitlab-ci.yml`).
