# Starla

An alternative unofficial [RIPE Atlas](https://atlas.ripe.net) software
probe. Running it contributes measurement capacity to the RIPE Atlas
network and earns you credits to run your own measurements.

## Installation

1. Add this repository to the add-on store:
   **Settings → Add-ons → Add-on Store → ⋮ → Repositories** and add
   `https://github.com/ananthb/starla`.
2. Install the **Starla** add-on.
3. Start the add-on.

The add-on image is assembled locally from the signed release image
`ghcr.io/ananthb/starla` — nothing is compiled on your device.

## Registration

On first start the probe generates an SSH keypair in the add-on's
private data directory. The public key is printed in the add-on log
(restart the add-on after the first start to see it, or copy it from
the startup log).

Register your probe at
[atlas.ripe.net/apply/swprobe](https://atlas.ripe.net/apply/swprobe/)
using that public key. Once RIPE approves it, the probe connects and
starts taking measurements. The key survives add-on updates and
restarts.

## Configuration

Option | Default | Description
------ | ------- | -----------
`log_level` | `info` | Log verbosity: `trace`, `debug`, `info`, `warn`, `error`.
`rxtxrpt` | `false` | Report network interface traffic statistics to RIPE Atlas.
`metrics` | `false` | Enable the Prometheus metrics exporter.
`metrics_listen_addr` | `127.0.0.1:9695` | Metrics exporter bind address. The add-on uses host networking, so this binds directly on the host — use `0.0.0.0:9695` to scrape from another machine.

## Networking

The add-on runs with host networking so measurements (including IPv6)
reflect your real network path rather than Docker's internal bridge. It
needs the `NET_RAW` capability for ICMP ping and traceroute. The probe
opens no listening ports of its own — all RIPE Atlas communication flows
through an outbound SSH tunnel on port 443.

## Support

Issues and source: [github.com/ananthb/starla](https://github.com/ananthb/starla)
