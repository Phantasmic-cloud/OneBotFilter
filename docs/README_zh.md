# OneBotFilter

[English](../README.md) | 中文

基于 OneBot v11 反向 WebSocket 的消息过滤网关。它位于一个 OneBot 实现和多个 bot 应用
之间：一条事件通过全局开关、全局黑名单、该 bot 自己的名单和内容过滤后，才会转发给对应
bot。

原项目：[xuanmingLQ/OneBotFilter](https://github.com/xuanmingLQ/OneBotFilter)（Go 实现），
本仓库是它的 Rust 重写版。

## 主要功能

- 群聊和私聊各有一套全局开关、per-bot 名单与内容过滤
- 过滤器热加载；可选出站文本改写，并从 API 回包里抹除改写痕迹
- 运行状态存放在 `db.json`，自动清理无用的 bot/群状态

## 环境

- Rust 1.85+（仅编译需要）
- 一个能反向 WS 连接的 OneBot v11 实现
- 一个或多个接受反向 WS 连接的 bot 应用

## 快速开始

预编译包见 [Releases](https://github.com/Phantasmic-cloud/OneBotFilter/releases)：

```text
onebotfilter-linux-amd64
onebotfilter-windows-amd64.exe
onebotfilter-macos-arm64
```

或从源码构建：

```bash
cargo build --release
cp config-example.yaml config.yaml
./target/release/onebotfilter
```

OneBot 实现连到 `ws://127.0.0.1:3939/ws`；程序按 `bot-apps[].uri` 主动连接各 bot。
日志出现 `OneBotFilter已启动 ...` 即就绪。

程序支持 `-c/--config <路径>`（默认 `config.yaml`）、`-V/--version`、`-h/--help`。

## 文档

- [协议、配置、指令与 API](./API.md)
- [架构与开发说明](../AGENTS.md)

## 致谢

- [xuanmingLQ/OneBotFilter](https://github.com/xuanmingLQ/OneBotFilter) — 原始 Go 实现
- [OneBot v11](https://github.com/botuniverse/onebot-11) — 协议规范

## 许可证

[Apache License 2.0](../LICENSE)。第三方组件见 [NOTICE](../NOTICE)。
