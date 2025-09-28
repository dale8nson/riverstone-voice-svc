FROM rust:1.81-bullseye AS builder

RUN apt-get update -y && apt-get install -y --no-install-recommends \
    pkg-config libsqlite3-dev ca-certificates && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Leverage caching: copy manifests first
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src && echo "fn main(){}" > src/main.rs && \
    cargo build --release && rm -rf target/release/deps/riverstone_voice_svc*

# Now copy real sources and build
COPY . .
RUN cargo build --release --locked

FROM debian:bookworm-slim AS runtime
RUN apt-get update -y && apt-get install -y --no-install-recommends \
    libsqlite3-0 ca-certificates && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/target/release/riverstone-voice-svc /app/riverstone-voice-svc

ENV RUST_LOG=info
# Will be overridden by Fly env but good local default
ENV BIND=0.0.0.0:8080

EXPOSE 8080

CMD ["/app/riverstone-voice-svc"]

