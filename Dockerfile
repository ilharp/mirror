FROM rust:1.73.0-bullseye

ENV DEBIAN_FRONTEND=noninteractive

WORKDIR /build
COPY . /build
RUN apt update && apt install -y musl-tools
RUN rustup target add x86_64-unknown-linux-musl
RUN cargo build --release --target x86_64-unknown-linux-musl

FROM alpine:3.17.2

WORKDIR /app
COPY --from=0 /build/target/x86_64-unknown-linux-musl/release/mirror /app/mirror
CMD [ "/app/mirror" ]
