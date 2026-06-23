# file: Dockerfile
# version: 1.0.0
# guid: 38b97a89-8b2a-4885-a89e-20a9fa03d6b1

FROM rust:1.96-bookworm AS builder

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /src/target/release/cockroach-rollout-agent /usr/local/bin/cockroach-rollout-agent

USER 65532:65532
ENTRYPOINT ["/usr/local/bin/cockroach-rollout-agent"]
