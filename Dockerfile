# Muse-IPTV 全 Rust 播放平台 — 多架构镜像（amd64 / arm64）
# 作者: Passwd Word
# 构建：docker buildx build --platform linux/amd64,linux/arm64 -t ghcr.io/judy-gotv/muse-iptv:latest .
FROM rust:1.100-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY xtask ./xtask
COPY locales ./locales
COPY migrations ./migrations
# B4：hydrate 产物——装 wasm 目标 + cargo-leptos，先构建 portal/admin 的
# 前端（target/site/pkg/*.js + *_bg.wasm）与服务端二进制（编译时烘入
# LEPTOS_OUTPUT_NAME，见 crates/*/Cargo.toml 的 [package.metadata.leptos]），
# 再构建其余三个二进制。没有这一步，shell 引用的 /pkg/*.js 全是 404，
# 语言切换与全部 WASM 交互都是死的（v0.1.1 现场复现）。
# 工具链须 ≥1.97（cargo-leptos 0.3.12 的 cargo-util-schemas 0.14.3 要
# 1.97），rustc 1.89 下编译失败 exit 101（v0.1.2 首建全红）；--locked
# 锁定其发布时 Cargo.lock，避免日后依赖再抬 MSRV 又红。版本见
# rust-toolchain.toml（基础镜像 rust:1.100-bookworm 与之对应）。
# B5（2026-11-26，用户选方案 B）：cargo-leptos 找不到 PATH 上的 wasm-bindgen
# 时会下载预编译二进制，wasm-bindgen 0.2.130 的预编译版要求 GLIBC_2.38，
# 而 bookworm 只有 glibc 2.36，cargo leptos build 在前端收尾阶段 exit 1，
# 两个 Docker run（main/tag）都红在 build-push（v0.1.2 第 80 轮定位）。
# cargo-leptos 0.3.12 的 Exe::get 优先用 PATH 上的 wasm-bindgen（which），
# 找到就不再下载。于是先从源码装 wasm-bindgen-cli（版本须与 Cargo.lock 里
# 的 wasm-bindgen 0.2.130 一致），编出来的二进制链接本机 glibc 2.36，
# 不再碰预编译包。代价：构建变慢，arm64 在 QEMU 下从源码编译更慢。
# 不用 --locked：wasm-bindgen-cli 发布包里没有 Cargo.lock，--locked 会报错。
RUN rustup target add wasm32-unknown-unknown \
    && cargo install cargo-leptos --version "=0.3.12" --locked \
    && cargo install wasm-bindgen-cli --version "=0.2.130" \
    && cargo leptos build --release -p iptv-portal \
    && cargo leptos build --release -p iptv-admin \
    && cargo build --release -p iptv-api -p iptv-worker -p iptv-gateway
# 五个二进制 + 前端 site 目录（两项目各用各的 site-root，见各包 Cargo.toml
# 的 [package.metadata.leptos]，这里把两边的 pkg/ 合并成一个 site/ 供运行时
# LEPTOS_SITE_ROOT 指向；直接 COPY target/site 会丢其中一个项目，见 B4）。
RUN mkdir -p /out /out/site/pkg && cp target/release/iptv-api target/release/iptv-worker target/release/iptv-portal target/release/iptv-admin target/release/iptv-gateway /out/ && cp target/site-portal/pkg/* target/site-admin/pkg/* /out/site/pkg/ && ls -la /out && ls -la /out/site/pkg/

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates sqlite3 wget && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /out/iptv-api /usr/local/bin/iptv-api
COPY --from=builder /out/iptv-worker /usr/local/bin/iptv-worker
COPY --from=builder /out/iptv-portal /usr/local/bin/iptv-portal
COPY --from=builder /out/iptv-admin /usr/local/bin/iptv-admin
COPY --from=builder /out/iptv-gateway /usr/local/bin/iptv-gateway
# B4：前端 hydrate 产物（/app/site/pkg/*.js），portal/admin 由 LEPTOS_SITE_ROOT 指向它。
# builder 已把两项目的 pkg/ 合并到 /out/site/pkg/（见上）；/app/target/site 不存在
# （两项目 site-root 各为 target/site-portal 与 target/site-admin），直接 COPY 它会在
# buildx 算缓存键时报 "/app/target/site": not found（v0.1.2 Docker(tag) 实测）。
COPY --from=builder /out/site /app/site
COPY deploy/entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh
VOLUME ["/app/data"]
ENV DATABASE_URL=sqlite:///app/data/iptv.db
ENV BIND_ADDR=0.0.0.0:8080
ENV GATEWAY_BIND_ADDR=0.0.0.0:8081
ENV LEPTOS_SITE_ROOT=/app/site
ENV MODE=all
# 对外只暴露网关一个端口 :8081；API :8080 与门户/后台内部门口 :18081/:18082 不对外。
EXPOSE 8081
ENTRYPOINT ["/entrypoint.sh"]
