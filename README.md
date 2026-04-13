<p align="center">
  <img src="assets/logo.png" width="128" height="128" alt="Starla logo">
</p>

# Starla

An alternative unofficial [RIPE Atlas](https://atlas.ripe.net) software probe written in Rust.

[![License](https://img.shields.io/github/license/ananthb/starla)](LICENSE)
[![Release](https://img.shields.io/github/v/release/ananthb/starla)](https://github.com/ananthb/starla/releases)
[![Docs](https://img.shields.io/badge/docs-GitHub%20Pages-blue)](https://ananthb.github.io/starla/)

## Features

- **All measurement types** — Ping, Traceroute, DNS, HTTP, TLS, NTP
- **Pure Rust SSH** — no OpenSSH dependency, uses `russh`
- **No local ports** — all communication flows through the SSH tunnel
- **Minimal container image** — just the binary + CA certs, multi-arch (amd64/arm64)
- **NixOS module** — declarative configuration with systemd hardening
- **Home Manager module** — user-level service with launchd (macOS) or systemd (Linux)
- **macOS app bundle** — Starla Tray.app with DMG installer and Install CLI script
- **Prometheus metrics** — optional observability export
- **Persistent result queue** — survives restarts via RocksDB

## Quick Start

```bash
# Docker / Podman
docker run -d --name starla \
  -v starla-state:/state \
  --cap-add NET_RAW \
  ghcr.io/ananthb/starla:latest

# NixOS
services.starla.enable = true;

# Ubuntu / Debian
curl -LO https://github.com/ananthb/starla/releases/latest/download/starla_0.1.0_amd64.deb
sudo dpkg -i starla_*.deb
sudo systemctl enable --now starla

# Fedora / RHEL
curl -LO https://github.com/ananthb/starla/releases/latest/download/starla-0.1.0-1.x86_64.rpm
sudo dnf install ./starla-*.rpm
sudo systemctl enable --now starla

# Release tarball
curl -LO https://github.com/ananthb/starla/releases/latest/download/starla-amd64.tar.gz
tar xzf starla-amd64.tar.gz && sudo ./starla/starla
```

After starting, register your probe at [atlas.ripe.net/apply/swprobe](https://atlas.ripe.net/apply/swprobe/)
using the public key from `probe_key.pub` in the state directory.

See the [full installation guide](https://ananthb.github.io/starla/install.html) for all options,
configuration, and signature verification.

## Documentation

- [Installation & Configuration](https://ananthb.github.io/starla/install.html)
- [Architecture](https://ananthb.github.io/starla/architecture.html)
- [API Reference](https://ananthb.github.io/starla/api/starla_common/)

## License

AGPL-3.0-or-later — See [LICENSE](LICENSE).
