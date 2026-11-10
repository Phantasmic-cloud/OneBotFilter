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

群聊和私聊各有一套“全局开关 → 全局黑用户 → per-bot 名单 → 内容过滤”的链路，再分发给
各 bot 应用；bot 的发言类 API 在出去前可以按正则改写文本，并把改写痕迹从回包里抹掉。

纯 Rust、无 CGO（TLS 用 rustls），产物是单文件二进制；运行时状态落盘到 `db.json`，
配置改动可热加载，无用的 bot/群状态会自动清理。

## 仓库结构

```
Cargo.toml / Cargo.lock   依赖与锁定版本（二进制项目，Cargo.lock 必须提交）
src/
  main.rs                 入口：建 tokio runtime，调用 lib::run("config.yaml")
  lib.rs                  run()：加载配置/状态、监听上游、起各 bot 客户端、配置热更
  global.rs               常量、App 共享状态、连接/统计/过滤器注册、上游唯一连接位
  config.rs               配置结构（mode.group/private、message.group/private）、校验、旧字段检测、热更
  onebot_message.rs       事件宽松解析、@我判断、指令文本提取、CQ at 处理
  message.rs              统一消息段 Segment / 出站 OutboundMessage（字符串或数组）
  onebot_api.rs           统一 OneBot API：typed action、echo 关联、回包解析、发送类判定
  gate.rs                 群/私聊统一闸门：ChatKind、全局开关、黑用户、per-bot 名单
  store.rs                db.json 运行时状态：全局开关、双名单、黑用户、群名缓存、清理
  api.rs                  高层 API：群名解析、群列表刷新、延迟测量
  filters.rs              内容过滤：mode、prefix 放行与替换、fancy-regex 正则
  rewrite.rs              出站文本改写、echo/message_id/指纹记录、回包正文抹除、自上报识别
  command.rs              内置指令（含 private 参数形态）与状态文案
  dispatch.rs             入站总入口：API 回包分流、自身上报丢弃、群/私聊投递
  server.rs               上游 OneBot 连接（WS 服务端，只允许一个，CAS 占位）
  client.rs               下游 bot 连接（WS 客户端）、重连、ping 延迟探测、连接统计
tests/
  gateway.rs              进程内端到端集成测试（mock 上游 + mock bot）
  README.md               测试说明
config-example.yaml       配置示例（只含占位值）
scripts/build.sh          跨平台 release 构建（产物 dist/）
docs/README_zh.md         面向使用者的中文说明
docs/API.md               协议、配置、db 与内部 API 参考
.github/workflows/build.yml  CI：fmt/clippy/test + 三平台构建；打 tag 时发 Release
AGENTS.md                 本文件
```

## 核心概念

### 两条连接、两个方向

| 连接 | 谁发起 | 代码 | 说明 |
| --- | --- | --- | --- |
| 上游 OneBot 实现 | 对方连本程序 | `server.rs` | 只有一个；第二个连接直接 403「只能连接一个OneBot客户端」 |
| 下游 bot 应用 | 本程序连对方 | `client.rs` | 每个 `bot-apps[]` 一条，断线按 `sleep-time` / 5s 重连 |

连 bot 应用时带的头：

```text
x-self-id: <server.bot-id>
authorization: Bearer <bot-apps[].access-token>
user-agent: <server.user-agent>
x-client-role: Universal
```

断线重连语义是稳定约定：拨号失败等 `server.sleep-time` 秒再试；已经建连后读失败，
固定等 5s 再重连。连接成功时复位该 bot 的流量计数；断开时标记 `DisconnectedAt` 并复位。

### 入站数据流

`server.rs` 的读循环把帧交给 `dispatch::dispatch_incoming(app, is_text, raw)`：

1. 非文本帧 → 原样广播给所有 bot；
2. 无 `post_type` 的 API 回包 → 本程序自己发起的（echo 以 `obf_` 开头）由
   `api::try_deliver_api_response` 吃掉；其余广播给 bot，若属于被改写的发言，
   正文会被 `rewrite::sanitize_rewritten_api_response` 剥掉；
3. 改写后的自身上报（`message_sent`，或 self_id 等于自己的 message）→ 丢弃；
4. `gate::classify` 认定为群/私聊 message 的：
   - 群消息：spawn `handle_group_event`（先处理内置指令，再走闸门）；
   - 私聊消息：直接 `handle_private_event`（无指令、不调 API）；
