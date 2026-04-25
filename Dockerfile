FROM rust:1-alpine AS builder
RUN apk add --no-cache musl-dev openssl-dev pkgconfig
RUN cargo install mcpstead --locked --force

FROM alpine:latest
RUN apk add --no-cache ca-certificates
COPY --from=builder /usr/local/cargo/bin/mcpstead /usr/local/bin/mcpstead
EXPOSE 8766
VOLUME /etc/mcpstead
CMD ["mcpstead", "--config", "/etc/mcpstead/config.yaml"]
