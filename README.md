# OneBotFilter

[中文文档](./docs/README_zh.md) | English

A WebSocket filtering gateway for OneBot v11: one OneBot implementation connects in,
multiple bot applications connect out, and messages reach a bot only after passing
global switches and per-bot allow lists.

- Pure Rust, no CGO: a single static-ish binary
- Reverse WebSocket on both sides (server towards the OneBot implementation, client towards bot apps)
- **Group and private chat both supported**: group switches + per-bot group lists for groups;
  a global private-user whitelist + per-bot private-user lists for direct messages
- Per-bot content filters (prefix / whitelist / blacklist) configured separately for group and private messages
- Outbound text can be rewritten with regex rules; rewritten echoes are scrubbed from API responses
- Runtime state (switches, lists, caches) lives in `db.json`; stale bot/group state is pruned automatically

## Quick start

```bash
# Build from source (release). Requires Rust 1.85+.
cargo build --release          # -> target/release/onebotfilter
# or: sh scripts/build.sh

cp config-example.yaml config.yaml
./target/release/onebotfilter
```

The gateway listens on `ws://<host>:<port>/ws` for the OneBot implementation and
connects out to each `bot-apps[].uri`. Once the log prints `OneBotFilter已启动 ...`,
it is ready.

Prebuilt release binaries:

```text
onebotfilter-linux-amd64
onebotfilter-windows-amd64.exe
onebotfilter-macos-arm64
```

## Layout

```
src/
  main.rs            entry point
  lib.rs             startup wiring: config, state, listener, clients, hot reload
  global.rs          shared App state, connections, counters, filter registry
  config.rs          config (mode.group/private, message.group/private), validation, hot reload
  message.rs         shared message segment / outbound message model
  onebot_api.rs      typed OneBot actions, echo correlation, response parsing
  gate.rs            shared group/private gating
  store.rs           db.json state: switches, allow lists, caches, pruning
  api.rs             high-level API: group names, group list, latency
  onebot_message.rs  inbound event parsing, @me, command text
  filters.rs         content filters (group and private configs)
  rewrite.rs         outbound text rewriting and echo scrubbing
  command.rs         built-in chat commands and status text
  dispatch.rs        inbound dispatch and delivery
  server.rs          upstream OneBot connection (single)
  client.rs          downstream bot connections, reconnect, latency probe
tests/gateway.rs     end-to-end integration tests
docs/README_zh.md    full Chinese guide
docs/API.md          protocol, config and internal API reference
AGENTS.md            architecture and contribution notes
```

Configuration, the command list and the gating order are documented in
[`docs/API.md`](./docs/API.md) and [`AGENTS.md`](./AGENTS.md).

## License

[Apache License 2.0](./LICENSE). Third-party components are listed in [`NOTICE`](./NOTICE).
