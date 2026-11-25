# Muse-IPTV — 全 Rust IPTV 播放平台

> 按开发文档 v1.2（`~/workspace/user/files/IPTV____________Rust.md`）逐项实现。
> 作者：Passwd Word

后端 Axum + 前端 Leptos SSR + WASM hydration，共享同一套 DTO；
Token 自动登录门户、按 Token 的并发控制（Redis Lua）、M3U/远程订阅同步、
XMLTV EPG、后台 RBAC 与审计、五种语言 i18n、Apple HIG 级设计系统。

## 一键安装

```sh
# Linux / macOS（自动识别架构，校验 SHA-256，装 systemd 服务）
curl -fsSL https://raw.githubusercontent.com/judy-gotv/Muse-IPTV/main/install.sh | sh
```

```powershell
# Windows（管理员 PowerShell）
irm https://raw.githubusercontent.com/judy-gotv/Muse-IPTV/main/install.ps1 | iex
```

安装脚本会打印**只显示一次**的管理员账号密码，请妥善保存。
或从 [Releases](https://github.com/judy-gotv/Muse-IPTV/releases) 下载对应架构包。

## Docker 部署

镜像 `ghcr.io/judy-gotv/muse-iptv:latest`（amd64/arm64），一个容器跑全部四个服务：
API :8080、门户 :8081、后台 :8082，数据在 `/app/data`（默认 SQLite）。

```sh
docker pull ghcr.io/judy-gotv/muse-iptv:latest

# 首次启动必须设 IPTV_ADMIN_PASSWORD，否则建不出管理员账号
docker run -d \
  --name muse-iptv \
  --restart unless-stopped \
  -p 8080:8080 -p 8081:8081 -p 8082:8082 \
  -v muse-iptv-data:/app/data \
  -e IPTV_ADMIN_USER=admin \
  -e IPTV_ADMIN_PASSWORD=改成你的密码 \
  ghcr.io/judy-gotv/muse-iptv:latest
```

- 门户：`http://服务器IP:8081`（票据登录 / 粘贴 Token 登录）
- 后台：`http://服务器IP:8082`（管理员登录）
- 健康检查：`curl http://127.0.0.1:8080/healthz`
- 看日志：`docker logs -f muse-iptv`；停掉：`docker stop muse-iptv`
- 升级：`docker pull ghcr.io/judy-gotv/muse-iptv:latest` 后 `docker rm -f muse-iptv`，再按上面命令重跑（数据在 volume 里，不丢）。

### Docker Compose

```sh
cp .env.example .env   # 改 IPTV_ADMIN_PASSWORD
docker compose -f deploy/docker-compose.yml up -d
```

### 环境变量

| 变量 | 默认 | 说明 |
|---|---|---|
| `IPTV_ADMIN_USER` / `IPTV_ADMIN_PASSWORD` | admin / 空 | 首次启动建超管；密码为空建不出账号 |
| `DATABASE_URL` | `sqlite:///app/data/iptv.db` | 改 PostgreSQL/MySQL 见 `.env.example` |
| `PLAY_SECRET` / `ADMIN_SECRET` | 为空则随机生成 | 不固定则重启后旧播放链接与会话失效 |
| `PORTAL_BASE_URL` | `http://127.0.0.1:8081` | 票据链接用的门户地址；远程访问改成服务器 IP/域名 |
| `IPTV_PORT` / `IPTV_PORTAL_PORT` / `IPTV_ADMIN_PORT` | 8080 / 8081 / 8082 | compose 映射到宿主的端口 |

注意：`:v0.1.0` 镜像的入口脚本没给门户/后台定端口（两个服务会抢 `127.0.0.1:3000`），请用 `:latest`。

## 快速开始（源码）

```sh
cp .env.example .env   # 改 IPTV_ADMIN_PASSWORD
cargo run -p iptv-api      # 后端 :8080（默认 SQLite ./data/iptv.db）
cargo run -p iptv-worker   # 同步/探测/清理
LEPTOS_SITE_ADDR=0.0.0.0:8081 cargo run -p iptv-portal   # 门户 :8081
LEPTOS_SITE_ADDR=0.0.0.0:8082 cargo run -p iptv-admin    # 后台 :8082
```

- 健康检查：`curl http://127.0.0.1:8080/healthz`
- OpenAPI：`http://127.0.0.1:8080/api/v1/openapi.json`
- 指标：`http://127.0.0.1:8080/metrics`

## 架构（文档 §1）

```text
反向代理 / TLS（Rustls）
  ├─ 用户端门户 Leptos SSR+WASM  /t/{ticket}        → :8081
  ├─ 后端 API 服务 Axum（鉴权/并发/频道/播放）      → :8080
  └─ 管理后台 Leptos SSR+WASM  /admin/*             → :8082
        │                │
        ▼                ▼
  关系型数据库 SQLx Any       Redis（会话/并发/限流）
  （PG / MySQL / SQLite）     （不配则内存兜底，同语义）
        │                │
  ClickHouse / ScyllaDB（可选分析库）  Worker（同步/EPG/探测）
```

Workspace 14 个 crate（文档 §1.1）：`iptv-core` / `iptv-dto` / `iptv-store` /
`iptv-m3u` / `iptv-epg` / `iptv-auth` / `iptv-concurrency` / `iptv-i18n` /
`iptv-design` / `iptv-api` / `iptv-worker` / `iptv-ui` / `iptv-portal` / `iptv-admin`，
另有 `xtask`（`cargo xtask i18n-check`）。

## 关键语义

- **Token 登录**（§4）：长期 Token `tk_live_…` 只存 SHA-256，明文只展示一次；
  门户默认用一次性票据 `/t/{ticket}`（60s、单次消费、可绑 IP 段），兑换成功 302 到
  干净 URL；会话 Cookie `iptv_portal_sid`（256 bit，DB 只存哈希），7 天滑动续期，
  记住设备 90 天。
- **并发**（§6）：`max_concurrent_sessions` = 同时播放路数，第 N+1 路返 `42901`；
  `max_bound_devices` = 绑定设备总数；Redis Lua 原子扣减（§6.3 脚本逐字落地），
  无 Redis 时内存实现跑同一语义；心跳 30s，90s 未心跳自动释放，DB 会话同步为 `expired`。
- **播放地址**（§8）：`play_url` 带 `exp` / `session` / `sig`（HMAC-SHA256，300s），
  上游真实地址不下发；`/p/{id}.m3u8` 与 `/p/seg` 做签名代理，SSRF 校验目标 host。
- **订阅同步**（§7）：BOM/CRLF 容错、逐条报错、内容哈希比对、差异分类
  （新增/元数据变更/URL 变更/消失）、消失按 `disable`（默认）或 `delete`、
  失败退避 1m → 5m → 30m → 2h。
- **后台**（§5）：Argon2id + TOTP 登录；角色超管/运营/客服/只读，权限矩阵在服务端
  校验；全部写操作写审计（含前后快照，保留 180 天）。
- **i18n**（§12）：`zh-Hant-TW`（默认）/ `zh-Hans-CN` / `en` / `ja` / `ko`，
  另有伪本地化 `en-XA`；判定顺序 用户设置 → Cookie → Accept-Language → 默认；
  错误 `message` 本地化、`code` 永远不变。
- **数据库**（§10）：SQLx Any，同一套代码跑 PostgreSQL / MySQL / SQLite，
  只改 `DATABASE_URL`；迁移按 `migrations/{postgres,mysql,sqlite}/` 分目录；
  契约测试在 CI 对 PG 与 MySQL 各跑一遍（§16）。

## 与文档的偏差（诚实说明，不假装 1:1）

| 项 | 文档要求 | 本实现 | 原因 |
|---|---|---|---|
| 日期/数字格式化 | `icu4x`（`icu_datetime`/`icu_decimal`） | `chrono` 按 locale 的惯用格式 + Fluent 复数规则 | 减依赖、保编译；格式覆盖 §12.6 的日期/数字/千分位，时区按用户设置 |
| 校验 | `validator` crate | 手写校验（DTO 层） | 同上；规则与文档一致（必填、并发 ≥1、同步周期 ≥300s 等） |
| 指标 | `metrics` + Prometheus 导出器 | 手写 `/metrics` 文本（Prometheus 格式，AtomicU64 计数器） | 同上；格式兼容 Prometheus 抓取 |
| 定时任务 | `tokio-cron-scheduler` | `tokio::time::interval`（15s 扫一轮）+ DB 认领 | 同上；语义一致（多实例安全） |
| 分析库 | ClickHouse / ScyllaDB | `playback_event` 表为默认事件库；ClickHouse/Scylla 未接 | 旁路库，不影响主库选择（§10.3）；事件表结构已按月分区友好设计 |
| Tauri 壳 | Tauri 2 跨端壳 | `deploy/tauri-shell/` 只含配置与入口（需 Tauri CLI 构建） | 壳本身是部署形态，不在主 CI 编译；门户/后台同一份 WASM 前端可直接装壳 |
| Leptos hydrate | SSR + hydration | SSR 已实现并可独立运行；WASM hydrate 需 `cargo leptos build`（wasm target）生成 `pkg/` | 主 CI 只验 SSR；hydrate 在 `ci.yml` 外单独验证，见 `docs/ACCEPTANCE.md` |
| SF 字体/图标 | SF Pro / SF Symbols | 系统字体栈 + Inter/Noto 回退；Lucide 风格开源图标 | §11.2 授权红线：SF 字体与符号不得跨端打包 |

## SQLite 冒烟（2026-11-25，本地实跑，23/23 通过）

`target/debug/iptv-api` + SQLite 实库，脚本见 `/tmp/smoke.py`（CI 的 SQLite
冒烟另见 `ci.yml`）：healthz、超管登录、建套餐/用户/Token（并发=2）、建频道、
M3U 上传（1 条入库、坏行逐条报错）、票据建→302 兑换→二次兑换 40101、粘贴
Token 登录、我的频道、播放 start×2 成功、第 3 路 42901、heartbeat/stop 释放
后可再播、审计有记录、错误 code 与语言无关且 message 随 Accept-Language 变
（40302：zh「Token 已停用或過期」/ en「Token disabled or expired」）。

冒烟修掉的真 bug（均已回归通过，不遮掩）：

- SQLite 首次启动打不开库：sqlx 默认 `create_if_missing=false`，`Store::connect`
  对无 mode 参数的 sqlite URL 补 `?mode=rwc`。
- axum 0.8 不允许 `/p/{channel_id}.m3u8`（一段内参数+后缀混写）：路由改
  `/p/{*rest}`，handler 内拆 id 与后缀。
- 管理员 Cookie 写了 `Path=/admin`，浏览器不发给 `/api/v1/admin/*`：改 `Path=/`。
- `list_channels_for_token` 把空 `allowed_groups` 当成“零频道”，与
  `authorize_channel`（空 = 全部组）矛盾：改为一致，store 测试同步改断言。
- `from_fn` 中间件拿不到 `with_state` 的 state：metrics 计数长期是 0，且错误
  本地化中间件不生效；两者改 `from_fn_with_state`。
- CI 引用的 `cargo test -p iptv-store --test contract` 目标不存在：新增
  `crates/iptv-store/tests/contract.rs`（无 DATABASE_URL 时跳过）。

其余 §0–§16 的逐项验收见 [docs/ACCEPTANCE.md](docs/ACCEPTANCE.md)。

## 常用命令

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p xtask -- i18n-check   # 语言包 key 齐全性 + 占位符一致性
```

## 发布

打 tag 即发版：`git tag v0.1.0 && git push origin v0.1.0` → Release 工作流打出
6 个架构包（linux x86_64/aarch64/armv7、macOS x86_64/arm64、windows x86_64）
+ SHA256SUMS；Docker 工作流推多架构镜像 `ghcr.io/judy-gotv/muse-iptv:<tag>`（amd64/arm64）。

## 安全

- 全站 HTTPS + HSTS（生产用 Rustls 终结，见部署文档）；Token 只存 SHA-256；
  管理员密码 Argon2id；会话 Cookie HttpOnly + SameSite=Lax。
- SSRF：订阅仅 http/https，目标私网 IP 拒绝；上传限 20 MB；按 IP/Token/账号三维限流。
- 仅接入有授权的源，提供下架与黑名单机制，日志脱敏（§13）。
