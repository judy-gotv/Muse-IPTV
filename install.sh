#!/bin/sh
# Muse-IPTV 一键安装脚本（Linux / macOS）
# 作者: Passwd Word
#
# 用法：
#   curl -fsSL https://raw.githubusercontent.com/judy-gotv/Muse-IPTV/main/install.sh | sh
#   # 或下载后：sh install.sh [选项]
# 选项：
#   --version vX.Y.Z    指定版本（默认查最新 Release）
#   --prefix DIR        安装目录前缀（默认 /usr/local）
#   --port N            对外端口（首次安装时用；已安装时=只改端口并自动重启，不下载）
#   --admin-user NAME   管理员账号（只在首次安装时生效，默认 admin）
#   --admin-pass PASS   管理员密码（只在首次安装时生效；不给则交互式问，留空随机生成）
#   --reinstall         已是最新版也强制重新下载安装
#   --no-service        不装服务，只装二进制并打印手动启动命令
#   -h / --help         显示本帮助
#
# 行为：
#   全新安装：交互式问端口 / 账号 / 密码（明文显示，核对后确认），再下载安装。
#   重跑脚本：已是最新版则直接提示（交互式可改端口）；有新版则在线升级
#     （换二进制、保留 .env 与数据、重启服务）。
#   改端口：sh install.sh --port <新端口>，只改 .env 并自动重启服务生效。
# 对外只暴露网关一个端口（默认 :8081）：门户 /、后台 /admin、API /api/* 与
# /healthz 全走它；API :8080 与门户/后台 :18081/:18082 只绑本机。
set -eu

REPO="judy-gotv/Muse-IPTV"
VERSION="${VERSION:-}"
PREFIX="${PREFIX:-/usr/local}"
REQ_PORT="${PORT:-}"
REQ_ADMIN_USER="${ADMIN_USER:-}"
REQ_ADMIN_PASS="${ADMIN_PASS:-}"
REINSTALL=0
NO_SERVICE=0

while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="$2"; shift 2 ;;
    --prefix) PREFIX="$2"; shift 2 ;;
    --port) REQ_PORT="$2"; shift 2 ;;
    --admin-user) REQ_ADMIN_USER="$2"; shift 2 ;;
    --admin-pass) REQ_ADMIN_PASS="$2"; shift 2 ;;
    --reinstall) REINSTALL=1; shift ;;
    --no-service) NO_SERVICE=1; shift ;;
    -h|--help) sed -n '2,24p' "$0"; exit 0 ;;
    *) echo "未知参数: $1"; exit 1 ;;
  esac
done

need() { command -v "$1" >/dev/null 2>&1 || { echo "缺少命令: $1，请先安装"; exit 1; }; }
need curl; need tar; need sha256sum || need shasum

# 端口早校验：别等查完最新版才发现端口非法。
valid_port() { case "$1" in ''|*[!0-9]*) return 1 ;; *) [ "$1" -ge 1 ] && [ "$1" -le 65535 ] ;; esac; }
if [ -n "$REQ_PORT" ]; then valid_port "$REQ_PORT" || { echo "端口无效：$REQ_PORT（须为 1-65535）"; exit 1; }; fi

