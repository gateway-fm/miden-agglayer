FROM rustlang/rust:nightly-trixie-slim AS builder

WORKDIR /usr/src/app

# copy sources
COPY src src
COPY axum-jrpc axum-jrpc
COPY migrations migrations
# vendor/ carries the temporary miden-client [patch.crates-io] override —
# the build cannot resolve the patch path without it.
COPY vendor vendor
COPY Cargo.* .

# build
ENV CARGO_REGISTRIES_CRATES_IO_PROTOCOL=sparse
RUN mkdir bin
RUN \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/src/app/target \
    cargo build --profile=release --features=postgres --bin=miden-client-service --bin=bridge-out-tool --bin=bridge-autoclaim \
    && cp target/release/miden-client-service bin/miden-client-service \
    && cp target/release/bridge-out-tool bin/bridge-out-tool \
    && cp target/release/bridge-autoclaim bin/bridge-autoclaim

FROM debian:trixie-slim

RUN apt-get update && apt-get upgrade -y && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/src/app/bin/miden-client-service /usr/local/bin/
COPY --from=builder /usr/src/app/bin/bridge-out-tool /usr/local/bin/
COPY --from=builder /usr/src/app/bin/bridge-autoclaim /usr/local/bin/
COPY LICENSE-APACHE LICENSE-MIT /usr/share/licenses/miden-client/
COPY axum-jrpc/LICENSE /usr/share/licenses/miden-client/LICENSE-axum-jrpc-MIT
RUN mkdir -p /var/lib/miden-client-service

# 8546 - JSON-RPC HTTP
EXPOSE 8546

ENTRYPOINT ["miden-client-service"]
# chain_id and network_id read from CHAIN_ID / NETWORK_ID env vars (clap env support)
CMD [ \
    "--miden-node=http://miden-node-001:57291", \
    "--miden-store-dir=/var/lib/miden-client-service", \
    "--port=8546" \
]
