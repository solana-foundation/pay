FROM rust:1.87-bookworm AS build
RUN apt-get update && apt-get install -y nodejs npm && npm i -g pnpm
WORKDIR /src
COPY pdb/ pdb/
RUN cd pdb && pnpm i --frozen-lockfile && pnpm build
COPY rust/ rust/
RUN cd rust && cargo build --release --bin pay

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/rust/target/release/pay /usr/local/bin/
ENTRYPOINT ["pay"]
