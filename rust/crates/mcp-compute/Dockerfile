# syntax=docker/dockerfile:1.7
FROM rust:1.89-slim-bookworm AS builder
WORKDIR /app

RUN apt-get update \
 && apt-get install -y --no-install-recommends pkg-config ca-certificates git cmake libssl-dev libclang-dev \
 && rm -rf /var/lib/apt/lists/*

COPY . .

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --release -p mcp-compute --bin mcp-compute && \
    cp /app/target/release/mcp-compute /mcp-compute

FROM gcr.io/distroless/cc-debian12:nonroot
WORKDIR /app
COPY --from=builder /mcp-compute /usr/local/bin/mcp-compute

ENV PORT=8080
EXPOSE 8080
USER nonroot
ENTRYPOINT ["/usr/local/bin/mcp-compute"]
