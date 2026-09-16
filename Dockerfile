FROM rust:1.93 AS builder
WORKDIR /src
COPY . .
RUN cargo build --release

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/provisioner-operator /
ENTRYPOINT ["/provisioner-operator"]
