<p align="center">
  <img src="assets/logo.png" width="128" height="128" alt="Starla logo">
</p>

<p align="center">
  <a href="README.md">English</a> ·
  <a href="README.es.md">Español</a> ·
  <a href="https://hosted.weblate.org/engage/starla/">+ your language</a>
</p>

# Starla

An alternative unofficial [RIPE Atlas](https://atlas.ripe.net) software probe written in Rust.

[![License](https://img.shields.io/github/license/ananthb/starla)](LICENSE)
[![Release](https://img.shields.io/github/v/release/ananthb/starla)](https://github.com/ananthb/starla/releases)
[![Docs](https://img.shields.io/badge/docs-GitHub%20Pages-blue)](https://ananthb.github.io/starla/)
[![Translation status](https://hosted.weblate.org/widget/starla/svg-badge.svg)](https://hosted.weblate.org/engage/starla/)

## Features

- **All measurement types**: Ping, Traceroute, DNS, HTTP, TLS, NTP
- **Pure Rust SSH**: no OpenSSH dependency, uses `russh`
- **No local ports**: all communication flows through the SSH tunnel
- **Minimal container image**: just the binary + CA certs, multi-arch (amd64/arm64)
- **Home Assistant add-on**: this repo doubles as an add-on repository
- **NixOS module**: declarative configuration with systemd hardening
- **Home Manager module**: user-level service with launchd (macOS) or systemd (Linux)
- **macOS app bundle**: Starla Tray.app with DMG installer and Install CLI script
- **Prometheus metrics**: optional observability export
- **Bounded in-memory result queue**: configurable capacity, drops oldest when full
- **Scamper backend (Linux)**: every Linux build statically links the
  [`rscamper`](https://crates.io/crates/rscamper) bindings, so ping and
  traceroute can be routed through a running scamper daemon by setting
  `"backend": "scamper"` on the measurement. Hosts without scamper
  installed are unaffected unless the backend is actually selected. The
  socket defaults to `/var/run/scamper/scamperd.sock` and can be overridden
  with `STARLA_SCAMPER_SOCKET`. Not available on macOS or Windows builds.

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
curl -LO https://github.com/ananthb/starla/releases/latest/download/starla_0.3.0_amd64.deb
sudo dpkg -i starla_*.deb
sudo systemctl enable --now starla

# Fedora / RHEL
curl -LO https://github.com/ananthb/starla/releases/latest/download/starla-0.3.0-1.x86_64.rpm
sudo dnf install ./starla-*.rpm
sudo systemctl enable --now starla

# Release tarball
curl -LO https://github.com/ananthb/starla/releases/latest/download/starla-amd64.tar.gz
tar xzf starla-amd64.tar.gz && sudo ./starla/starla
```

### Home Assistant

[![Add repository to your Home Assistant instance](https://my.home-assistant.io/badges/supervisor_add_addon_repository.svg)](https://my.home-assistant.io/redirect/supervisor_add_addon_repository/?repository_url=https%3A%2F%2Fgithub.com%2Fananthb%2Fstarla)

Add `https://github.com/ananthb/starla` as an add-on repository
(**Settings → Add-ons → Add-on Store → ⋮ → Repositories**), then install
the **Starla** add-on. See the [add-on docs](starla/DOCS.md).

After starting, register your probe at [atlas.ripe.net/apply/swprobe](https://atlas.ripe.net/apply/swprobe/)
using the public key from `probe_key.pub` in the state directory.

See the [full installation guide](https://ananthb.github.io/starla/install.html) for all options,
configuration, and signature verification.

## Documentation

- [Installation & Configuration](https://ananthb.github.io/starla/install.html)
- [Architecture](https://ananthb.github.io/starla/architecture.html)
- [API Reference](https://ananthb.github.io/starla/api/starla_common/)

## Translating

Starla speaks whatever languages people contribute: the tray app, this
README, and the documentation site are all translated on
[Weblate](https://hosted.weblate.org/engage/starla/). Adding a language
takes no Rust and no pull request — see
[doc/translating.md](doc/translating.md).

## License

AGPL-3.0-or-later: See [LICENSE](LICENSE).