5. 其它无群号事件（notice/request 等）一律丢弃；meta_event 等原样广播。

### 群 / 私聊统一闸门（`gate.rs`）

```text
群消息：
  内置指令（enable/disable 及其 private 形态不受群开关限制）
  → 群是否启用（enabled_groups）
  → 发送者是否全局黑用户（black_users）
  → 每个 bot：mode.group 群名单 → message.group 内容过滤 → 转发

私聊消息：
  发送者是否在全局私聊白名单（enabled_private_users）
  → 发送者是否全局黑用户
  → 每个 bot：mode.private 私聊名单 → message.private 内容过滤 → 转发
```

`ChatKind::{Group, Private}`、`global_gate`、`bot_list_gate` 由两种会话共用；
`dispatch::deliver_to_bots` 是唯一的转发循环，不要再写第二份。

### 名单与开关

- **全局开关**：
  - `enabled_groups`：群是否 `/enable`（等价于“这个群允许用 BOT”）；
  - `enabled_private_users`：用户是否 `/enable <qq> private`（私聊全局白名单，空=私聊全关）。
- **per-bot 名单**（`db.json` 的 `bots[name].groups` / `users`）：
  - `mode.group` = `whitelist` → 群在列表内才收；`blacklist` → 群在列表内拒收；
  - `mode.private` = `whitelist` → 用户在列表内才收私聊；`blacklist` → 用户在列表内拒收。
- **内容过滤**：`message.group` / `message.private` 各一份，`mode` 为
  `on` / `off` / `whitelist` / `blacklist`，`prefix` 命中的消息会按 `prefix-replace` 改写前缀。
- 修改名单只走 `Store` 方法（`set_bot_group` / `set_bot_private_user`），不要直接改 db.json。

### 内置指令

只在**群消息**里生效，且要求 `server.command-prefix` 前缀。权限不足、群未启用、
指令不存在时**静默返回**。私聊相关操作不新增顶层指令，而是在原指令后加 `private` 参数。

| 指令 | 权限 | 说明 |
| --- | --- | --- |
| `@bot /enable [群号]` / `@bot /disable [群号]` | superuser | 全局群开关 |
| `@bot /enable <qq> private` / `@bot /disable <qq> private` | superuser | 全局私聊白名单增删 |
| `/bu add\|del <qq>` / `/bu list` | superuser | 全局黑用户（群/私聊都生效） |
| `/bot` / `/bot status` / `/bot <name>` | 无 | 列表 / 状态 / 详情 |
| `/group status` / `/group <name>` | superuser | 群启用列表 / bot 群开关明细 |
| `/group status private` | superuser | 全局私聊白名单 |
| `/group <name> private` | superuser | 某 bot 私聊详情 |
| `/<name> on\|off` | superuser | 当前群在该 bot 群名单中的开关 |
| `/<name> on\|off <qq> private` | superuser | 该 bot 私聊名单开关 |

注意：

- `/enable`、`/disable` 必须 @bot 才被当作指令，没 @ 会原样放行给 bot；
- 指令处理可能调用 OneBot API（比如解析群名），所以必须在独立 task 里跑
  （`dispatch::handle_group_event` 已经是 spawn 出去的）：在读循环里同步等 API 回包会死锁；
- `private` 形态的管理/查询不受当前群是否启用限制，但仍在群消息里执行；
- 文案与格式（`✔◉ / ✔◎ / ✔⊗ / ✘⟳ / ¿⟳`、uptime、延迟、`%Y-%-m-%-d %H:%M:%S`）是稳定输出，
  改文案算行为变更。

### 统一消息模型与 OneBot API 层

- `message.rs`：`Segment`（`type` + `data`）、`OutboundMessage`（`Text` / `Segments`）。
  入站解析、出站回复、改写都复用这一套。
- `onebot_api.rs`：
  - `Action`（要回包）：`GetGroupInfo` / `GetGroupList` / `GetLoginInfo`；
  - `SendAction`（发言类）：`SendGroupMsg` / `SendPrivateMsg`；
  - `call`（echo 关联 + 超时 + 错误映射）、`enqueue_send`、`encode_send`、`new_echo`；
  - `is_send_action` / `is_send_frame` 是发送类判定的唯一来源；
  - `ApiError::NotConnected` 的 Display 保持旧文案「没有连接到OneBot客户端」。
