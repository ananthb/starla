#!/usr/bin/with-contenv bashio
# ==============================================================================
# Starla add-on: render add-on options into config.toml and start the probe.
# ==============================================================================
set -e

CONFIG=/etc/starla/config.toml
mkdir -p /etc/starla

cat > "${CONFIG}" <<EOF
[probe]
log_level = "$(bashio::config 'log_level')"

[network]
rxtxrpt = $(bashio::config 'rxtxrpt')

[metrics]
enabled = $(bashio::config 'metrics')
listen_addr = "$(bashio::config 'metrics_listen_addr')"

[logging]
format = "text"
output = "stdout"
EOF

if bashio::fs.file_exists /data/probe_key.pub; then
    bashio::log.info "Probe public key (register at https://atlas.ripe.net/apply/swprobe/):"
    bashio::log.info "$(cat /data/probe_key.pub)"
else
    bashio::log.notice "No probe key yet; one will be generated now."
    bashio::log.notice "Restart the add-on to see the public key needed for registration."
fi

export SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt

exec /usr/bin/starla --config "${CONFIG}" --state-dir /data
