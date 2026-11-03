# syntax=docker/dockerfile:1
# The editor is compiled only at image build time; runtime needs no Node or CDN.
FROM node:22-bookworm AS editor-builder
WORKDIR /editor
# Keep npm's dependency layer independent from changing application sources.
COPY package.json package-lock.json ./
# Reuse downloaded npm packages across uncached builds. The optional PEM
# certificates are trusted only during this build step, not in the runtime.
RUN --mount=type=cache,id=datawizz-npm,target=/root/.npm,sharing=locked \
    --mount=type=bind,source=certs,target=/tmp/corporate-certs,readonly \
    set -eu; \
    ca_bundle=/tmp/datawizz-node-extra-ca.pem; \
    for cert in /tmp/corporate-certs/*.crt /tmp/corporate-certs/*.pem; do \
      [ -f "$cert" ] || continue; \
      cat "$cert" >> "$ca_bundle"; \
      echo >> "$ca_bundle"; \
    done; \
    if [ -s "$ca_bundle" ]; then export NODE_EXTRA_CA_CERTS="$ca_bundle"; fi; \
    npm ci --prefer-offline --no-audit --no-fund
COPY scripts/build-editor.cjs ./scripts/build-editor.cjs
COPY web/editor-entry.js ./web/editor-entry.js
RUN npm run build:editor

FROM rust:1-bookworm AS builder
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Source certificates are mounted only for this build step.
# The resulting builder trust store is not copied into the runtime image.
# PEM-encoded *.crt and *.pem files in certs/ are supported; no certs is fine.
RUN --mount=type=bind,source=certs,target=/tmp/corporate-certs,readonly \
    set -eu; \
    for cert in /tmp/corporate-certs/*.crt /tmp/corporate-certs/*.pem; do \
      [ -f "$cert" ] || continue; \
      name="$(basename "$cert")"; \
      cp "$cert" "/usr/local/share/ca-certificates/datawizz-${name%.*}.crt"; \
    done; \
    update-ca-certificates
ENV CARGO_HTTP_CAINFO=/etc/ssl/certs/ca-certificates.crt \
    CARGO_HTTP_PROXY_CAINFO=/etc/ssl/certs/ca-certificates.crt \
    SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
WORKDIR /build
COPY Cargo.toml ./
COPY src ./src
COPY web ./web
COPY --from=editor-builder /editor/web/vendor/sql-editor.js ./web/vendor/sql-editor.js
RUN cargo build --release

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/datawizz /usr/local/bin/datawizz
USER 65532:65532
ENV HOST=0.0.0.0 PORT=3000
EXPOSE 3000
ENTRYPOINT ["/usr/local/bin/datawizz"]
