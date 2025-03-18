FROM rust:1.85 AS builder

WORKDIR /app

COPY . .

RUN cargo build --release

FROM debian:12.9

RUN useradd -m -u 1001 backend

USER backend

WORKDIR /app

COPY --from=builder --chown=backend:backend /app/target/release/vn_back /app/.env .

EXPOSE 3000

CMD ["./vn_back"]