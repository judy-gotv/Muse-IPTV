#!/bin/sh
# Muse-IPTV 一键安装脚本（Linux / macOS）
# 作者: Passwd Word
#
# 用法：
#   curl -fsSL https://raw.githubusercontent.com/judy-gotv/Muse-IPTV/main/install.sh | sh
#   # 或下载后：sh install.sh [--version v0.1.0] [--prefix /usr/local] [--no-service] [--port 8080]
#
# 行为：
#   1. 识别 OS/架构，下载对应 Release 包（全架构见 release.yml）
#   2. 校验 SHA-256（对照 Release 的 SHA256SUMS）
#   3. 安装 iptv-api / iptv-worker / iptv-portal / iptv-admin 四个二进制
#   4. 生成配置文件与随机密钥（PLAY_SECRET / ADMIN_SECRET / 管理员密码，只打印一次）
#   5. Linux 上写 systemd 服务并启动；macOS 上打印启动命令
set -eu

REPO="judy-gotv/Muse-IPTV"
VERSION="${VERSION:-}"
PREFIX="${PREFIX:-/usr/local}"
PORT="${PORT:-8080}"
NO_SERVICE=0

while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="$2"; shift 2 ;;
    --prefix) PREFIX="$2"; shift 2 ;;
    --port) PORT="$2"; shift 2 ;;
    --no-service) NO_SERVICE=1; shift ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
    *) echo "未知参数: $1"; exit 1 ;;
  esac
done

need() { command -v "$1" >/dev/null 2>&1 || { echo "缺少命令: $1，请先安装"; exit 1; }; }
need curl; need tar; need sha256sum || need shasum

OS="$(uname -s)"
ARCH="$(uname -m)"
case "$OS" in
  Linux) OS_ID="linux" ;;
  Darwin) OS_ID="macos" ;;
  *) echo "不支持的操作系统: $OS"; exit 1 ;;
esac
case "$ARCH" in
  x86_64|amd64) ARCH_ID="x86_64" ;;
  aarch64|arm64) ARCH_ID="aarch64" ;;
  armv7l|armv7) ARCH_ID="armv7" ;;
  *) echo "不支持的架构: $ARCH"; exit 1 ;;
esac

if [ -z "$VERSION" ]; then
  echo "正在查询最新版本…"
  VERSION="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" | grep '"tag_name"' | head -1 | sed -E 's/.*"([^"]+)".*/\1/')"
fi
[ -n "$VERSION" ] || { echo "无法获取最新版本，请用 --version 指定"; exit 1; }
echo "安装 Muse-IPTV $VERSION（$OS_ID/$ARCH_ID）"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT INT TERM
FILE="muse-iptv-$VERSION-$OS_ID-$ARCH_ID.tar.gz"
URL="https://github.com/$REPO/releases/download/$VERSION/$FILE"
echo "下载 $URL"
curl -fsSL -o "$TMP/$FILE" "$URL"
curl -fsSL -o "$TMP/SHA256SUMS" "https://github.com/$REPO/releases/download/$VERSION/SHA256SUMS"
echo "校验 SHA-256…"
(cd "$TMP" && grep " $FILE\$" SHA256SUMS | sha256sum -c - 2>/dev/null || (command -v shasum >/dev/null && (cd "$TMP" && grep " $FILE\$" SHA256SUMS | shasum -a 256 -c -)) || { echo "SHA-256 校验失败"; exit 1; })

tar -xzf "$TMP/$FILE" -C "$TMP"
# 包内布局：iptv-api / iptv-worker / iptv-portal / iptv-admin + README
BIN_DIR="$PREFIX/bin"
if [ -w "$BIN_DIR" ]; then
  INSTALL_BIN="$BIN_DIR"
else
  echo "$BIN_DIR 不可写，改用 sudo 安装（或用 --prefix 指定可写目录）"
  sudo mkdir -p "$BIN_DIR"
  INSTALL_BIN="$BIN_DIR"
  sudo cp "$TMP/iptv-api" "$TMP/iptv-worker" "$TMP/iptv-portal" "$TMP/iptv-admin" "$INSTALL_BIN/"
  sudo chmod +x "$INSTALL_BIN/iptv-api" "$INSTALL_BIN/iptv-worker" "$INSTALL_BIN/iptv-portal" "$INSTALL_BIN/iptv-admin"
  echo "二进制已装到 $INSTALL_BIN"
  # 配置仍继续（sudo 部分已完成）
  BIN_DIR="$INSTALL_BIN"
  SKIP_CP=1
fi
if [ "${SKIP_CP:-0}" != "1" ]; then
  mkdir -p "$BIN_DIR"
  cp "$TMP/iptv-api" "$TMP/iptv-worker" "$TMP/iptv-portal" "$TMP/iptv-admin" "$BIN_DIR/"
  chmod +x "$BIN_DIR/iptv-api" "$BIN_DIR/iptv-worker" "$BIN_DIR/iptv-portal" "$BIN_DIR/iptv-admin"