- 新加 OneBot 调用必须在这里加 typed struct，不要在业务模块手拼 `{"action": ...}`。

### 出站改写与回包抹除

bot → 上游的「发言类」API（`send_msg`、`send_group_msg`、`send_private_msg`、
`send_group_forward_msg` 等，判定见 `onebot_api::is_send_action`）会先过
`Filter::rewrite_outbound`，按 `rewrite-text` 规则顺序做正则替换，`message` 字段可能是
字符串也可能是消息段数组（只改 `text` 段）。

改写之后必须有配套动作，否则 bot 会看到改过的文本：

- 记录 echo 与正文指纹（`remember_rewrite`）；
- 对应的 API 回包到达时，按 echo / `message_id` / 正文指纹匹配
  （`sanitize_rewritten_api_response`），把 `data` 里的正文剥掉、只留 `message_id`；
- bot 自己上报的 `message_sent` 按同一套记录丢弃（`is_rewritten_self_report`）。

记录有效期 `REWRITE_TTL = 90s`，读写时顺带清理过期项。给新的发言类 action 加改写时，
三件事要一起改：`is_send_action`、`rewrite_outbound` 的调用点、回包抹除的匹配路径。

### 运行时状态（db.json）与自动清理

```json
{
  "enabled_groups": [],
  "enabled_private_users": [],
  "black_users": [],
  "bots": { "<name>": { "groups": [], "users": [] } },
  "group_names": {},
  "joined_groups": []
}
```

- 全部读写走 `store.rs` 的 `Store` 方法，内部有 `parking_lot::RwLock`；落盘用
  「写临时文件再 rename」，不要自己 `fs::write` 直写 `db.json`。
- 旧版 `bots[name].ids` 通过 serde alias 读成 `groups`；缺失字段按空值补齐。
- 清理（`server.prune-runtime-state`，默认 true）：
  - 启动/热更：`prune_config_bots` 删除配置里不存在的 bot 条目；
  - `get_group_list` 成功且列表非空：`prune_groups` 删除不在列表里的 `enabled_groups`、
    `bots[*].groups`、`group_names`；列表为空只 warn 不清理；
  - 私聊用户名单不自动清理（无法可靠判断）。
- `group_names` / `joined_groups` 是缓存，靠 `get_group_list` 刷新；`resolve_group_name`
  先查 API，失败或超时才退回缓存。

### 配置与热更新

- `serde_yaml_ng` 解析；`notify` 监听 `config.yaml`（200ms 去抖）；校验失败保留旧配置、
  只打日志。
- 检测到旧字段（`group-id` / `private-message` / `group-message` / 扁平 `message`）时
  启动报明确迁移错误，不静默忽略。
- `Config::check` 的约束：`server.host/port/bot-id/user-agent` 必填；
  `bot-apps[].name` 不能带空格、不能占用内置词；`name` 与所有 `alias` 全局大小写不敏感互斥；
  `mode.group` / `mode.private` 必须是 `whitelist` / `blacklist`；
  `message.group/private.mode` 只能是 `on/off/whitelist/blacklist`；
  `rewrite-text.original` 必须能编译。
- 热更顺序：`prune_config_bots` → `sync_bots_from_config` → 替换 `App.config` →
  重编译已存在的过滤器。热更只更新**已存在**的过滤器，新增/删除 bot 需要重启进程。

### 并发模型

- tokio 多线程 runtime；每个 bot 一条长生命周期 task（`ws_client_handler`），
  bot 的读/写/ping 各一条子 task，断开时 abort 并清理。
- 共享状态都收在 `App`（`global.rs`）：`config: ArcSwap<Config>`、`store: Arc<Store>`、
  `filters: RwLock<Vec<Arc<RwLock<Filter>>>>`、`clients: RwLock<Vec<Arc<Client>>>`、
  `upstream: RwLock<UpstreamConn>`、统计计数用 `AtomicU64`、改写记录用
  `Mutex<RewriteTracker>`、上游唯一连接位用 `AtomicBool` CAS。
- 写 WebSocket 一律经 channel 串行：上游 `mpsc::UnboundedSender<UpstreamWrite>`，
  下游 `mpsc::UnboundedSender<ClientWrite>`；写成功后再按标记计数。
- 持锁时不要做网络 IO，也不要在 `parking_lot` guard 生命周期内 `.await`；
  需要 await 的逻辑先取快照再放锁。
