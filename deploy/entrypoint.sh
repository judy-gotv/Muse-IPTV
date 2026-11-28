#!/bin/sh
# Muse-IPTV 容器入口。MODE=all（默认）: worker 后台 + api + 门户 + 后台，
# 再由 iptv-gateway 在 :8081 对外统一收口——MODE=all 时 API 只绑 127.0.0.1:8080、
# 门户/后台只走内部端口 18081/18082，容器外看不到它们，只看到网关一个端口。
# 对外只暴露一个端口：门户 http://服务器IP:8081、后台 http://服务器IP:8081/admin、
# 健康检查 curl http://127.0.0.1:8081/healthz；域名反代只需指到 :8081。
# MODE=api|worker|portal|admin|gateway: 只跑指定服务（compose 里分服务用；
# 单跑 portal/admin 时仍默认 8081/8082，老用法不变）。
# 作者: Passwd Word
set -e
mkdir -p /app/data

# N3/M-C：密钥持久化。优先级：显式环境变量（非空）> 数据卷持久文件
# /app/data/.secrets（每行 KEY=值）> 生成一次并写入文件复用。
# 生成只用 /dev/urandom + od（镜像内必有，不依赖 openssl/xxd）；只记
# 日志“已生成”，绝不打印值。轮换：删 /app/data/.secrets + 重启，
# 全体会话随之失效。MODE=all 单容器内五个进程共享同一份 env。
# 注意：这里只补齐密钥；Config::from_env 仍是 fail-closed 的最后一道，
# 密钥最终仍为空时 API 会拒绝启动（不要把本段逻辑绕过去）。
SECRETS_FILE=/app/data/.secrets
ensure_secret() {
  _key="$1"
  case "$_key" in
    PLAY_SECRET) _val="${PLAY_SECRET:-}" ;;
    ADMIN_SECRET) _val="${ADMIN_SECRET:-}" ;;
    *) echo "unknown secret key: $_key"; exit 1 ;;
  esac
  if [ -n "$_val" ]; then return 0; fi
  if [ -f "$SECRETS_FILE" ]; then
    chmod 600 "$SECRETS_FILE"
    _val="$(sed -n "s/^$_key=//p" "$SECRETS_FILE" | head -1)"
  fi
  if [ -z "$_val" ]; then
    _val="$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"
    printf '%s=%s\n' "$_key" "$_val" >> "$SECRETS_FILE"
    chmod 600 "$SECRETS_FILE"
    echo "generated persistent $_key (stored in $SECRETS_FILE, value not logged)"
  fi
  # 赋值经 export 一次完成（不用 eval，避免值里的特殊字符被二次解析）。
  export "$_key=$_val"
}
ensure_secret PLAY_SECRET
ensure_secret ADMIN_SECRET
case "${MODE:-all}" in
  api) exec /usr/local/bin/iptv-api ;;
  worker) exec /usr/local/bin/iptv-worker ;;
  portal) LEPTOS_SITE_ADDR=0.0.0.0:8081 exec /usr/local/bin/iptv-portal ;;
  admin) LEPTOS_SITE_ADDR=0.0.0.0:8082 exec /usr/local/bin/iptv-admin ;;
  gateway) exec /usr/local/bin/iptv-gateway ;;
  all)
    /usr/local/bin/iptv-worker &
    BIND_ADDR=127.0.0.1:8080 /usr/local/bin/iptv-api &
    LEPTOS_SITE_ADDR=0.0.0.0:18081 /usr/local/bin/iptv-portal &
    LEPTOS_SITE_ADDR=0.0.0.0:18082 /usr/local/bin/iptv-admin &
    IPTV_PORTAL_BASE=http://127.0.0.1:18081 IPTV_ADMIN_BASE=http://127.0.0.1:18082 \
      exec /usr/local/bin/iptv-gateway
    ;;
  *) echo "unknown MODE=$MODE (api|worker|portal|admin|gateway|all)"; exit 1 ;;
esac
