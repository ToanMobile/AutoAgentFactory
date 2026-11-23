# Multi-stage hardened Linux runner image for AutoAgentFactory (`aaf` + `aaf-hook`).

FROM rust:1.89-slim-bookworm AS builder

WORKDIR /src

RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

RUN cargo build --release --locked -p aaf-cli -p aaf-hook

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bubblewrap \
        ca-certificates \
        git \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 aaf \
    && useradd --uid 10001 --gid 10001 --create-home --home-dir /home/aaf --shell /bin/sh aaf \
    && mkdir -p /workspace /home/aaf/.local/share/autofactory /home/aaf/.config/autofactory \
    && chown -R 10001:10001 /workspace /home/aaf

COPY --from=builder --chmod=0755 /src/target/release/aaf /usr/local/bin/aaf
COPY --from=builder --chmod=0755 /src/target/release/aaf-hook /usr/local/bin/aaf-hook

USER 10001:10001
WORKDIR /workspace

ENV HOME=/home/aaf \
    AAF_DATA_DIR=/home/aaf/.local/share/autofactory \
    AAF_CONFIG_DIR=/home/aaf/.config/autofactory

ENTRYPOINT ["aaf"]
CMD ["--help"]