- 指令处理可能调 API，必须在独立 task 里跑，不能在读循环里同步等回包。

## 编码规范

### 语言与框架

- Rust 2021 edition，`rust-version = "1.85"`；只用 `tokio`、`tokio-tungstenite`（rustls）、
  `serde` / `serde_json` / `serde_yaml_ng`、`fancy-regex`、`notify`、`parking_lot`、
  `arc-swap`、`chrono`、`log`、`anyhow` 这些依赖，不要引入需要 C 工具链的 TLS/加密库，
  保证三个平台都能原生构建。
- 源码按职责拆分模块，改逻辑时先看对应模块，不要跨模块复制实现。
- 消息段 / OneBot action 只能走 `message.rs` 与 `onebot_api.rs`。

### 错误处理

- 启动阶段用 `anyhow::Result`，配置/状态加载失败直接退出（`main.rs` 打日志后 exit(1)）。
- 运行时路径不 `unwrap` / 不 `panic`：连接错误写日志并清理，消息解析失败就丢弃或原样转发。
- 错误日志写清楚是哪个 bot、哪个群、哪个 action，文案保持稳定。

### 命名与风格

- 模块/文件 `snake_case`，类型 `PascalCase`，函数 `snake_case`，与现有代码保持一致。
- 注释解释「为什么」和「不变量」，不写废话；同一段逻辑的注释保持中文。
- 提交前必须 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings` 干净。

### 日志

- 用 `log` 宏；`logging.rs` 把格式固定成 `YYYY/MM/DD HH:MM:SS message`（写 stderr）。
- `server.debug` 只控制「每条消息的判定结果」这类高频日志，连接建立/断开、错误无条件打。
- 日志里可以出现群号/账号用于排查，但**不要**打 access-token、完整消息正文。

## 测试

- `cargo test`：单测在各模块 `#[cfg(test)] mod tests`；集成测试在 `tests/gateway.rs`
  （进程内起 mock OneBot 上游 + mock bot 下游），覆盖：
  唯一上游限制（含并发）、群开关与指令、群内容过滤/黑用户、私聊全局白名单 +
  per-bot 名单 + 私聊内容过滤、`private` 参数指令与查询、出站改写与回包抹除、db 清理。
- 需要真实 OneBot 实现与 bot 应用的联调不在 CI 里做：本地起一个空 `bot-apps`
  的配置确认能监听，再接入真实环境看日志。
- CI（`.github/workflows/build.yml`）在 push / PR 上跑 `cargo fmt --check`、
  `cargo clippy --all-targets -- -D warnings`、`cargo test`，并构建三个平台。

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
4. 同步更新 `docs/README_zh.md` / `docs/API.md` 的配置表；
5. 如果影响热更，确认 `config::reload` 路径能正确处理。

### 新增/修改指令

1. 在 `src/command.rs` 的 `try_handle_command` 里识别指令形态（大小写、别名、是否要 @bot、
   末位 `private` 参数的含义）；
2. 在 `needs_super` / `private_related` 里决定权限和是否受群开关限制；
3. 写处理函数，通过 `reply_group_text` 回复，不要自己拼 WebSocket 帧；
4. 更新 `docs/README_zh.md` 与 `docs/API.md` 的指令表。

### 修改群/私聊闸门

- 入口是 `src/gate.rs`；先想清楚改的是全局开关、黑用户，还是 per-bot 名单；
- 顺序是稳定行为：群聊「命令 → 群启用 → 黑用户 → 群名单」；
  私聊「私聊白名单 → 黑用户 → 私聊名单」；
- 内容过滤放 `filters.rs`，投递循环只有 `dispatch::deliver_to_bots` 一处。

### 新增 OneBot API

1. 在 `src/onebot_api.rs` 加 typed `Action` / `SendAction`；
2. 需要改写判定就同步 `is_send_action` 列表；
3. 在 `api.rs` / `command.rs` 等调用方使用 typed 接口；
4. 更新 `docs/API.md` 的 API 清单。

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
[Feat] Add private message routing and state pruning
[Fix] Drop rewritten self reports
[Chore] Unify OneBot API layer
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
- 配置破坏性变更（第二代 schema）与 db 自动清理必须在 release notes 里写清楚；
- 不要提内部基础设施（内网地址、服务器名、私有部署细节），也不要带真实账号信息；
- 预发布（`-alpha` / `-beta` / `-rc`）勾选 pre-release，正式版不勾。
