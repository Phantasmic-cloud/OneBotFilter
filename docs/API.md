# OneBotFilter API 参考

本文档描述 OneBotFilter 对外暴露的接口（协议、配置、状态文件、聊天指令）以及内部
Rust API 的分层。所有示例值都是占位，不含真实账号信息。

## 1. 上游 OneBot 实现接入

| 项 | 值 |
| --- | --- |
| 传输 | WebSocket（明文 `ws://`，上游侧不提供 TLS） |
| 地址 | `ws://<server.host>:<server.port><server.suffix>`（默认 `ws://127.0.0.1:3939/ws`） |
| 并发 | 只接受一个连接；第二个连接返回 HTTP 403（body：`只能连接一个OneBot客户端`） |
| 路径 | 必须严格等于 `server.suffix`，其余路径 404 |

连上之后，本程序会：

1. 接收 OneBot v11 的事件与 API 回包文本帧；二进制帧原样广播给所有 bot。
2. 对 `post_type=message` 的群/私聊消息按 §3 的规则过闸后转发。
3. 其它事件（`meta_event`、notice/request 等）按原逻辑透传（无群号且非 message 的事件不转发）。
4. 本程序主动发起的 API 调用使用 `echo` 前缀 `obf_`；这类回包只投递给内部调用方，
   不会广播给 bot。改写过发言内容的回包会把 `data` 正文剥掉，只保留 `message_id`。
5. 收到 ping 会自动回 pong；本程序也会对下游 bot 发 ping 测延迟。

## 2. 下游 bot 应用接入

本程序按 `bot-apps[].uri` 主动连接（反向 WebSocket），并携带：

```text
x-self-id: <server.bot-id>
authorization: Bearer <bot-apps[].access-token>
user-agent: <server.user-agent>
x-client-role: Universal
```

- 连接失败按 `server.sleep-time` 秒重试；建连后再断开固定等 5s 重连。
- 建连后立即测一次 ping，之后每 30s 一次（2s 超时）。
- bot 可以发送任意 OneBot API 帧，本程序透传给上游；`send_*msg*` 这类发言类 action
  会先按 `rewrite-text` 改写正文。
- bot 收到的消息帧已按名单与内容过滤。

## 3. 群聊 / 私聊判定与闸门

```text
群消息（message_type=group, group_id≠0）：
  内置指令 → 群是否启用（enabled_groups）→ 全局黑用户（black_users）
  → 每个 bot：mode.group 群名单 → message.group 内容过滤 → 转发

私聊消息（message_type=private）：
  全局私聊白名单（enabled_private_users）→ 全局黑用户
  → 每个 bot：mode.private 私聊名单 → message.private 内容过滤 → 转发
```

- 私聊消息不触发内置指令；notice/request/meta_event 等不进入闸门。
- 名单语义：`whitelist` 时在列表内=允许，`blacklist` 时在列表内=拒绝（全局私聊白名单固定为白名单）。
- 内容过滤 `mode`：`on` 全放行、`off` 全拦截、`whitelist` 命中才放行、`blacklist` 命中才拦截；
  `prefix` 命中的消息会放行并按 `prefix-replace` 改写前缀。

## 4. 内置指令

只在群消息生效，必须带 `command-prefix`（默认 `/`）；`/enable`、`/disable` 需要 @bot；
管理类指令需要 `server.superusers`。权限不足/群未启用时静默返回。

| 指令 | 权限 | 作用 |
| --- | --- | --- |
| `@bot /enable [群号]` / `@bot /disable [群号]` | superuser | 启用/禁用群 |
| `@bot /enable <qq> private` / `@bot /disable <qq> private` | superuser | 全局私聊白名单增删 |
| `/bu add\|del <qq>`、`/bu list` | superuser | 全局黑用户 |
| `/bot`、`/bot status`、`/bot <name>` | 无 | 列表 / 状态 / 详情 |
| `/group status`、`/group <name>` | superuser | 群启用列表 / bot 群开关明细 |
| `/group status private` | superuser | 全局私聊白名单 |
| `/group <name> private` | superuser | 某 bot 的私聊详情 |
| `/<name> on\|off [群号]` | superuser | 该 bot 在指定群（默认当前群）的开关 |
| `/<name> on\|off <qq> private` | superuser | 该 bot 私聊名单开关 |

群号的解析规则对所有群内开关一致：写了的群号如果已加入（`get_group_list` 能查到）
就作用于该群，否则（没写、未加入或不是正整数）一律作用于当前群。
末尾带 `private` 时参数按 qq 处理，`数字 + private` 只改私聊名单、不会当群号；
只写 `private` 而没跟 qq 会回一条缺参提示，不会被当成群号执行。

## 5. 配置参考

### 5.1 server