# --- 交互式输入（curl | sh 时 stdin 是脚本本身，必须读 /dev/tty） ---
# 注意：[ -r /dev/tty ] 测的是设备节点权限，没控制终端时也返回真；exec 重定向失败
# 在 dash 里是致命错误，所以用外部命令 head 试开（读 0 字节，不消耗输入）。
HAVE_TTY=0
if head -c 0 /dev/tty 2>/dev/null; then HAVE_TTY=1; fi
tty_out() { if [ "$HAVE_TTY" = "1" ]; then printf "%s\n" "$*" > /dev/tty; else printf "%s\n" "$*" >&2; fi; }
ask() { # ask "提示" "默认值" -> $ASK_VAL（无终端时直接取默认值）
  if [ "$HAVE_TTY" = "1" ]; then
    printf "%s [%s]: " "$1" "$2" > /dev/tty
    IFS= read -r ASK_VAL < /dev/tty || ASK_VAL=""
    [ -z "$ASK_VAL" ] && ASK_VAL="$2"
  else
    ASK_VAL="$2"
  fi
  return 0 # 末尾的 [ -z … ] && … 在用户输入非空时返回 1，set -e 会误杀脚本，必须显式返回 0。
}
ask_yes() { # 默认 Yes，返回 0=是 / 1=否（无终端时默认是）
  if [ "$HAVE_TTY" = "1" ]; then
    printf "%s [Y/n]: " "$1" > /dev/tty
    IFS= read -r _yn < /dev/tty || _yn=""
    case "$_yn" in n|N|no|NO) return 1 ;; *) return 0 ;; esac
  else
    return 0
  fi
}
ask_port() { # ask_port 默认值 -> $PORT（交互式会循环问到合法为止）
  while :; do
    ask "对外端口" "$1"
    if valid_port "$ASK_VAL"; then PORT="$ASK_VAL"; return 0; fi
    tty_out "端口无效：请输入 1-65535 的数字。"
  done
}
rand_hex() { head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n'; }

# --- .env 读写 ---
env_val() { # env_val 文件 KEY -> 值（没有则空）
  sed -n "s/^$2=//p" "$1" 2>/dev/null | head -1
}
set_env_key() { # set_env_key 文件 KEY VALUE（没有则追加；可移植写法，不用 sed -i）
  _f="$1"; _k="$2"; _v="$3"
  grep -v "^$_k=" "$_f" > "$_f.tmp" 2>/dev/null || true
  echo "$_k=$_v" >> "$_f.tmp"
  mv "$_f.tmp" "$_f"
  chmod 600 "$_f"
}

# --- 服务 ---
check_health() { # $PORT 已定：curl 健康检查，返回 0/1
  if curl -fsSL "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1; then
    echo "健康检查通过：http://127.0.0.1:$PORT/healthz"
    return 0
  else
    echo "健康检查未通过（端口 $PORT），请查看：systemctl status muse-iptv-gateway --no-pager"
    return 1
  fi
}
restart_services() { # 重启全部五个服务并做健康检查（改端口 / 升级后调用）
  if [ "$OS_ID" = "linux" ] && command -v systemctl >/dev/null 2>&1; then
    echo "正在重启服务…"
    systemctl restart muse-iptv muse-iptv-worker muse-iptv-portal muse-iptv-admin muse-iptv-gateway || echo "警告：服务重启失败，请查看 systemctl status"
    sleep 2
    check_health || true
  else
    echo "未检测到 systemd，请手动重启五个服务生效。"
  fi
}
show_summary() { # 打印当前端口 / 账号 / 密码（明文，防忘）与访问地址
  _u="$(env_val "$CONF_DIR/.env" IPTV_ADMIN_USER)"
  _p="$(env_val "$CONF_DIR/.env" IPTV_ADMIN_PASSWORD)"
  echo ""
  echo "==================== Muse-IPTV ===================="
  echo "端口：$PORT"
  echo "管理员账号：${_u:-admin}"
  echo "管理员密码：${_p:-（见 $CONF_DIR/.env）}"
  echo "门户：http://127.0.0.1:$PORT（需先在后台建 Token）"
  echo "后台：http://127.0.0.1:$PORT/admin"
  echo "健康检查：curl http://127.0.0.1:$PORT/healthz"
  echo "配置文件：$CONF_DIR/.env"
  echo "==================================================="
  echo ""
}

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

# 配置目录（/etc/muse-iptv/.env，不可写时用 ~/.config/muse-iptv/.env）
if [ -w /etc ] || [ -d /etc/muse-iptv ]; then
  CONF_DIR="/etc/muse-iptv"
  DATA_DIR="/var/lib/muse-iptv"
else
  CONF_DIR="$HOME/.config/muse-iptv"
  DATA_DIR="$HOME/.local/share/muse-iptv"
fi

# --- 已安装检测 ---
INSTALLED=0
INSTALLED_VERSION=""
CURRENT_PORT=8081
if [ -f "$CONF_DIR/.env" ]; then
  INSTALLED=1
  [ -f "$CONF_DIR/version" ] && INSTALLED_VERSION="$(cat "$CONF_DIR/version" 2>/dev/null)"
  _cp="$(env_val "$CONF_DIR/.env" GATEWAY_BIND_ADDR | sed 's/.*://')"
  valid_port "$_cp" && CURRENT_PORT="$_cp"
fi

# --- 只改端口（已安装 + --port，不下载不升级） ---
if [ "$INSTALLED" = "1" ] && [ -n "$REQ_PORT" ] && [ -z "$VERSION" ] && [ "$REINSTALL" = "0" ]; then
  if [ "$REQ_PORT" = "$CURRENT_PORT" ]; then
    echo "端口已经是 $REQ_PORT，无需修改。"
    exit 0
  fi
  if [ "$OS_ID" = "linux" ] && command -v systemctl >/dev/null 2>&1; then
    if [ -f /etc/systemd/system/muse-iptv.service ] && [ ! -f /etc/systemd/system/muse-iptv-gateway.service ]; then
      echo "当前安装是旧版本（没有网关服务），请先重跑脚本升级，再改端口。"
      exit 1
    fi
  fi
  set_env_key "$CONF_DIR/.env" "GATEWAY_BIND_ADDR" "0.0.0.0:$REQ_PORT"
  PORT="$REQ_PORT"
  echo "端口已改成 $REQ_PORT。"
  if [ "$NO_SERVICE" = "1" ]; then
    echo "已跳过服务操作，请手动重启服务生效。"
  else
    restart_services
  fi
  show_summary
  exit 0
fi

# --- 确定目标版本 ---
if [ -z "$VERSION" ]; then
  echo "正在查询最新版本…"
  VERSION="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" | grep '"tag_name"' | head -1 | sed -E 's/.*"([^"]+)".*/\1/')"
