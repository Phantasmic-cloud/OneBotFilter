# 测试

单元测试与源码放在一起（模块内 `#[cfg(test)] mod tests`），端到端集成测试在
`tests/gateway.rs`（进程内起 mock OneBot 上游 + mock bot 下游）。

```bash
cargo test
```

覆盖：

- 配置校验与旧字段检测、message 分组（`src/config.rs`）
- 群/私聊闸门：全局开关、黑用户、per-bot 白/黑名单（`src/gate.rs`）
- 过滤模式、前缀替换、群聊/私聊各自配置（`src/filters.rs`）
- 出站改写规则、回包正文抹除、自身上报识别、TTL 清理（`src/rewrite.rs`）
- `db.json` 状态迁移（旧 `ids` → `groups`）、双名单、无用状态清理（`src/store.rs`）
- 消息模型、OneBot action/echo/回包（`src/message.rs`、`src/onebot_api.rs`）
- 消息解析、@我判断、CQ 码处理（`src/onebot_message.rs`）
- 端到端：唯一上游限制（含并发）、群开关与指令、内容过滤、黑用户、
  私聊全局白名单 + per-bot 名单 + 私聊内容过滤、群号目标解析与缺参提醒、
  出站改写与回包抹除、db 清理

与 Go 旧版的差分对比脚本是本地维护的，不放进仓库；CI 只跑 `fmt` / `clippy` / `test`。
