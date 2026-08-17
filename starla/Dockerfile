# BUILD_VERSION is the add-on version from config.yaml (passed by the
# Supervisor builder); release images are tagged v<version>, so the
# add-on version and image tag stay in sync automatically.
ARG BUILD_FROM
ARG BUILD_VERSION

FROM ghcr.io/ananthb/starla:v${BUILD_VERSION} AS starla

FROM ${BUILD_FROM}

# The starla binary is nix-built and dynamically linked against the
# /nix/store closure shipped in the upstream image.
COPY --from=starla /nix /nix
COPY --from=starla /bin/starla /usr/bin/starla

RUN apk add --no-cache ca-certificates

COPY run.sh /
RUN chmod a+x /run.sh

CMD [ "/run.sh" ]