fi
[ -n "$VERSION" ] || { echo "无法获取最新版本，请用 --version 指定"; exit 1; }

# --- 已是最新版 ---
if [ "$INSTALLED" = "1" ] && [ "$VERSION" = "$INSTALLED_VERSION" ] && [ "$REINSTALL" = "0" ]; then
  if [ -n "$REQ_PORT" ]; then
    # --version 与 --port 同时给了：版本相同，只改端口
    if [ "$REQ_PORT" != "$CURRENT_PORT" ]; then
      set_env_key "$CONF_DIR/.env" "GATEWAY_BIND_ADDR" "0.0.0.0:$REQ_PORT"
      PORT="$REQ_PORT"
      echo "已是最新版本 $VERSION，端口已改成 $REQ_PORT。"
      [ "$NO_SERVICE" = "1" ] || restart_services
      show_summary
    else
      echo "已是最新版本 $VERSION，端口已经是 $REQ_PORT。"
    fi
    exit 0
  fi
  if [ "$HAVE_TTY" = "1" ]; then
    ask_port "$CURRENT_PORT"
    if [ "$PORT" != "$CURRENT_PORT" ]; then
      set_env_key "$CONF_DIR/.env" "GATEWAY_BIND_ADDR" "0.0.0.0:$PORT"
      echo "已是最新版本 $VERSION，端口已改成 $PORT。"
      [ "$NO_SERVICE" = "1" ] || restart_services
      show_summary
      exit 0
    fi
  fi
  echo "已是最新版本 $VERSION，无需升级。如需改端口：sh install.sh --port <新端口>"
  exit 0
fi

# --- 确定本次安装的端口 / 账号 / 密码 ---
if [ "$INSTALLED" = "1" ]; then
  if [ -n "$REQ_ADMIN_USER" ] || [ -n "$REQ_ADMIN_PASS" ]; then
    echo "注意：已有管理员账号，--admin-user / --admin-pass 只在首次安装时生效；要换密码请在后台新建管理员账号。"
  fi
  if [ -n "$REQ_PORT" ]; then
    PORT="$REQ_PORT"
  elif [ "$HAVE_TTY" = "1" ]; then
    ask_port "$CURRENT_PORT"
  else
    PORT="$CURRENT_PORT"
  fi
  echo "升级到 $VERSION（端口 $PORT）"
