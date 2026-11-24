#!/bin/sh
# Muse-IPTV 容器入口。MODE=all（默认）: worker 后台 + api 前台；
# MODE=api|worker|portal|admin: 只跑指定服务（compose 里分服务用）。
# 作者: Passwd Word
set -e
mkdir -p /app/data
case "${MODE:-all}" in
  api) exec /usr/local/bin/iptv-api ;;
  worker) exec /usr/local/bin/iptv-worker ;;
  portal) exec /usr/local/bin/iptv-portal ;;
  admin) exec /usr/local/bin/iptv-admin ;;
  all)
    /usr/local/bin/iptv-worker &
    /usr/local/bin/iptv-portal &
    /usr/local/bin/iptv-admin &
    exec /usr/local/bin/iptv-api
    ;;
  *) echo "unknown MODE=$MODE (api|worker|portal|admin|all)"; exit 1 ;;
esac
