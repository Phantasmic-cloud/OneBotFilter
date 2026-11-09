# AGENTS.md — OneBotFilter AI 代理开发指南

这份文件是本仓库唯一的代理开发说明，给任何在仓库里干活的 AI 助手（Codex、Claude、
Copilot 等）看，也是接手本项目的第一份材料。如果仓库里还有 `CLAUDE.md` 之类的文件，
它们只指向这里；要改规则就改这份，不要往别处复制一份。

本文件用中文书写。仓库里所有示例值（账号、地址、token）都是占位，不包含任何真实
账号信息；提交前请确认没有把真实配置带进来。

## 项目概述

OneBotFilter 是 OneBot v11 的 WebSocket 消息过滤网关。它同时扮演两个角色：

- 对**上游 OneBot 实现**是**客户端**：OneBot 实现（NapCat、Lagrange、LLOneBot 等）
  主动连 `ws://<host>:<port><suffix>`，本程序只接受一个连接；
- 对**下游 bot 应用**是**服务端**：本程序按 `bot-apps[].uri` 主动连出去（反向 WebSocket，
  与 NoneBot 等框架的 reverse-ws 接入方式一致）。

群消息从上游进来后，按「群开关 → 黑用户 → bot 群名单 → 内容过滤」逐层过闸，再分发给
各 bot 应用；bot 的发言类 API 在出去前可以按正则改写文本，并把改写痕迹从回包里抹掉。

纯 Rust、无 CGO（TLS 用 rustls），产物是单文件二进制；运行时状态落盘到 `db.json`，
配置改动可热加载。

## 仓库结构

```
Cargo.toml / Cargo.lock   依赖与锁定版本（二进制项目，Cargo.lock 必须提交）
src/
  main.rs                 入口：建 tokio runtime，调用 lib::run("config.yaml")
  lib.rs                  run()：加载配置/状态、监听上游、起各 bot 客户端、配置热更
  global.rs               常量、App 共享状态（配置/连接/统计/过滤器注册）、读写锁封装
  config.rs               配置结构（serde）、默认值、校验、notify 热更
  onebot_message.rs       事件宽松解析、@我判断、指令文本提取、CQ at 处理
  stats.rs                发言类 API 判定
  store.rs                db.json 运行时状态：群开关、黑用户、bot 名单、群名缓存
  api.rs                  echo 关联的同步 OneBot API、群名/群列表缓存
  filters.rs              内容过滤：mode、prefix 放行与替换、fancy-regex 正则
  rewrite.rs              出站文本改写、echo/message_id/指纹记录、回包正文抹除、自上报识别
  command.rs              内置指令（/enable /disable /bu /bot /group /<bot> on|off）与状态文案
  dispatch.rs             入站总入口：API 回包分流、自身上报丢弃、指令、黑用户、群开关、按 bot 分发
  server.rs               上游 OneBot 连接（WS 服务端，只允许一个），读循环与分发
  client.rs               下游 bot 连接（WS 客户端）、重连、ping 延迟探测、连接统计
tests/
  gateway.rs              进程内端到端集成测试（mock 上游 + mock bot）
  README.md               测试说明
config-example.yaml       配置示例（只含占位值）
scripts/build.sh          跨平台 release 构建（产物 dist/）
docs/README_zh.md         面向使用者的中文说明
.github/workflows/build.yml  CI：fmt/clippy/test + 三平台构建；打 tag 时发 Release
AGENTS.md                 本文件
```

## 核心概念

### 两条连接、两个方向

| 连接 | 谁发起 | 代码 | 说明 |
| --- | --- | --- | --- |
| 上游 OneBot 实现 | 对方连本程序 | `server.rs` | 只有一个，第二个连接直接 403「只能连接一个OneBot客户端」 |
| 下游 bot 应用 | 本程序连对方 | `client.rs` | 每个 `bot-apps[]` 一条，断线按 `sleep-time` / 5s 重连 |