fi

# 配置：/etc/muse-iptv/.env（不可写时用 ~/.config/muse-iptv/.env）
if [ -w /etc ] || [ -d /etc/muse-iptv ]; then
  CONF_DIR="/etc/muse-iptv"
  DATA_DIR="/var/lib/muse-iptv"
else
  CONF_DIR="$HOME/.config/muse-iptv"
  DATA_DIR="$HOME/.local/share/muse-iptv"
fi
mkdir -p "$CONF_DIR" "$DATA_DIR" 2>/dev/null || sudo mkdir -p "$CONF_DIR" "$DATA_DIR"
rand_hex() { head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n'; }
if [ ! -f "$CONF_DIR/.env" ]; then
  PLAY_SECRET="$(rand_hex)"
  ADMIN_SECRET="$(rand_hex)"
  ADMIN_PASS="$(head -c 12 /dev/urandom | od -An -tx1 | tr -d ' \n')"
  cat > "$CONF_DIR/.env.tmp" <<EOF
# Muse-IPTV 配置（首次安装生成，密钥只打印一次，请妥善保存）
DATABASE_URL=sqlite://$DATA_DIR/iptv.db
PLAY_SECRET=$PLAY_SECRET
ADMIN_SECRET=$ADMIN_SECRET
PORTAL_BASE_URL=http://127.0.0.1:8081
BIND_ADDR=0.0.0.0:$PORT
IPTV_ADMIN_USER=admin
IPTV_ADMIN_PASSWORD=$ADMIN_PASS
EOF
  mv "$CONF_DIR/.env.tmp" "$CONF_DIR/.env"
  chmod 600 "$CONF_DIR/.env"
  echo ""
  echo "==================== 首次安装信息（只显示一次） ===================="
  echo "管理员账号：admin"
  echo "管理员密码：$ADMIN_PASS"
  echo "配置文件：$CONF_DIR/.env"
  echo "=================================================================="
  echo ""
else
  echo "配置文件已存在：$CONF_DIR/.env（保留原密钥与管理员密码）"
fi

# 服务
if [ "$NO_SERVICE" = "1" ]; then
  echo "跳过服务安装。手动启动："
  echo "  set -a; . $CONF_DIR/.env; set +a"
  echo "  $BIN_DIR/iptv-api"
  exit 0
fi

if [ "$OS_ID" = "linux" ] && command -v systemctl >/dev/null 2>&1; then
  UNIT="/etc/systemd/system/muse-iptv.service"
  cat > /tmp/muse-iptv.service <<EOF
[Unit]
Description=Muse-IPTV API + Worker
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
EnvironmentFile=$CONF_DIR/.env
ExecStartPre=/bin/sh -c 'mkdir -p $DATA_DIR'
ExecStart=$BIN_DIR/iptv-api
Restart=on-failure
RestartSec=5
# worker 与 api 同库，单独起一个服务亦可；此处由 api 进程外的 worker 单元负责
[Install]
WantedBy=multi-user.target
EOF
  cat > /tmp/muse-iptv-worker.service <<EOF
[Unit]
Description=Muse-IPTV Worker
After=network-online.target muse-iptv.service
Wants=network-online.target

[Service]
Type=simple
EnvironmentFile=$CONF_DIR/.env
ExecStart=$BIN_DIR/iptv-worker
Restart=on-failure
RestartSec=5
[Install]
WantedBy=multi-user.target
EOF
  if [ -w /etc/systemd/system ]; then
    cp /tmp/muse-iptv.service "$UNIT"
    cp /tmp/muse-iptv-worker.service /etc/systemd/system/muse-iptv-worker.service
    systemctl daemon-reload
    systemctl enable --now muse-iptv muse-iptv-worker
  else
    sudo cp /tmp/muse-iptv.service "$UNIT"
    sudo cp /tmp/muse-iptv-worker.service /etc/systemd/system/muse-iptv-worker.service
    sudo systemctl daemon-reload
    sudo systemctl enable --now muse-iptv muse-iptv-worker
  fi
  sleep 2
  if curl -fsSL "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1; then
    echo "服务已启动，健康检查通过：http://127.0.0.1:$PORT/healthz"
  else
    echo "服务已安装但健康检查未通过，请查看：systemctl status muse-iptv --no-pager"
  fi
  echo "门户：http://127.0.0.1:8081（需先在后台建 Token）  后台：http://127.0.0.1:8082/admin"
  echo "管理服务：systemctl status|restart muse-iptv muse-iptv-worker"
else
  echo "未检测到 systemd（或为 macOS），请手动启动："
  echo "  set -a; . $CONF_DIR/.env; set +a"
  echo "  $BIN_DIR/iptv-worker & $BIN_DIR/iptv-api"
  echo "健康检查：curl http://127.0.0.1:$PORT/healthz"
fi
echo "安装完成：Muse-IPTV $VERSION"
