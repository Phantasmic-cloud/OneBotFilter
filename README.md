# OneBotFilter

[中文](./docs/README_zh.md) | English

A reverse-WebSocket gateway for OneBot v11. It sits between one OneBot
implementation and several bot applications, and forwards an event to a bot only
after the global switch, the global blacklist, the bot's own allow list and its
content filters pass.

This repository is a Rust rewrite of
[xuanmingLQ/OneBotFilter](https://github.com/xuanmingLQ/OneBotFilter) (Go).

## Features

- Group and private chat, each with its own global switch, per-bot allow list and content filters
- Hot-reloaded filters; optional outbound text rewriting with echo scrubbing
- Runtime state in `db.json`; stale bot/group state is pruned automatically

## Requirements

- Rust 1.85+ (to build)
- A OneBot v11 implementation that can connect as a reverse-WS client
- One or more bot apps that accept reverse-WS connections

## Quick start

Prebuilt binaries are attached to
[Releases](https://github.com/Phantasmic-cloud/OneBotFilter/releases):

```text
onebotfilter-linux-amd64
onebotfilter-windows-amd64.exe
onebotfilter-macos-arm64
```

Or build from source:

```bash
cargo build --release
cp config-example.yaml config.yaml
./target/release/onebotfilter
```

The OneBot implementation connects to `ws://127.0.0.1:3939/ws`; the gateway
dials every `bot-apps[].uri`. The log line `OneBotFilter已启动 ...` means ready.

## Docs

- [Chinese guide](./docs/README_zh.md)
- [Protocol, config and API reference](./docs/API.md)
- [Architecture notes](./AGENTS.md)

## Credits

- [xuanmingLQ/OneBotFilter](https://github.com/xuanmingLQ/OneBotFilter) — the original Go implementation
- [OneBot v11](https://github.com/botuniverse/onebot-11) — protocol specification

## License

[Apache License 2.0](./LICENSE). Third-party components are listed in
[NOTICE](./NOTICE).
