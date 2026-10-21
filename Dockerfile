# Peerwire images (see docs/DOCKER.md):
#
#   docker build -t peerwire-peer .                         # the always-on peer (default target)
#   docker build -t peerwire-examples --target examples .   # the browser demos, served by nginx
#
# Both run as a non-root user. Neither publishes a port by itself: docker-compose.yml binds them to
# localhost only.

# ---- build: the peer binary, and the wasm module for the demos ----
FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p peerwire-peer \
    && install -D -m 0755 target/release/peerwire-peer /out/peerwire-peer \
    && mkdir -p /out/data

FROM build AS wasm
RUN rustup target add wasm32-unknown-unknown
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    ./packages/js/build.sh

# ---- examples: packages/js (demo/, src/, pkg/) as static files, non-root, port 8080 ----
FROM nginx:stable-alpine AS examples
COPY docker/examples-nginx.conf /etc/nginx/nginx.conf
COPY docker/examples-index.html /srv/index.html
COPY packages/js/demo /srv/demo
COPY packages/js/src /srv/src
COPY --from=wasm /src/packages/js/pkg/peerwire_ffi.wasm /srv/pkg/peerwire_ffi.wasm
USER nginx
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s CMD wget -q -O /dev/null http://127.0.0.1:8080/ || exit 1

# ---- peer: one static-ish binary on distroless (glibc, no shell), uid 65532 ----
FROM gcr.io/distroless/cc-debian12:nonroot AS peer
COPY --from=build /out/peerwire-peer /usr/local/bin/peerwire-peer
# An empty /data owned by the runtime user, so a named volume mounted there is writable.
COPY --from=build --chown=65532:65532 /out/data /data
ENV PEERWIRE_DATA=/data PEERWIRE_BIND=127.0.0.1:7700
VOLUME /data
EXPOSE 7700/udp
HEALTHCHECK --interval=30s --timeout=10s --start-period=5s CMD ["/usr/local/bin/peerwire-peer", "check"]
ENTRYPOINT ["/usr/local/bin/peerwire-peer"]
