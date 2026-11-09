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

所以一条群消息的完整链路是：

```
OneBot 实现 ──▶ server.go 读帧 ──▶ dispatch.go 过闸 ──▶ client.go 写帧 ──▶ bot 应用
bot 应用     ──▶ client.go 读帧 ──▶（改写/计数）────▶ server.go 写帧 ──▶ OneBot 实现
```

## 安装与运行

需要 Rust 1.85+ 才能编译；运行只需要一个静态二进制。

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

`config.yaml` 与程序放在同一目录，`db-path` 指向的 `db.json` 会自动创建。
日志里出现下面这行即就绪：

```
OneBotFilter已启动 ws://127.0.0.1:3939/ws
```

## 配置

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
| `bot-apps[].name` | bot 名字，指令里用；不能带空格，也不能占用内置词 |
| `bot-apps[].alias` | 别名列表，同样不能与其它 name/alias 重复 |
| `bot-apps[].uri` | bot 应用的反向 WS 地址 |
| `bot-apps[].access-token` | 鉴权 token，连接时以 `Bearer` 头带上 |
| `bot-apps[].group-id.mode` | `whitelist` / `blacklist`，决定该 bot 收哪些群 |
| `bot-apps[].message` | 消息过滤总配置 |
| `bot-apps[].private-message` | 私聊过滤，缺省继承 `message` |
| `bot-apps[].group-message` | 群聊过滤，缺省继承 `message` |
| `bot-apps[].rewrite-text` | 出站文本改写规则 |

`message` 里的 `mode`：

| 值 | 行为 |
| --- | --- |
| `on` | 全部放行 |
| `off` | 全部拦截 |
| `whitelist` | 命中 `filters` 或带 `prefix` 才放行 |
| `blacklist` | 命中 `filters` 或带 `prefix` 则拦截，其余放行 |

`private-message` / `group-message` 的 `mode` 还可以填 `default`，表示继承 `message`。

## 群管理指令

指令只在**群消息**里生效，且要求 `server.command-prefix` 前缀（默认 `/`）。
`/enable`、`/disable` 必须 @bot 才认；管理类指令需要 `superusers`。

| 指令 | 权限 | 作用 |
| --- | --- | --- |
| `@bot /enable [群号]` | superuser | 启用某群的 BOT 服务，缺省当前群 |
| `@bot /disable [群号]` | superuser | 禁用某群的 BOT 服务 |
| `/bu add\|del <qq>` | superuser | 全局黑用户增减 |
| `/bu list` | superuser | 列出黑用户 |
| `/bot` | 无 | 本群已开/已关的 bot 列表 |
| `/bot status` | 无 | 连接状态、延迟、收发计数 |
| `/bot <name\|alias>` | 无 | 单个 bot 的详情与当前群状态 |
| `/group status` | superuser | 已启用/已禁用的群列表 |
| `/group <name\|alias>` | superuser | 某 bot 在各群的开关明细 |
| `/<name\|alias> on\|off` | superuser | 当前群开/关某个 bot |

权限不足或群未启用时指令静默返回，不打扰群里。

## 只转发群事件

无 `group_id` 的 `message` / `notice` / `request`（含私聊、好友戳一戳）一律不转发给
bot 应用。这是有意为之：本项目面向群聊场景，私聊不在转发范围内。

## 出站文本改写

`rewrite-text` 里的规则按顺序对 bot 的「发言类」API（`send_group_msg` 等）正文做正则替换。
改写之后：

- 对应的 API 回包会被剥掉 `data` 正文，只留 `message_id`；
- bot 自己上报的那条 `message_sent` 会被丢弃。

两者都是为了避免改过的文本被 bot 侧看到。记录按 echo / message_id / 正文指纹匹配，
有效期 90 秒。

## 运行时状态

`db.json` 结构：

```json
{
  "enabled_groups": [],
  "black_users": [],
  "bots": { "<name>": { "ids": [] } },
  "group_names": {},
  "joined_groups": []
}
```

- `enabled_groups`：启用 BOT 服务的群
- `black_users`：全局黑用户（这些人发的消息直接丢弃）
- `bots.<name>.ids`：配合该 bot 的 `group-id.mode` 使用。白名单模式下在列表里=放行，
  黑名单模式下在列表里=拦截
- `group_names` / `joined_groups`：群名与已加入群的缓存，靠 `get_group_list` 刷新

配置文件改动会热加载：校验通过后替换配置、同步 bot 名单、重新编译过滤器。
注意热加载只会更新**已存在**的过滤器，新增或删除 bot 需要重启。

## 许可

[Apache License 2.0](../LICENSE)。