| 键 | 类型 | 默认 | 说明 |
| --- | --- | --- | --- |
| `host` | string | 必填 | 监听地址 |
| `port` | u16 | 必填 | 监听端口（不能为 0） |
| `suffix` | string | 必填 | 接入路径 |
| `bot-id` | string | 必填 | 本 bot 账号 |
| `user-agent` | string | 必填 | 连下游 bot 的 UA |
| `superusers` | int[] | `[]` | 管理指令账号 |
| `command-prefix` | string | `/` | 指令前缀 |
| `db-path` | string | `db.json` | 状态文件 |
| `buffer-size` | int | `4096` | WS 读写缓冲 |
| `sleep-time` | float | `5` | 连接失败重试间隔（秒） |
| `debug` | bool | `false` | 打印逐条过滤判定 |
| `prune-runtime-state` | bool | `true` | 自动清理无用状态 |

### 5.2 bot-apps[]

| 键 | 类型 | 说明 |
| --- | --- | --- |
| `name` | string | 唯一名字，不含空格，不能占用内置词 |
| `alias` | string[] | 别名，全局大小写不敏感互斥 |
| `uri` | string | 反向 WS 地址 |
| `access-token` | string | Bearer token，可空 |
| `mode` | `whitelist`/`blacklist` | 群聊与私聊共用的名单模式 |
| `message` | object | 群聊与私聊共用的消息过滤（`mode`/`filters`/`prefix`/`prefix-replace`） |
| `group` | object | 群聊覆盖：`mode` 与/或 `message`，可选 |
| `private` | object | 私聊覆盖：`mode` 与/或 `message`，可选 |
| `rewrite-text` | object[] | 出站改写：`{ original, replace }`（正则） |

生效规则：

- 群聊 / 私聊先继承 bot 级 `mode` 与 `message`，再被同名侧块覆盖：`group.mode` /
  `private.mode` 整体替换沿用值，`group.message` / `private.message` 按字段合并
  （只写 `filters` 就只改 filters，其余沿用共用配置）；
- 群聊 / 私聊最终都必须拿到 `mode`（`whitelist` / `blacklist`）：写 bot 级 `mode`，
  或两侧各写 `group.mode` / `private.mode`；
- 群聊和私聊配置完全一样时只写 bot 级一份；完全不同时只写 `group` / `private` 两块。

旧配置迁移（启动时报错并提示）：

| 旧写法 | 新写法 |
| --- | --- |
| v1 `group-id.mode` | bot 级 `mode` |
| v1 `group-message` / `private-message` | `group.message` / `private.message` |
| v1 扁平 `message`（默认过滤） | bot 级 `message` |
| v2 `mode.group` / `mode.private` | `group.mode` / `private.mode` |
| v2 `message.group` / `message.private` | `group.message` / `private.message` |

## 6. db.json 状态

```json
{
  "enabled_groups": [],
  "enabled_private_users": [],
  "black_users": [],
  "bots": { "alpha": { "groups": [], "users": [] } },
  "group_names": {},
  "joined_groups": []
}
```

- 写入方式：整体 JSON，先写 `<path>.tmp` 再 rename；读写有锁。
- 旧版 `bots[].ids` 自动读成 `groups`；缺少的字段按空值补齐。
- 清理（`prune-runtime-state`）：启动/热更删掉配置里没有的 bot；`get_group_list`
  成功且非空时删掉不在列表里的群相关状态；私聊用户名单不自动清理。

## 7. 本程序使用的 OneBot API

| action | 用途 | 超时 |
| --- | --- | --- |
| `get_login_info` | 测上游 RTT | 3s |
| `get_group_info` | 解析群名 | 3s |
| `get_group_list` | 刷新已加入群/群名缓存 | 5s |
| `send_group_msg` | 回复内置指令 | 不等待回包 |

回包统一结构 `{status, retcode, data, message, wording}`；`retcode != 0 && status != "ok"`
视为失败。请求/回包关联用 `echo`（前缀 `obf_`）。

## 8. 内部 Rust API 分层

| 模块 | 职责 |
| --- | --- |
| `global::App` | 共享状态：配置快照、Store、连接、统计、过滤器注册、上游唯一连接位 |
| `message` | `Segment` / `OutboundMessage`：字符串与消息段数组两种形态 |
| `onebot_api` | typed action（`Action`/`SendAction`）、`call`、`enqueue_send`、`is_send_frame`、`new_echo`、回包投递 |
| `api` | 高层：`resolve_group_name`、`refresh_joined_groups`（含清理）、`measure_onebot_latency` |
| `gate` | `ChatKind`、`classify`、`global_gate`、`bot_list_gate` |
| `dispatch` | 入站分流、群/私聊统一投递（`deliver_to_bots`） |
| `filters` | `Filter::filter_content` 返回 `FilterDecision { allowed, modified }` |
| `rewrite` | `rewrite_outbound`、`sanitize_rewritten_api_response`、`is_rewritten_self_report` |
| `store` | db.json 状态与清理 |
| `command` | 指令解析与回复文案 |
| `server` / `client` | 上游/下游 WS 生命周期 |

## 9. 超时与重连

| 场景 | 行为 |
| --- | --- |
| 上游 API 调用 | 默认 3s 超时；`get_group_list` 5s |
| 下游 ping | 2s 超时，每 30s 一次 |
| 下游建连失败 | `server.sleep-time` 秒后重试 |
| 下游建连后断开 | 固定 5s 后重连 |
| 上游断开 | 清理 pending API 调用、复位流量，等待重新接入 |
