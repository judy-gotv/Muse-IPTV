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
