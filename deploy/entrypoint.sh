#!/bin/sh
# Muse-IPTV 容器入口。MODE=all（默认）: worker 后台 + api 前台；
# MODE=api|worker|portal|admin: 只跑指定服务（compose 里分服务用）。
# 门户 :8081、后台 :8082 在这里写死：Leptos 默认监听 127.0.0.1:3000，
# 不写死的话两个服务抢同一个端口，且容器外访问不到。
# 作者: Passwd Word
set -e
mkdir -p /app/data
case "${MODE:-all}" in
  api) exec /usr/local/bin/iptv-api ;;
  worker) exec /usr/local/bin/iptv-worker ;;
  portal) LEPTOS_SITE_ADDR=0.0.0.0:8081 exec /usr/local/bin/iptv-portal ;;
  admin) LEPTOS_SITE_ADDR=0.0.0.0:8082 exec /usr/local/bin/iptv-admin ;;
  all)
    /usr/local/bin/iptv-worker &
    LEPTOS_SITE_ADDR=0.0.0.0:8081 /usr/local/bin/iptv-portal &
    LEPTOS_SITE_ADDR=0.0.0.0:8082 /usr/local/bin/iptv-admin &
    exec /usr/local/bin/iptv-api
    ;;
  *) echo "unknown MODE=$MODE (api|worker|portal|admin|all)"; exit 1 ;;
esac
