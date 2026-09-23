# syntax=docker/dockerfile:1

# ---- build ----
FROM rust:1.85-bookworm AS builder
WORKDIR /src
COPY . .
RUN cargo build --release --locked \
    && strip target/release/dunlin

# ---- runtime ----
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tzdata \
    && rm -rf /var/lib/apt/lists/*
RUN useradd --system --home /var/lib/dunlin --create-home --shell /usr/sbin/nologin dunlin
COPY --from=builder /src/target/release/dunlin /usr/local/bin/dunlin
COPY dunlin.example.toml /etc/dunlin/dunlin.example.toml
USER dunlin
WORKDIR /var/lib/dunlin
VOLUME ["/var/lib/dunlin"]
EXPOSE 8080
ENTRYPOINT ["dunlin"]
CMD ["--config", "/etc/dunlin/dunlin.toml"]
