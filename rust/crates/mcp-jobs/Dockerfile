FROM rust:1.88-bookworm AS builder
WORKDIR /src
COPY . .
RUN cargo build --release -p mcp-jobs

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/mcp-jobs /usr/local/bin/mcp-jobs
USER 65532:65532
ENTRYPOINT ["mcp-jobs"]