连 bot 应用时带的头：

```text
x-self-id: <server.bot-id>
authorization: Bearer <bot-apps[].access-token>
user-agent: <server.user-agent>
x-client-role: Universal
```

断线重连语义是稳定行为约定：拨号失败等 `server.sleep-time` 秒再试；已经建连后读失败，
固定等 5s 再重连。连接成功时复位该 bot 的流量计数；断开时标记 `DisconnectedAt` 并复位。

### 入站数据流

`server.rs` 的读循环把帧交给 `dispatch::dispatch_incoming(app, is_text, raw)`，分四种情况：

1. 非文本帧 → 原样广播给所有 bot；
2. 无 `post_type` 的 API 回包 → 本程序自己发起的（echo 以 `obf_` 开头）由
   `api::try_deliver_api_response` 吃掉，不进 bot；其余广播给 bot，若属于被改写的发言，
   正文会被 `rewrite::sanitize_rewritten_api_response` 剥掉；
3. 改写后的自身上报（`message_sent`，或 self_id 等于自己的 message）→ 丢弃；
4. 能解析的文本事件 → 有 `group_id` 的 `tokio::spawn(handle_group_event)`；
   **无 `group_id` 的 message / notice / request（含私聊）一律丢弃**。

`handle_group_event` 的判定顺序是硬性的，不要调换：

```text
内置指令 → 黑用户 → 群是否启用 → 该 bot 的群名单 → 该 bot 的内容过滤 → 写帧
```

### 内容过滤语义

`bot-apps[].message`（以及覆盖它的 `private-message` / `group-message`）里的 `mode`：

| 值 | 行为 |
| --- | --- |
| `on` | 全放行 |
| `off` | 全拦截 |
| `whitelist` | 命中 `filters` 正则或带 `prefix` 才放行，否则拦截 |
| `blacklist` | 命中 `filters` 正则或带 `prefix` 则拦截，否则放行 |

- `prefix` 命中的消息除了放行，还会把前缀替换成 `prefix-replace` 后再交给 bot
  （`MessageFilter::prefix_pass`），同时改 `message` 与 `raw_message`，替换第一处。
- `private-message` / `group-message` 的 `mode` 可以为 `default`，表示继承 `message`。
  私聊事件目前不会进到这一步（无群号直接丢弃），这两项是预留结构。
- 正则用 `fancy-regex`：兼容 lookahead/lookbehind 等常见扩展语法；`rewrite-text` 与
  `filters` 里的规则都按它的语法写，改规则时注意测试。

### bot 群名单

`bot-apps[].group-id.mode` 与 `db.json` 里 `bots[<name>].ids` 配合：

- `whitelist`：`ids` 里的群才收该 bot 的消息；
- `blacklist`：`ids` 里的群不收，其余都收。

改名单的入口是群指令 `/<bot> on|off`（写进 `store.rs` 的 `set_bot_group`），不要绕过它直接改 `db.json`。

### 出站改写与回包抹除

bot → 上游的「发言类」API（`send_msg`、`send_group_msg`、`send_private_msg`、
`send_group_forward_msg` 等，判定见 `stats.rs` 的 `is_outbound_send_action`）会先过
`Filter::rewrite_outbound`，按 `rewrite-text` 规则顺序做正则替换，`message` 字段可能是
字符串也可能是消息段数组（只改 `text` 段）。

改写之后必须有配套动作，否则 bot 会看到改过的文本：

- 记录 echo 与正文指纹（`remember_rewrite`）；
- 对应的 API 回包到达时，按 echo / `message_id` / 正文指纹匹配
  （`sanitize_rewritten_api_response`），把 `data` 里的正文剥掉、只留 `message_id`；
- bot 自己上报的 `message_sent` 按同一套记录丢弃（`is_rewritten_self_report`）。

记录有效期 `REWRITE_TTL = 90s`，读写时顺带清理过期项。给新的发言类 action 加改写时，
三件事要一起改：`is_outbound_send_action`、`rewrite_outbound` 的调用点、回包抹除的匹配路径。

