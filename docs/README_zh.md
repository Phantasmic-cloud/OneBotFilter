# OneBotFilter 中文说明

[English](../README.md) | 中文

## 这是什么

一个 OneBot v11 消息过滤网关。一条流量路径：

```
QQ 消息 → OneBot 实现 ──ws──▶ OneBotFilter ──过滤──▶ bot 应用 A
                                                   └─▶ bot 应用 B
```

OneBotFilter 自己既是服务端也是客户端：

- 对**下游 bot 应用**：它是服务端。各 bot 应用按配置里的 `uri` 由本程序主动连出去
  （反向 WebSocket 模式，与 NoneBot 等的 reverse-ws 接入方式一致）。
- 对**上游 OneBot 实现**：它是客户端。程序把自己的接入地址
  `ws://<host>:<port><suffix>` 交给 OneBot 实现去连，只接受一个连接。

## 安装与运行

需要 Rust 1.85+ 才能编译；运行只需要一个二进制。

```bash
# 编译（release）
cargo build --release        # 或 sh scripts/build.sh

# 运行
cp config-example.yaml config.yaml
chmod +x target/release/onebotfilter
./target/release/onebotfilter
```

Release 里提供三个平台的预编译包（由 CI 原生构建）：

```text
onebotfilter-linux-amd64
onebotfilter-windows-amd64.exe
onebotfilter-macos-arm64
```

## 群聊与私聊的转发规则

群聊和私聊各有一套“全局开关 + per-bot 名单 + 内容过滤”的链路：

```text
群消息
  1. 内置指令（可调 API；/enable、/disable 及 private 形态不受群开关限制）
  2. 群是否启用？否 → 丢弃
  3. 发送者是否全局黑用户？是 → 丢弃
  4. 每个 bot：
     a. 该 bot 的群名单（mode.group）是否允许本群？
     b. message.group 内容过滤（前缀/正则）是否通过？
     c. 转发

私聊消息
  1. 发送者是否在全局私聊白名单（/enable <qq> private）？否 → 丢弃
  2. 发送者是否全局黑用户？是 → 丢弃
  3. 每个 bot：
     a. 该 bot 的私聊用户名单（mode.private）是否允许该用户？
     b. message.private 内容过滤是否通过？
     c. 转发
```

- 私聊消息**不触发**内置指令（指令只在群消息里生效）；notice/request 等非 message 事件不转发。
- 全局 `/bu` 黑名单对群聊和私聊都生效。
- 私聊默认关闭：必须先用 `@bot /enable <qq> private` 把用户加入全局私聊白名单，
  再用 `/<bot> on <qq> private` 允许该 bot 接收这个用户的私聊（`mode.private` 为黑名单时语义相反）。

## 配置

第二代 schema（**破坏性变更**，旧配置会启动报错并提示迁移）：

```yaml
bot-apps:
  - name: "alpha"
    alias: [ "a" ]
    uri: "ws://127.0.0.1:8080/onebot/v11/ws"
    access-token: ""

    # 名单模式：group 管群名单，private 管私聊用户名单
    mode:
      group: "blacklist"
      private: "whitelist"

    # 消息过滤：群聊/私聊分别配置
    message:
      group:
        mode: "blacklist"
        filters: [ "^/" ]
        prefix: [ "#" ]
        prefix-replace: "/"
      private:
        mode: "on"

    rewrite-text:
      - original: "v[0-9]+\\.[0-9]+\\.[0-9]+"
        replace: "v1.0.0"
```

`server` 配置：

| 键 | 说明 |
| --- | --- |
| `server.host` / `server.port` | 监听地址与端口 |
| `server.suffix` | OneBot 客户端接入路径，如 `/ws` |
| `server.bot-id` | 本 bot 账号，用于判断「@我」 |
| `server.user-agent` | 连 bot 应用时带的 UA |
| `server.superusers` | 可用管理指令的账号列表 |
| `server.command-prefix` | 指令前缀，默认 `/` |
| `server.db-path` | 运行时状态文件，默认 `db.json` |
| `server.buffer-size` | WebSocket 读写缓冲 |
| `server.sleep-time` | 重连间隔（秒） |
| `server.debug` | 打印每条消息的过滤判定 |
| `server.prune-runtime-state` | 自动清理 db.json 里已不存在的 bot/群状态，默认 `true` |

`bot-apps[]` 配置：

| 键 | 说明 |
| --- | --- |
| `name` | bot 名字，指令里用；不能带空格，也不能占用内置词 |
| `alias` | 别名列表，不能与其它 name/alias 重复 |
| `uri` | bot 应用的反向 WS 地址 |
| `access-token` | 鉴权 token，连接时以 `Bearer` 头带上 |
| `mode.group` | 群名单模式：`whitelist` 或 `blacklist`（必填） |
| `mode.private` | 私聊用户名单模式：`whitelist` 或 `blacklist`（必填） |
| `message.group` | 群消息过滤：`mode` / `filters` / `prefix` / `prefix-replace` |
| `message.private` | 私聊消息过滤，字段同上 |
| `rewrite-text` | 出站文本改写规则 |

