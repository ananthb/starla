# STARLA_VERSION pins the upstream starla image this add-on layers on top
# of. It is deliberately NOT the add-on version: the Supervisor builds this
# Dockerfile from whatever is on the default branch, so deriving the tag
# from BUILD_VERSION meant every version bump that landed before its release
# pointed the add-on at an image that did not exist yet, and installs failed
# with "failed to resolve source metadata". Bump this only once the release
# it names has actually pushed its image; ci.yml enforces that it exists.
ARG BUILD_FROM
ARG STARLA_VERSION

FROM ghcr.io/ananthb/starla:v${STARLA_VERSION} AS starla

FROM ${BUILD_FROM}

# The starla binary is nix-built and dynamically linked against the
# /nix/store closure shipped in the upstream image.
COPY --from=starla /nix /nix
COPY --from=starla /bin/starla /usr/bin/starla

RUN apk add --no-cache ca-certificates

COPY run.sh /
RUN chmod a+x /run.sh

CMD [ "/run.sh" ]
