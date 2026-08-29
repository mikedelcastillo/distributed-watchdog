FROM rust:1.98-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates iproute2 lm-sensors pciutils \
    && useradd --system --uid 10001 --home-dir /nonexistent --shell /usr/sbin/nologin watchdog \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/distributed-watchdog /usr/local/bin/distributed-watchdog
USER watchdog
WORKDIR /config
EXPOSE 7373
ENTRYPOINT ["/usr/local/bin/distributed-watchdog"]
CMD ["--config", "/config/config.toml", "serve"]