else
  if [ -n "$REQ_PORT" ]; then
    PORT="$REQ_PORT"
  else
    ask_port 8081
  fi
  if [ -n "$REQ_ADMIN_USER" ]; then
    ADMIN_USER="$REQ_ADMIN_USER"
  else
    ask "管理员账号" "admin"
    ADMIN_USER="$ASK_VAL"
  fi
  if [ -n "$REQ_ADMIN_PASS" ]; then
    ADMIN_PASS="$REQ_ADMIN_PASS"
  else
    ask "管理员密码（明文输入，留空随机生成）" ""
    ADMIN_PASS="$ASK_VAL"
    [ -z "$ADMIN_PASS" ] && ADMIN_PASS="$(head -c 12 /dev/urandom | od -An -tx1 | tr -d ' \n')"
  fi
  if [ "$HAVE_TTY" = "1" ]; then
    echo ""
    echo "---------- 安装信息确认（明文显示，请核对） ----------"
    echo "端口：$PORT"
    echo "管理员账号：$ADMIN_USER"
    echo "管理员密码：$ADMIN_PASS"
    echo "------------------------------------------------------"
    ask_yes "确认无误，开始安装？" || { echo "已取消，请重新运行脚本。"; exit 0; }
  fi
  echo "安装 Muse-IPTV $VERSION（$OS_ID/$ARCH_ID）"
fi

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
# 包内布局：顶层目录 muse-iptv-<版本>-<os>-<arch>/ 下是
# iptv-api / iptv-worker / iptv-portal / iptv-admin / iptv-gateway + README。
# （旧脚本直接按 $TMP/iptv-api 取，顶层目录在就一定取不到——从未跑通过。）
PKG_DIR="$TMP/muse-iptv-$VERSION-$OS_ID-$ARCH_ID"
[ -d "$PKG_DIR" ] || PKG_DIR="$TMP"
# 老 Release 包没有网关，直接报错，别装出一个缺网关的半套。
for b in iptv-api iptv-worker iptv-portal iptv-admin iptv-gateway; do
  [ -f "$PKG_DIR/$b" ] || { echo "错误：Release 包 $FILE 里没有 $b（该版本太旧，不含单端口网关），请用更新的版本。"; exit 1; }
done
BIN_DIR="$PREFIX/bin"
if [ -w "$BIN_DIR" ]; then
  INSTALL_BIN="$BIN_DIR"
else
  echo "$BIN_DIR 不可写，改用 sudo 安装（或用 --prefix 指定可写目录）"
  sudo mkdir -p "$BIN_DIR"
  INSTALL_BIN="$BIN_DIR"
  sudo cp "$PKG_DIR/iptv-api" "$PKG_DIR/iptv-worker" "$PKG_DIR/iptv-portal" "$PKG_DIR/iptv-admin" "$PKG_DIR/iptv-gateway" "$INSTALL_BIN/"
  sudo chmod +x "$INSTALL_BIN/iptv-api" "$INSTALL_BIN/iptv-worker" "$INSTALL_BIN/iptv-portal" "$INSTALL_BIN/iptv-admin" "$INSTALL_BIN/iptv-gateway"
  echo "二进制已装到 $INSTALL_BIN"
  # 配置仍继续（sudo 部分已完成）
  BIN_DIR="$INSTALL_BIN"
  SKIP_CP=1
fi
if [ "${SKIP_CP:-0}" != "1" ]; then
  mkdir -p "$BIN_DIR"
  cp "$PKG_DIR/iptv-api" "$PKG_DIR/iptv-worker" "$PKG_DIR/iptv-portal" "$PKG_DIR/iptv-admin" "$PKG_DIR/iptv-gateway" "$BIN_DIR/"
  chmod +x "$BIN_DIR/iptv-api" "$BIN_DIR/iptv-worker" "$BIN_DIR/iptv-portal" "$BIN_DIR/iptv-admin" "$BIN_DIR/iptv-gateway"
  # B4：前端 hydrate 产物（包内 site/pkg/*.js）。旧包没有 site/ 时跳过，
  # 门户/后台退回 SSR-only（语言切换等 WASM 交互不可用，见 v0.1.1 教训）。
  if [ -d "$PKG_DIR/site" ]; then
    rm -rf "$BIN_DIR/site"
    cp -r "$PKG_DIR/site" "$BIN_DIR/site"
  fi
fi