`message.*.mode` 取值：`on` 全放行、`off` 全拦截、`whitelist` 命中才放行、`blacklist` 命中才拦截。

## 名单语义

- `mode.group` + `db.json` 的 `bots[name].groups`：
  - `whitelist`：`groups` 里的群才收该 bot 的消息；
  - `blacklist`：`groups` 里的群不收，其余都收。
- `mode.private` + `db.json` 的 `bots[name].users`：
  - `whitelist`：`users` 里的用户才收该 bot 的私聊；
  - `blacklist`：`users` 里的用户不收，其余都收。
- 全局私聊白名单 `enabled_private_users` 在两边之前生效（类似 `enabled_groups`）。

## 群管理指令

指令只在**群消息**里生效，且要求 `server.command-prefix` 前缀（默认 `/`）。
`/enable`、`/disable` 必须 @bot 才认；管理类指令需要 `superusers`。
私聊相关操作不新增顶层指令，而是在原指令后加 `private` 参数。

| 指令 | 权限 | 作用 |
| --- | --- | --- |
| `@bot /enable [群号]` | superuser | 启用某群的 BOT 服务，缺省当前群 |
| `@bot /disable [群号]` | superuser | 禁用某群的 BOT 服务 |
| `@bot /enable <qq> private` | superuser | 把 qq 加入全局私聊白名单 |
| `@bot /disable <qq> private` | superuser | 把 qq 移出全局私聊白名单 |
| `/bu add\|del <qq>` | superuser | 全局黑用户增减（群/私聊都生效） |
| `/bu list` | superuser | 列出黑用户 |
| `/bot` | 无 | 本群已开/已关的 bot 列表 |
| `/bot status` | 无 | 连接状态、延迟、收发计数 |
| `/bot <name\|alias>` | 无 | 单个 bot 详情（含名单模式与群消息配置） |
| `/group status` | superuser | 已启用/已禁用的群列表 |
| `/group status private` | superuser | 全局私聊白名单用户列表 |
| `/group <name\|alias>` | superuser | 某 bot 在各群的开关明细 |
| `/group <name\|alias> private` | superuser | 某 bot 的私聊详情（名单、过滤配置） |
| `/<name\|alias> on\|off` | superuser | 当前群开/关某个 bot（群名单） |
| `/<name\|alias> on\|off <qq> private` | superuser | 允许/禁止某用户私聊该 bot（私聊名单） |

权限不足或群未启用时指令静默返回，不打扰群里（`private` 形态的管理/查询不受当前群开关限制）。

## 出站文本改写

`rewrite-text` 里的规则按顺序对 bot 的「发言类」API（`send_group_msg`、`send_private_msg` 等）
正文做正则替换。改写之后：

- 对应的 API 回包会被剥掉 `data` 正文，只留 `message_id`；
- bot 自己上报的那条 `message_sent` 会被丢弃。

两者都是为了避免改过的文本被 bot 侧看到。记录按 echo / message_id / 正文指纹匹配，
有效期 90 秒。

## 运行时状态

`db.json` 结构：

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

- `enabled_groups`：启用 BOT 服务的群
- `enabled_private_users`：全局私聊白名单用户
- `black_users`：全局黑用户（群聊/私聊都丢弃）
- `bots.<name>.groups` / `users`：配合 `mode.group` / `mode.private` 的 per-bot 名单
- `group_names` / `joined_groups`：群名与已加入群的缓存

自动清理（`server.prune-runtime-state: true`，默认开）：

- 启动/热更时删掉配置里已不存在的 bot 条目；
- `get_group_list` 成功且列表非空时，删掉不在列表里的 `enabled_groups`、per-bot `groups`、`group_names`；
- 列表为空或刷新失败时不清理（避免实现方异常导致误删）；
- 私聊用户名单不自动清理（无法可靠判断）。

配置文件改动会热加载：校验通过后替换配置、同步 bot 名单、重新编译过滤器，并清理无用状态。
热加载只会更新**已存在**的过滤器，新增或删除 bot 需要重启。

## 从旧配置迁移

| 旧字段 | 新字段 |
| --- | --- |
| `group-id.mode` | `mode.group` |
| `group-message` | `message.group` |
| `private-message` | `message.private` |
| 旧扁平 `message` | 拆成 `message.group` / `message.private` |
| （无） | `mode.private`（新增，必填） |

旧 `db.json` 的 `bots[].ids` 会自动迁移为 `groups`，`enabled_private_users` 默认为空（私聊关闭）。

## 许可

[Apache License 2.0](../LICENSE)。
