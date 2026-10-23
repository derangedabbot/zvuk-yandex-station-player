# Сборка под колонку: aarch64 + musl, статически.
#
# Колонка: ядро aarch64, userspace 64-битный (проверено: ELF class 2 у наших утилит).
# На Apple Silicon этот образ собирается нативно — эмуляции нет.
FROM rust:1-alpine AS builder

RUN apk add --no-cache musl-dev gcc make perl linux-headers \
    && rustup target add aarch64-unknown-linux-musl

WORKDIR /src
COPY Cargo.toml ./
COPY src ./src
COPY queries ./queries

# полностью статический бинарник: на колонке нет наших библиотек, и они не нужны
ENV RUSTFLAGS="-C target-feature=+crt-static"
RUN cargo build --release --target aarch64-unknown-linux-musl

RUN mkdir -p /out && cp target/aarch64-unknown-linux-musl/release/zvukd /out/zvukd \
    && strip /out/zvukd 2>/dev/null || true

FROM scratch
COPY --from=0 /out/zvukd /zvukd
