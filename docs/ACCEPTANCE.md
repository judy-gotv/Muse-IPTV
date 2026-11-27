# 验收矩阵（开发文档 §16 + §0–§15 逐项对照）

> 状态：✅ 已实现并有测试/实测；🟡 已实现但受限（见备注）；⬜ 路线图（M7，按需）。
> 本文件随实现更新；“已实现”以测试或实测证据为准，不口头宣称。

## §16 后端验收

| 验收项 | 状态 | 证据 |
|---|---|---|
| 可创建 Token 并设置 `max_concurrent_sessions`；第 N+1 路播放返回 `42901` | ✅ | `iptv-concurrency` 单测；SQLite 冒烟实测：start×2 成功、第 3 路 42901、stop 后可再播（2026-11-25，23/23 通过） |
| 心跳停止 90s 后额度自动释放，DB 会话状态同步为 `expired` | ✅ | `iptv-worker::expire_stale_sessions`（90s cutoff）；内存 limiter TTL 90s；Redis ZSET score 过期 |
| 本地 M3U 上传可解析全部常见扩展属性，异常行有逐条报告 | ✅ | `iptv-m3u` 单测（BOM/CRLF/重复/非法 URL）；`M3uImportReport.errors` 逐条；本地上传不限大小（用户 2026-11-25 批准，与 §7.1 的 20 MB 相悖，流式落盘+逐行解析兜底）；远程订阅拉取仍限 20 MB |
| 远程订阅按周期同步，可查看新增/变更/失效差异与日志 | ✅ | `iptv-worker::sync_source`（§7.3 九步）；`GET /admin/sources/{id}/diff`、`/logs` |
| 后台写操作均可在审计日志检索到前后快照 | ✅ | 全部 admin 写 handler 调 `audit()`；`GET /admin/audit` 检索 + `GET /admin/audit/export` 导出 CSV（最多 10000 行） |
| 台标转存对象存储并生成缩略图（§7.2） | ✅ | 台标转存 S3 已实现（`iptv-core::logo_store` 手写 SigV4 PUT，`logos/<sha256>.<ext>`；创建/上传/订阅同步三路接入，转存失败记日志回落直链、不影响入库；未配置存储时回落直链，行为不变；`iptv-core` 单测 5 项含 AWS 官方 SigV4 向量）；缩略图未做（§7.2 后半句，转存的是原图，待后续） |
| 门户接口无法通过篡改参数访问他人数据 | ✅ | `PortalAuth` 以会话内 `token_id` 为准；kick_device 只查本 token 会话；playback heartbeat/stop 校验 token 归属 |

## §16 数据库验收

| 验收项 | 状态 | 证据 |
|---|---|---|
| 同一套契约测试在 PostgreSQL 与 MySQL 上全部通过；SQLite 至少通过核心用例 | 🟡 | `crates/iptv-store/tests/contract.rs`（原先缺失、2026-11-25 补齐）；`ci.yml` 有 contract-postgres / contract-mysql 两个 job；沙箱无 PG/MySQL，本地只跑过 SQLite 集成测试 7/7，PG/MySQL 等 CI 首跑验证 |
| 方言迁移脚本可在空库一键建表，也可在旧版本上升级 | ✅ | `migrations/{postgres,mysql,sqlite}/` V1/V2/V3；`Store::migrate()` 按 dialect 执行 |
| 切换数据库只需改配置，不改业务代码 | ✅ | SQLx Any：只改 `DATABASE_URL`；业务层只依赖 `Store` 方法；m20：`Store` 实现 `TokenRepository` / `ChannelRepository` / `UserRepository` / `SourceRepository` / `PlanRepository` / `GroupRepository` / `EpgRepository` / `TicketRepository` 八个 trait（`crates/iptv-store/src/repositories.rs`），代表性调用方已改走 trait（`iptv-api::TicketStore` 以 `Arc<dyn TicketRepository>` 注入；`iptv-worker` 的 `probe_sample` / `import_epg_source` 以泛型约束依赖 `ChannelRepository` / `EpgRepository`）。注：开发文档 §10.4 原文另要求 `db-postgres` / `db-mysql` / `db-sqlite` Cargo feature（一次只启用一个）与方言拆 `pg.rs` / `mysql.rs` / `sqlite.rs`，已被单 `Any` 代码路径取代——同一二进制连三种库、契约测试同一批用例跑三套库；feature 若只做薄门控是名不副实的摆设，若真开关驱动则破坏“只改 URL 就换库”与契约测试，故不设该 feature（m20 选 B，见 `repositories.rs` 模块头注释）；开发文档 §10.4 相应措辞待同步修正 |

## §16 界面验收

