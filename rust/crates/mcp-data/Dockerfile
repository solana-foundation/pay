# syntax=docker/dockerfile:1.7
FROM rust:1.89-slim-bookworm AS builder
WORKDIR /app

RUN apt-get update \
 && apt-get install -y --no-install-recommends pkg-config ca-certificates git cmake libssl-dev libclang-dev \
 && rm -rf /var/lib/apt/lists/*

COPY . .

RUN cargo build --release -p mcp-data --bin mcp-data && \
    cp /app/target/release/mcp-data /mcp-data

FROM gcr.io/distroless/cc-debian12:nonroot
WORKDIR /app
COPY --from=builder /mcp-data /usr/local/bin/mcp-data

ENV PORT=8080
EXPOSE 8080
USER nonroot
ENTRYPOINT ["/usr/local/bin/mcp-data"]
