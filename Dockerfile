FROM rustlang/rust:nightly-bookworm-slim AS builder

WORKDIR /usr/src/app

# copy sources
COPY src src
COPY axum-jrpc axum-jrpc
COPY Cargo.* .

# build
ENV CARGO_REGISTRIES_CRATES_IO_PROTOCOL=sparse
RUN mkdir bin
RUN \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/src/app/target \
    cargo build --profile=release --bin=miden-agglayer-service --bin=bridge-out-tool \
    && cp target/release/miden-agglayer-service bin/miden-agglayer-service \
    && cp target/release/bridge-out-tool bin/bridge-out-tool

FROM debian:bookworm-slim

RUN apt-get update
RUN apt-get install -y ca-certificates curl

COPY --from=builder /usr/src/app/bin/miden-agglayer-service /usr/local/bin/
COPY --from=builder /usr/src/app/bin/bridge-out-tool /usr/local/bin/
RUN mkdir -p /var/lib/miden-agglayer-service

# 8546 - JSON-RPC HTTP
EXPOSE 8546

ENTRYPOINT ["miden-agglayer-service"]
# chain_id and network_id read from CHAIN_ID / NETWORK_ID env vars (clap env support)
CMD [ \
    "--miden-node=http://miden-node-001:57291", \
    "--miden-store-dir=/var/lib/miden-agglayer-service", \
    "--port=8546" \
]
