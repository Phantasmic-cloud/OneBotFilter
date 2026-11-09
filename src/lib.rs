//! OneBot v11 WebSocket filter gateway.
//!
//! 一个上游 OneBot 实现连进来，多个下游 bot 应用接出去；群事件逐层过闸后分发，
//! 出站发言可以按正则改写并把改写痕迹从回包里抹掉。

pub mod api;
pub mod client;
pub mod command;
pub mod config;
pub mod dispatch;
pub mod filters;
pub mod global;
pub mod logging;
pub mod onebot_message;
pub mod rewrite;
pub mod server;
pub mod stats;
pub mod store;

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use tokio::net::TcpListener;

use crate::global::App;
use crate::store::Store;

/// 启动入口：加载配置与状态、监听上游端口、为每个 bot-app 起客户端。
pub async fn run(config_path: &str) -> Result<()> {
    logging::init();

    let cfg = config::load_config(config_path).map_err(|e| anyhow!("加载配置异常: {}", e))?;
    let store =
        Store::load(&cfg.server.db_path, &cfg).map_err(|e| anyhow!("加载 db.json 异常: {}", e))?;
    let app = Arc::new(App::new(cfg, store));

    let addr = app.config().listen_addr();
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("监听服务出错: {}", addr))?;

    let cfg_snapshot = app.config();
    log::info!(
        "OneBotFilter已启动 ws://{}:{}{}",
        cfg_snapshot.server.host,
        cfg_snapshot.server.port,
        cfg_snapshot.server.suffix
    );

    for bot in cfg_snapshot.bot_apps.iter().cloned() {
        let app = app.clone();
        tokio::spawn(client::ws_client_handler(app, bot));
    }

    // 配置热更。
    config::spawn_watcher(app.clone(), config_path.to_string());

    server::accept_loop(app, listener).await
}
