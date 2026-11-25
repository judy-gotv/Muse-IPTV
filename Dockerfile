# Muse-IPTV 全 Rust 播放平台 — 多架构镜像（amd64 / arm64）
# 作者: Passwd Word
# 构建：docker buildx build --platform linux/amd64,linux/arm64 -t ghcr.io/judy-gotv/muse-iptv:latest .
FROM rust:1.89-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY xtask ./xtask
COPY locales ./locales
COPY migrations ./migrations
RUN cargo build --release --bins
# 五个二进制：iptv-api / iptv-worker / iptv-portal / iptv-admin / iptv-gateway
RUN mkdir -p /out && cp target/release/iptv-api target/release/iptv-worker target/release/iptv-portal target/release/iptv-admin target/release/iptv-gateway /out/ && ls -la /out

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates sqlite3 wget && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /out/iptv-api /usr/local/bin/iptv-api
COPY --from=builder /out/iptv-worker /usr/local/bin/iptv-worker
COPY --from=builder /out/iptv-portal /usr/local/bin/iptv-portal
COPY --from=builder /out/iptv-admin /usr/local/bin/iptv-admin
COPY --from=builder /out/iptv-gateway /usr/local/bin/iptv-gateway
COPY deploy/entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh
VOLUME ["/app/data"]
ENV DATABASE_URL=sqlite:///app/data/iptv.db
ENV BIND_ADDR=0.0.0.0:8080
ENV GATEWAY_BIND_ADDR=0.0.0.0:8081
ENV MODE=all
# 对外只暴露网关一个端口 :8081；API :8080 与门户/后台内部门口 :18081/:18082 不对外。
EXPOSE 8081
ENTRYPOINT ["/entrypoint.sh"]
