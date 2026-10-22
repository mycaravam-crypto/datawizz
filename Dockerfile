# syntax=docker/dockerfile:1
FROM rust:1-bookworm AS builder
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
# Optional corporate CA via BuildKit secret; never copy certs into the repository/image.
# docker build --secret id=corporate_ca,src=certs/company-ca.crt -t datawizz:local .
RUN --mount=type=secret,id=corporate_ca \
    if [ -f /run/secrets/corporate_ca ]; then \
      cp /run/secrets/corporate_ca /usr/local/share/ca-certificates/company-ca.crt && \
      update-ca-certificates; \
    fi
ENV CARGO_HTTP_CAINFO=/etc/ssl/certs/ca-certificates.crt \
    CARGO_HTTP_PROXY_CAINFO=/etc/ssl/certs/ca-certificates.crt \
    SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
WORKDIR /build
COPY Cargo.toml ./
COPY src ./src
COPY web ./web
RUN cargo build --release

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/datawizz /usr/local/bin/datawizz
USER 65532:65532
ENV HOST=0.0.0.0 PORT=3000
EXPOSE 3000
ENTRYPOINT ["/usr/local/bin/datawizz"]
