FROM rust:1-bookworm as builder

WORKDIR /app/src
RUN USER=root cargo new --bin replex
COPY Cargo.toml Cargo.lock ./replex/

WORKDIR /app/src/replex
RUN cargo build --release

# COPY ./ ./
COPY Cargo.toml Cargo.lock ./
COPY ./src ./src
RUN cargo build --release

FROM debian:bookworm-slim as replex
RUN apt update \
    && apt install -y openssl ca-certificates curl \
    && apt clean \
    && rm -rf /var/lib/apt/lists/* /tmp/* /var/tmp/*
COPY --from=builder /app/src/replex/target/release/replex /app/

# Run as an unprivileged user. The process only needs to write to /data
# (cache); everything else stays read-only at runtime.
RUN useradd -m -u 10001 replex \
    && mkdir -p /data/replex-cache \
    && chown -R replex:replex /data /app
USER replex

WORKDIR /app

EXPOSE 80

# The binary serves /ping; use it to detect a hung process.
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
  CMD curl -fsS http://localhost:80/ping || exit 1

CMD ["/app/replex"]
