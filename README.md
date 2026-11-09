# OneBotFilter

[中文文档](./docs/README_zh.md) | English

A WebSocket filtering gateway for OneBot v11: one OneBot implementation connects in,
multiple bot applications connect out, and group messages reach a bot only after
passing a per-group switch, a blacklist check, that bot's group list and its content
filters.

- Pure Rust, no CGO: a single static binary
- Reverse WebSocket on both sides (server towards the OneBot implementation, client towards bot apps)
- Group switches, black users and per-bot group lists are runtime state, editable from chat and persisted to `db.json`
- Outbound text can be rewritten with regex rules; rewritten echoes are scrubbed from API responses

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
  config.rs          config, validation, hot reload
  onebot_message.rs  event parsing, @me, command text
  stats.rs           send-like API detection
  store.rs           db.json runtime state
  api.rs             echo-correlated OneBot API calls
  filters.rs         content filters (prefix / whitelist / blacklist)
  rewrite.rs         outbound text rewriting and echo scrubbing
  command.rs         built-in chat commands and status text
  dispatch.rs        inbound dispatch and gating
  server.rs          upstream OneBot connection (single)
  client.rs          downstream bot connections, reconnect, latency probe
tests/gateway.rs     end-to-end integration tests
docs/README_zh.md    full Chinese guide
AGENTS.md            architecture and API notes
```

Configuration, the command list and the gating order are documented in
[`AGENTS.md`](./AGENTS.md) and the [Chinese guide](./docs/README_zh.md).

## License

[Apache License 2.0](./LICENSE). Third-party components are listed in [`NOTICE`](./NOTICE).
