<p align="center">
  <img src="assets/logo.png" width="128" height="128" alt="Starla logo">
</p>

<p align="center">
  <a href="README.md">English</a> ·
  <a href="README.es.md">Español</a> ·
  <a href="https://hosted.weblate.org/engage/starla/">+ your language</a>
</p>

# Starla

Una sonda de software no oficial alternativa para [RIPE
Atlas](https://atlas.ripe.net) escrita en Rust.

[![License](https://img.shields.io/github/license/ananthb/starla)](LICENSE)
[![Release](https://img.shields.io/github/v/release/ananthb/starla)](https://github.com/ananthb/starla/releases)
[![Docs](https://img.shields.io/badge/docs-GitHub%20Pages-blue)](https://ananthb.github.io/starla/)
[![Translation
status](https://hosted.weblate.org/widget/starla/svg-badge.svg)](https://hosted.weblate.org/engage/starla/)

## Características

- **Todos los tipos de medición**: Ping, Traceroute, DNS, HTTP, TLS, NTP
- **SSH puro en Rust**: sin dependencia de OpenSSH, utiliza `russh`
- **Sin puertos locales**: toda la comunicación fluye a través del túnel SSH
- **Imagen de contenedor mínima**: solo el binario + certificados CA,
  multiarquitectura (amd64/arm64)
- **Home Assistant add-on**: packaged in
  [ananthb/hass-addons](https://github.com/ananthb/hass-addons)
- **Módulo para NixOS**: configuración declarativa con hardening de systemd
- **Módulo para Home Manager**: servicio a nivel de usuario con launchd (macOS)
  o systemd (Linux)
- **Paquete de aplicación para macOS**: Starla Tray.app con instalador DMG y
  script de instalación CLI
- **Métricas de Prometheus**: exportación opcional para observabilidad
- **Cola de resultados en memoria limitada**: capacidad configurable, descarta
  los más antiguos cuando está llena
- **Backend Scamper (Linux)**: cada compilación para Linux enlaza estáticamente
  las bindings [`rscamper`](https://crates.io/crates/rscamper), por lo que el
  ping y el traceroute pueden enrutarse a través de un demonio scamper en
  ejecución configurando `"backend": "scamper"` en la medición. Los hosts sin
  scamper instalado no se ven afectados a menos que se seleccione realmente este
  backend. El socket por defecto es `/var/run/scamper/scamperd.sock` y puede
  anularse mediante `STARLA_SCAMPER_SOCKET`. No disponible en compilaciones para
  macOS o Windows.

## Inicio rápido

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

[![Add repository to your Home Assistant
instance](https://my.home-assistant.io/badges/supervisor_add_addon_repository.svg)](https://my.home-assistant.io/redirect/supervisor_add_addon_repository/?repository_url=https%3A%2F%2Fgithub.com%2Fananthb%2Fhass-addons)

Add `https://github.com/ananthb/hass-addons` as an add-on repository (**Settings
→ Add-ons → Add-on Store → ⋮ → Repositories**), then install the **Starla**
add-on. See the [add-on
docs](https://github.com/ananthb/hass-addons/blob/main/starla/DOCS.md).

Una vez iniciado, registra tu sonda en
[atlas.ripe.net/apply/swprobe](https://atlas.ripe.net/apply/swprobe/) utilizando
la clave pública de `probe_key.pub` en el directorio de estado.

Consulta la [guía completa de
instalación](https://ananthb.github.io/starla/install.html) para ver todas las
opciones, configuración y verificación de firmas.

## Documentación

- [Instalación y configuración](https://ananthb.github.io/starla/install.html)
- [Arquitectura](https://ananthb.github.io/starla/architecture.html)
- [Referencia de la API](https://ananthb.github.io/starla/api/starla_common/)

## Traducciones

Starla habla los idiomas que aporte la gente: la aplicación de la bandeja del
sistema, este README y el sitio de documentación se traducen en
[Weblate](https://hosted.weblate.org/engage/starla/). Añadir un idioma no
requiere Rust ni pull requests: consulta
[doc/translating.md](doc/translating.md).

## Licencia

AGPL-3.0-o-posterior: consulta [LICENSE](LICENSE).
