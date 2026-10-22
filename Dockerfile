FROM rust:1-bookworm AS builder
WORKDIR /build
COPY Cargo.toml ./
COPY src ./src
COPY web ./web
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/datawizz /usr/local/bin/datawizz
USER 65532:65532
ENV HOST=0.0.0.0
EXPOSE 3000
ENTRYPOINT ["datawizz"]