### 内置指令

只在**群消息**里生效，且要求 `server.command-prefix` 前缀。权限不足、群未启用、
指令不存在时**静默返回**，不往群里发任何东西。

| 指令 | 权限 | 说明 |
| --- | --- | --- |
| `@bot /enable [群号]` | superuser | 启用某群 BOT 服务，缺省当前群 |
| `@bot /disable [群号]` | superuser | 禁用某群 BOT 服务 |
| `/bu add\|del <qq>` | superuser | 全局黑用户增减 |
| `/bu list` | superuser | 黑用户列表 |
| `/bot` | 无 | 本群已开/已关的 bot |
| `/bot status` | 无 | 连接状态、延迟、收发计数 |
| `/bot <name\|alias>` | 无 | 单个 bot 详情 |
| `/group status` | superuser | 已启用/已禁用的群 |
| `/group <name\|alias>` | superuser | 某 bot 在各群的开关明细 |
| `/<name\|alias> on\|off` | superuser | 当前群开/关某个 bot |

注意三个容易踩的点：

- `/enable`、`/disable` 必须 @bot 才被当作指令，没 @ 会原样放行给 bot；
- 指令处理可能调用 OneBot API（比如解析群名），所以必须在独立 task 里跑
  （`dispatch::handle_group_event` 已经是 spawn 出去的）：在读循环里同步等 API 回包会死锁；
- 文案与格式（`✔◉ / ✔◎ / ✔⊗ / ✘⟳ / ¿⟳`、uptime、延迟、`%Y-%-m-%-d %H:%M:%S`）必须
  是稳定输出格式，改文案算行为变更。

### 运行时状态（db.json）

```json
{
  "enabled_groups": [],
  "black_users": [],
  "bots": { "<name>": { "ids": [] } },
  "group_names": {},
  "joined_groups": []
}
```

- 全部读写走 `store.rs` 的 `Store` 方法，内部有 `parking_lot::RwLock`；落盘用
  「写临时文件再 rename」，不要自己 `fs::write` 直写 `db.json`。
- `group_names` / `joined_groups` 是缓存，靠 `get_group_list` 刷新；`resolve_group_name`
  先查 API（`get_group_info`），失败或超时才退回缓存。
- 配置热更后会 `sync_bots_from_config`，只补新增的 bot，不删除已有条目。

### 配置与热更新

- `serde_yaml_ng` 解析；`notify` 监听 `config.yaml`（200ms 去抖）；校验失败保留旧配置、
  只打日志；校验通过后依次：同步 bot 名单 → 替换 `App.config` → 重编译已存在的过滤器。
- `Config::check` 的约束：`server.host/port/bot-id/user-agent` 必填；`bot-apps[].name`
  不能带空格、不能占用内置词（`status/enable/disable/bu/bot/group/on/off`）；
  `name` 与所有 `alias` 在整个配置内大小写不敏感互斥；`group-id.mode` 只能是
  `whitelist` / `blacklist`；`message.mode` 只能是 `on/off/whitelist/blacklist`；
  `rewrite-text.original` 必须能编译。
- 热更只重新 `compile` **已存在**的过滤器（按名字匹配），新增或删除 bot 应用需要重启进程。

### OneBot API 调用

`api.rs` 的 `call_onebot_api` 用 `echo` 关联请求与回包，默认超时 3 秒，回包经 oneshot
投递。目前用到的 action：

| action | 用途 | 调用点 |
| --- | --- | --- |
| `get_login_info` | 测上游 RTT | `measure_onebot_latency` |
| `get_group_info` | 解析群名 | `resolve_group_name` |
| `get_group_list` | 刷新已加入群与群名缓存 | `refresh_joined_groups` |
| `send_group_msg` | 回复内置指令 | `command::reply_group_text` |

