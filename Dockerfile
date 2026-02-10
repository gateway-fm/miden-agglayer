FROM rustlang/rust:nightly-bookworm-slim AS builder

WORKDIR /usr/src/app
COPY . .
RUN cargo build --profile=dev --bin=miden-agglayer-service

FROM debian:bookworm-slim

WORKDIR /app

COPY --from=builder /usr/src/app/target/debug/miden-agglayer-service /usr/local/bin/
RUN mkdir -p /var/lib/miden-agglayer-service

# 8546 - JSON-RPC HTTP
EXPOSE 8546

ENTRYPOINT ["miden-agglayer-service"]
CMD [ \
    "--chain-id=2", \
    "--miden-node=http://miden-node-001:57291", \
    "--miden-store-dir=/var/lib/miden-agglayer-service", \
    "--port=8546" \
]
