FROM rust:1.88-alpine as builder

WORKDIR /app
COPY src /app/src
COPY Cargo.lock /app/
COPY Cargo.toml /app/
RUN apk add --no-cache musl-dev
RUN cargo build --release

FROM scratch
COPY --from=builder /app/target/release/cloudflare-cname-switcher /app

EXPOSE 3000
CMD ["/app"]