回包统一结构 `{status, retcode, data, message, wording}`；`retcode != 0 && status != "ok"`
视为失败。本程序发起的回包带 `obf_` 前缀，只由 `try_deliver_api_response` 消费，
不会被广播给 bot。

### 并发模型

- tokio 多线程 runtime；每个 bot 一条长生命周期 task（`ws_client_handler`），
  bot 的读/写/ping 各一条子 task，断开时 abort 并清理。
- 共享状态都收在 `App`（`global.rs`）：`config: ArcSwap<Config>`、`store: Arc<Store>`、
  `filters: RwLock<Vec<Arc<RwLock<Filter>>>>`、`clients: RwLock<Vec<Arc<Client>>>`、
  `upstream: RwLock<UpstreamConn>`、统计计数用 `AtomicU64`、改写记录用 `Mutex<RewriteTracker>`。
- 写 WebSocket 一律经 channel 串行：上游 `mpsc::UnboundedSender<UpstreamWrite>`，
  下游 `mpsc::UnboundedSender<ClientWrite>`；写成功后再按标记计数。
- 持锁时不要做网络 IO，也不要在 `parking_lot` guard 生命周期内 `.await`（会阻塞 runtime）；
  需要 await 的逻辑先取快照再放锁。
- 指令处理可能调 API，必须在独立 task 里跑，不能在读循环里同步等回包。

## 编码规范

### 语言与框架

- Rust 2021 edition，`rust-version = "1.85"`；只用 `tokio`、`tokio-tungstenite`（rustls）、
  `serde` / `serde_json` / `serde_yaml_ng`、`fancy-regex`、`notify`、`parking_lot`、
  `arc-swap`、`chrono`、`log`、`anyhow` 这些依赖，不要引入需要 C 工具链的 TLS/加密库，
  保证三个平台都能原生构建。
- 源码按职责拆分模块，改逻辑时先看对应模块，不要跨模块复制实现。

### 错误处理

- 启动阶段用 `anyhow::Result`，配置/状态加载失败直接退出（`main.rs` 打日志后 exit(1)）。
- 运行时路径不 `unwrap` / 不 `panic`：连接错误写日志并清理，消息解析失败就丢弃或原样转发。
- 错误日志写清楚是哪个 bot、哪个群、哪个 action，文案保持稳定。

### 命名与风格

- 模块/文件 `snake_case`，类型 `PascalCase`，函数 `snake_case`，与现有代码保持一致。
- 注释解释「为什么」和「不变量」，不写废话；同一段逻辑的注释保持中文。
- 提交前必须 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings` 干净。

### 日志

- 用 `log` 宏；`logging.rs` 把格式固定成 `YYYY/MM/DD HH:MM:SS message`（写 stderr），
  便于与线上日志对照。
- `server.debug` 只控制「每条消息的判定结果」这类高频日志，连接建立/断开、错误无条件打。
- 日志里可以出现群号/账号用于排查，但**不要**打 access-token、完整消息正文。

## 测试

- `cargo test`：单测在各模块 `#[cfg(test)] mod tests`；集成测试在 `tests/gateway.rs`
  （进程内起 mock OneBot 上游 + mock bot 下游），覆盖唯一上游限制、群开关、`/enable`、
  内容过滤、黑用户、出站改写与回包抹除。
- 需要真实 OneBot 实现与 bot 应用的联调不在 CI 里做：本地起一个空 `bot-apps`
  的配置确认能监听，再接入真实环境看日志。
- CI（`.github/workflows/build.yml`）在 push / PR 上跑 `cargo fmt --check`、
  `cargo clippy --all-targets -- -D warnings`、`cargo test`，并交叉检查三个平台构建。

## 构建

```bash
# 当前主机（release）
cargo build --release
# 或
sh scripts/build.sh

# 指定目标（需要对应目标工具链；CI 用原生 runner）
sh scripts/build.sh x86_64-unknown-linux-gnu
sh scripts/build.sh x86_64-pc-windows-msvc
sh scripts/build.sh aarch64-apple-darwin
```

