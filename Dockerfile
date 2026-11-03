# Build a fully static musl binary, then ship it on a tiny Alpine base.
# Works for both linux/amd64 and linux/arm64 under `docker buildx`.
FROM rust:1-alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY tests ./tests
RUN cargo build --release --locked --bin laliga-proxy

FROM alpine:3.24
RUN apk add --no-cache ca-certificates \
    && adduser -D -u 10001 app
COPY --from=builder /src/target/release/laliga-proxy /usr/local/bin/laliga-proxy
USER app
EXPOSE 8888 9090
ENTRYPOINT ["/usr/local/bin/laliga-proxy"]