# 配置
mkdir -p "$CONF_DIR" "$DATA_DIR" 2>/dev/null || sudo mkdir -p "$CONF_DIR" "$DATA_DIR"
if [ "$INSTALLED" = "0" ]; then
  PLAY_SECRET="$(rand_hex)"
  ADMIN_SECRET="$(rand_hex)"
  cat > "$CONF_DIR/.env.tmp" <<EOF
# Muse-IPTV 配置（首次安装生成，请妥善保存）
DATABASE_URL=sqlite://$DATA_DIR/iptv.db
PLAY_SECRET=$PLAY_SECRET
ADMIN_SECRET=$ADMIN_SECRET
PORTAL_BASE_URL=http://127.0.0.1:$PORT
BIND_ADDR=127.0.0.1:8080
GATEWAY_BIND_ADDR=0.0.0.0:$PORT
# B4：前端 hydrate 产物位置（portal/admin 由此找 site/pkg/*.js）。
LEPTOS_SITE_ROOT=$BIN_DIR/site
IPTV_ADMIN_USER=$ADMIN_USER
IPTV_ADMIN_PASSWORD=$ADMIN_PASS
EOF
  mv "$CONF_DIR/.env.tmp" "$CONF_DIR/.env"
  chmod 600 "$CONF_DIR/.env"
else
  # 升级：保留原 .env，只做单端口布局迁移 + 本次改的端口。
  if ! grep -q '^GATEWAY_BIND_ADDR=' "$CONF_DIR/.env"; then
    echo "GATEWAY_BIND_ADDR=0.0.0.0:$PORT" >> "$CONF_DIR/.env"
  else
    set_env_key "$CONF_DIR/.env" "GATEWAY_BIND_ADDR" "0.0.0.0:$PORT"
  fi
  if grep -q '^BIND_ADDR=0.0.0.0:' "$CONF_DIR/.env"; then
    set_env_key "$CONF_DIR/.env" "BIND_ADDR" "127.0.0.1:8080"
  fi
  if grep -q '^PORTAL_BASE_URL=http://127.0.0.1:808' "$CONF_DIR/.env"; then
    set_env_key "$CONF_DIR/.env" "PORTAL_BASE_URL" "http://127.0.0.1:$PORT"
  fi
  # B4：升级补前端产物位置（旧 .env 没有这一行时才写，不覆盖已有配置）。
  if ! grep -q '^LEPTOS_SITE_ROOT=' "$CONF_DIR/.env"; then
    echo "LEPTOS_SITE_ROOT=$BIN_DIR/site" >> "$CONF_DIR/.env"
  fi
  echo "配置文件已存在：$CONF_DIR/.env（保留原密钥与管理员密码，已按单端口布局迁移）"
fi
echo "$VERSION" > "$CONF_DIR/version"

# 服务
if [ "$NO_SERVICE" = "1" ]; then
  echo "跳过服务安装。手动启动："
  echo "  set -a; . $CONF_DIR/.env; set +a"
  echo "  $BIN_DIR/iptv-worker & $BIN_DIR/iptv-api &"
  echo "  LEPTOS_SITE_ADDR=0.0.0.0:18081 $BIN_DIR/iptv-portal & LEPTOS_SITE_ADDR=0.0.0.0:18082 $BIN_DIR/iptv-admin &"
  echo "  IPTV_PORTAL_BASE=http://127.0.0.1:18081 IPTV_ADMIN_BASE=http://127.0.0.1:18082 $BIN_DIR/iptv-gateway"
  show_summary
  echo "安装完成：Muse-IPTV $VERSION"
  exit 0
fi

if [ "$OS_ID" = "linux" ] && command -v systemctl >/dev/null 2>&1; then
  UNIT="/etc/systemd/system/muse-iptv.service"
  cat > /tmp/muse-iptv.service <<EOF
[Unit]
Description=Muse-IPTV API
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
EnvironmentFile=$CONF_DIR/.env
ExecStartPre=/bin/sh -c 'mkdir -p $DATA_DIR'
ExecStart=$BIN_DIR/iptv-api
Restart=on-failure
RestartSec=5
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
  cat > /tmp/muse-iptv-portal.service <<EOF
[Unit]
Description=Muse-IPTV Portal
After=network-online.target muse-iptv.service
Wants=network-online.target

