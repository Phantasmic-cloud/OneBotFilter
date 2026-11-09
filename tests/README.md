# 测试

单元测试与源码放在一起（`src/*_test.rs` 的 `#[cfg(test)] mod tests`），集成测试在
`tests/gateway.rs`（进程内起 mock OneBot 上游 + mock bot 下游）。

```bash
cargo test
```

覆盖：

- 配置校验与默认值/继承（`src/config.rs`）
- 过滤模式、前缀替换、私有/群聊继承（`src/filters.rs`）
- 出站改写规则、回包正文抹除、自身上报识别、TTL 清理（`src/rewrite.rs`）
- `db.json` 状态迁移与持久化（`src/store.rs`）
- 消息解析、@我判断、CQ 码处理（`src/onebot_message.rs`）
- 发言类 API 判定（`src/stats.rs`）
- 端到端：唯一上游限制、群开关、`/enable` 指令、内容过滤、黑用户、出站改写与回包抹除

与 Go 旧版的差分对比脚本是本地维护的，不放进仓库；CI 只跑 `fmt` / `clippy` / `test`。
