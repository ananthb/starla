# starla-measurements

Network measurement implementations compatible with the RIPE Atlas result format.

## Supported Measurements

| Type | Description |
|------|-------------|
| **Ping** | ICMP echo request/reply with RTT measurement |
| **Traceroute** | Path discovery via ICMP, UDP, or TCP |
| **DNS** | DNS queries with timing and response capture |
| **HTTP** | HTTP/HTTPS requests with timing breakdown |
| **TLS** | TLS certificate retrieval and validation |
| **NTP** | NTP time synchronization measurement |

## Usage

All measurements implement the `Measurement` trait:

```rust
use starla_measurements::{Ping, PingConfig, Measurement};

let config = PingConfig {
    target: "8.8.8.8".parse()?,
    packets: 3,
    size: 64,
    ..Default::default()
};

let ping = Ping::new(config);
let result = ping.execute().await?;
```

## Result Format

Results are formatted to match the RIPE Atlas JSON schema, ensuring compatibility with existing analysis tools and the RIPE Atlas API.
