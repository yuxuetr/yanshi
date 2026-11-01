# syntax=docker/dockerfile:1

FROM rust:1-bookworm AS builder

WORKDIR /app

RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates pkg-config libssl-dev protobuf-compiler \
  && rm -rf /var/lib/apt/lists/*

COPY . .

ARG PACKAGE=yanshi-server
ARG BIN=yanshi-server

RUN cargo build --release -p "${PACKAGE}" --bin "${BIN}"

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates curl libssl3 \
  && rm -rf /var/lib/apt/lists/* \
  && useradd --create-home --uid 10001 --shell /usr/sbin/nologin yanshi

ARG BIN=yanshi-server

COPY --from=builder /app/target/release/${BIN} /usr/local/bin/yanshi

USER yanshi
ENV PORT=3000
ENV RUST_LOG=info
EXPOSE 3000

ENTRYPOINT ["/usr/local/bin/yanshi"]