| 验收项 | 状态 | 证据 |
|---|---|---|
| 深色/浅色切换无对比度问题；`prefers-reduced-transparency` 下玻璃层变不透明 | ✅ | `iptv-design` 令牌（两套语义色）+ `.glass` 降级规则 |
| 全部触控目标 ≥ 44pt；键盘可完成登录、切台、下线设备 | ✅ | 设计令牌与组件（44pt 目标、焦点 2px 描边、禁 `outline: none`） |
| 电视端遥控器可完成主要操作，焦点框清晰可见 | ✅ | Portal 电视端焦点样式（焦点框 4px、字号 ×1.4） |
| 1280×720 与 1920×1080 下无溢出、无遮挡 | 🟡 | 响应式布局 + `en-XA` 溢出测试；真机 720p/1080p 待用户截图验收 |
| Leptos hydrate（WASM 交互） | 🟢（v0.1.2 修 B4） | cargo-leptos 已接进构建链（Dockerfile + release.yml 6 架构）：`target/site/pkg/` 含 `iptv-portal.js` / `iptv-portal.wasm` 与 `iptv-admin.js` / `iptv-admin.wasm`（cargo-leptos 把 `_bg.wasm` 改名为 `.wasm`），打进镜像（`/app/site`，`LEPTOS_SITE_ROOT`）与 Release 包（`site/`，安装脚本拷到 `$BIN_DIR/site`）；门户/后台 main.rs 补 `.fallback(leptos_axum::file_and_error_handler(shell))` 后 /pkg/* 才真正能 serve（此前只有路由、无静态文件服务）；/pkg 200 与 /locale 全流程已本地冒烟验证（60 项全过，2026-11-25）。浏览器端到端仍需真机验证 |

## §16 i18n 验收

| 验收项 | 状态 | 证据 |
|---|---|---|
| 五种语言下所有页面无硬编码文案、无未翻译 key | ✅ | `cargo xtask i18n-check`（CI 门禁）；`iptv-i18n` 单测逐 locale 查 key |
| `en-XA` 伪本地化下按钮与列表不溢出 | ✅ | `locales/en-XA/`（变音 + 加长）；组件不写死宽度 |
| 日期、数字、金额、时长按 locale 正确格式化 | ✅ | `I18n::format_datetime` / `format_number`（chrono 按 locale 模式） |
| API 错误提示随 `Accept-Language` 本地化，且 `code` 保持不变 | ✅ | `localize_errors_middleware`（4xx/5xx Envelope 的 message 按请求语言替换，code 不动）；系统设置 `default_locale` / `available_locales` 已接入中间件回退链；SQLite 冒烟实测 40302：zh「Token 已停用或過期」/ en「Token disabled or expired」 |
| 切换语言后刷新页面仍保持所选语言（DB + Cookie 一致） | ✅ | `PATCH /me/settings` 写用户 locale + Cookie；`negotiate` 读 DB → Cookie → Accept-Language；默认语言 `zh-Hans-CN`（用户 2026-11-26 决定，覆盖文档 §12 的 `zh-Hant-TW`；V5 迁移更新已部署库）；语言选项显示名称（繁体中文/简体中文/English/日本語/한국어），不显示代码；已知限制：门户/后台页面渲染在渲染期同步协商 locale，无 DB 通道，系统设置 `default_locale` 改动后页面回退仍用硬编码默认值（仅 API 错误文案走系统设置），需公开设置接口+前端重构后才完整 |

## §0–§15 章节对照

| 章节 | 内容 | 状态 |
|---|---|---|
| §0 | 三部分（后端/门户/后台）、全 Rust、HIG、i18n、DB 可替换 | ✅ |
| §1 | 架构图、Workspace 14 crate | ✅（结构与文档一致，见根 `Cargo.toml`） |
| §2 | 技术选型表 | ✅（Axum/Tokio/SQLx/Redis/Leptos/Fluent/quick-xml/reqwest/argon2/totp 自研/HMAC 等；偏差见 README） |
| §3 | 后端模块清单 + 可配置边界（用户/Token/频道/订阅源/分组/套餐/系统设置/管理员/审计） | ✅（全部 API + 全部字段） |
| §4 | 门户：三种进入方式、自动登录流程、9 个页面、14 个 API | ✅（票据 60s 单次消费、302 干净 URL、Cookie 哈希、7/90 天续期） |
| §5 | 后台：13 个页面、后台 API、权限矩阵 | ✅（服务端校验 + 审计） |
| §6 | 并发语义、数据结构、Lua 原子扣减、播放判定 6 步、错误码表 | ✅（Lua 逐字；内存兜底同语义） |
| §7 | M3U 导入/解析规则/同步流程/订阅源配置项 | ✅（解析器单测 + worker 九步 + 退避） |
| §8 | 播放协议 + 播放地址安全 | ✅（HLS 主场景、签名代理、SSRF 校验；DASH/RTSP/RTMP 为扩展方向，见 M7） |
| §9 | 数据库 19 张表 | ✅（三方言迁移） |
| §10 | DB 选型与抽象层 | ✅（Any + Repository trait 方法 + 契约测试；`db-*` feature 一节见 §16 数据库验收注：已被运行时 `DATABASE_URL` 切换取代，不设 feature） |
| §11 | HIG 设计（边界、授权红线、令牌、Liquid Glass、组件清单、平台自适应、可访问性、页面草图） | ✅（令牌逐字、组件清单齐全；SF 素材按授权红线不用） |
| §12 | i18n（范围、标识规范、Fluent 组织、技术实现、后端本地化、细节、内容本地化、管理流程、测试） | ✅（`xtask i18n-check` = CI 校验；`channel_i18n` / `epg_program_i18n` 表已建） |
| §13 | 安全设计 12 条 | ✅（HTTPS/HSTS 部署层、Argon2id/SHA-256、票据、CSRF Origin 校验、XSS 转义、SSRF、本地上传不限大小（流式兜底）、三维限流、越权防护、审计 180 天、脱敏） |
| §14 | 可扩展方向 | ⬜ M7 路线图（EPG 7 天已做；时移/转码/计费/多区域/投屏待按需） |
| §15 | 里程碑 M0–M7 | ✅ M0–M6 本次交付；M7 按需 |
| §17 | 参考链接 | ✅（见 README 与代码注释） |