产物为 `target/<triple>/release/onebotfilter[.exe]`（`scripts/build.sh` 会复制到 `dist/`）。
release profile 开了 `lto = "thin"`、`codegen-units = 1`、`strip = true`、`panic = "abort"`。

## 常见任务

### 新增一个配置字段

1. 在 `src/config.rs` 对应结构体里加字段（`serde(rename_all = "kebab-case")` + `default`）；
2. 需要校验就在 `Check` 里补一条，error 文案要能看出是哪个 bot 的哪一项；
3. 更新 `config-example.yaml`，只写占位值；
4. 同步更新 `docs/README_zh.md` 的配置表；
5. 如果新字段影响热更，确认 `config::reload` 路径能正确处理。

### 新增一个内置指令

1. 在 `src/command.rs` 的 `try_handle_command` 里识别指令形态（注意大小写、别名、
   是否要求 @bot）；
2. 在 `needs_super` 里决定是否需要 superuser；
3. 写处理函数，通过 `reply_group_text` 回复，不要自己拼 WebSocket 帧；
4. 更新 `docs/README_zh.md` 与 `AGENTS.md` 的指令表。

### 修改过滤逻辑

- 入口是 `src/dispatch.rs` 的 `handle_group_event`；先想清楚改的是「群级闸门」还是
  「bot 级闸门」，不要破坏现有顺序（指令永远最先、黑用户其次）。
- 只影响消息内容的判定放 `filters.rs`，不要写进 `dispatch.rs`。
- 改完确认：未启用的群仍然不转发；黑用户仍然优先于一切 bot 分发。

### 修改出站改写

见「出站改写与回包抹除」。核心约束：**改了出站正文，就必须能把对应回包的正文抹掉**，
否则 bot 会读到改写后的文本。

## Git 提交

提交信息格式：

```text
[Type] 简短描述
```

| Type | 用途 |
| --- | --- |
| `[Feat]` | 新功能 |
| `[Fix]` | 修复 |
| `[Chore]` | 维护、重构、依赖或构建变更 |
| `[Docs]` | 仅文档 |

规则：

- 描述用祈使句、首字母大写、结尾不加句号，长度尽量不超过 70 个字符；
- 一个提交只做一件事；
- 提交信息里不得出现真实账号、群号、token、内部服务名或本机路径；
- 需要标注 AI 协助时，用标准 `Co-authored-by:` trailer（放在正文，与标题隔一个空行），
  不要自创 `Agent:` 之类的行。

示例：

```text
[Feat] Add per-group bot toggles
[Fix] Drop rewritten self reports
[Chore] Bump websocket dependency
```

## GitHub Actions

`.github/workflows/build.yml`：

- `test`：ubuntu 上跑 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、
  `cargo test`；
- `build`：三平台矩阵，用原生 runner 构建：`ubuntu-latest`（linux amd64）、
  `windows-latest`（windows amd64）、`macos-14`（macos arm64），产物命名为
  `onebotfilter-linux-amd64` / `onebotfilter-windows-amd64.exe` / `onebotfilter-macos-arm64`，
  上传 artifact；
- `release`：仅 `v*` tag，汇总三个 artifact + `config-example.yaml`，生成
  `SHA256SUMS-<tag>.txt`，用 `softprops/action-gh-release` 发布。

工作流改动保持最小权限（默认 `contents: read`，只有 Release job 需要 `contents: write`）；
第三方 action 优先用官方版本标签，需要长期钉死时再换成完整的 commit SHA。

## 发布说明

- 版本号用 `vX.Y.Z` 的 tag；
- release notes 只写用户关心的事：新增功能、行为变化、修复；
- 不要提内部基础设施（内网地址、服务器名、私有部署细节），也不要带真实账号信息；
- 预发布（`-alpha` / `-beta` / `-rc`）勾选 pre-release，正式版不勾。