[Service]
Type=simple
EnvironmentFile=$CONF_DIR/.env
Environment=LEPTOS_SITE_ADDR=0.0.0.0:18081
ExecStart=$BIN_DIR/iptv-portal
Restart=on-failure
RestartSec=5
[Install]
WantedBy=multi-user.target
EOF
  cat > /tmp/muse-iptv-admin.service <<EOF
[Unit]
Description=Muse-IPTV Admin
After=network-online.target muse-iptv.service
Wants=network-online.target

[Service]
Type=simple
EnvironmentFile=$CONF_DIR/.env
Environment=LEPTOS_SITE_ADDR=0.0.0.0:18082
ExecStart=$BIN_DIR/iptv-admin
Restart=on-failure
RestartSec=5
[Install]
WantedBy=multi-user.target
EOF
  cat > /tmp/muse-iptv-gateway.service <<EOF
[Unit]
Description=Muse-IPTV Gateway (single port)
After=network-online.target muse-iptv.service muse-iptv-portal.service muse-iptv-admin.service
Wants=network-online.target

[Service]
Type=simple
EnvironmentFile=$CONF_DIR/.env
Environment=IPTV_PORTAL_BASE=http://127.0.0.1:18081
Environment=IPTV_ADMIN_BASE=http://127.0.0.1:18082
ExecStart=$BIN_DIR/iptv-gateway
Restart=on-failure
RestartSec=5
[Install]
WantedBy=multi-user.target
EOF
  if [ -w /etc/systemd/system ]; then
    cp /tmp/muse-iptv.service "$UNIT"
    cp /tmp/muse-iptv-worker.service /etc/systemd/system/muse-iptv-worker.service
    cp /tmp/muse-iptv-portal.service /etc/systemd/system/muse-iptv-portal.service
    cp /tmp/muse-iptv-admin.service /etc/systemd/system/muse-iptv-admin.service
    cp /tmp/muse-iptv-gateway.service /etc/systemd/system/muse-iptv-gateway.service
    systemctl daemon-reload
    systemctl enable --now muse-iptv muse-iptv-worker muse-iptv-portal muse-iptv-admin muse-iptv-gateway || echo "警告：部分服务启动失败，见下面的健康检查。"
  else
    sudo cp /tmp/muse-iptv.service "$UNIT"
    sudo cp /tmp/muse-iptv-worker.service /etc/systemd/system/muse-iptv-worker.service
    sudo cp /tmp/muse-iptv-portal.service /etc/systemd/system/muse-iptv-portal.service
    sudo cp /tmp/muse-iptv-admin.service /etc/systemd/system/muse-iptv-admin.service
    sudo cp /tmp/muse-iptv-gateway.service /etc/systemd/system/muse-iptv-gateway.service
    sudo systemctl daemon-reload
    sudo systemctl enable --now muse-iptv muse-iptv-worker muse-iptv-portal muse-iptv-admin muse-iptv-gateway || echo "警告：部分服务启动失败，见下面的健康检查。"
  fi
  # 升级时 running 的还是旧二进制，必须重启才生效；全新安装刚启动的也是新二进制，重启无害。
  sleep 2
  restart_services
  echo "管理服务：systemctl status|restart muse-iptv muse-iptv-worker muse-iptv-portal muse-iptv-admin muse-iptv-gateway"
else
  echo "未检测到 systemd（或为 macOS），请手动启动："
  echo "  set -a; . $CONF_DIR/.env; set +a"
  echo "  $BIN_DIR/iptv-worker & $BIN_DIR/iptv-api &"
  echo "  LEPTOS_SITE_ADDR=0.0.0.0:18081 $BIN_DIR/iptv-portal & LEPTOS_SITE_ADDR=0.0.0.0:18082 $BIN_DIR/iptv-admin &"
  echo "  IPTV_PORTAL_BASE=http://127.0.0.1:18081 IPTV_ADMIN_BASE=http://127.0.0.1:18082 $BIN_DIR/iptv-gateway"
  echo "健康检查：curl http://127.0.0.1:$PORT/healthz"
fi
show_summary
if [ "$INSTALLED" = "1" ]; then echo "升级完成：Muse-IPTV $VERSION"; else echo "安装完成：Muse-IPTV $VERSION"; fi
