FROM rust:bookworm AS build

ENV CARGO_NET_GIT_FETCH_WITH_CLI=true

RUN apt-get update && apt-get install -y \
    ca-certificates \
    pkg-config \
    libssl-dev \
    libclang-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src

# Use HTTPS for public git deps instead of SSH
RUN git config --global url."https://github.com/".insteadOf "git@github.com:" && \
    git config --global url."https://github.com/".insteadOf "ssh://git@github.com/"

COPY . .

RUN cargo build --release --features gcp_kms

RUN cp target/release/pay /usr/local/bin/pay

# ── Runtime ──
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

RUN groupadd -r pay && useradd -r -g pay -s /sbin/nologin pay

COPY --from=build /usr/local/bin/pay /usr/local/bin/pay

WORKDIR /app

USER pay

ENTRYPOINT ["pay"]
