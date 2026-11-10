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
pub mod gate;
pub mod global;
pub mod logging;
pub mod message;
pub mod onebot_api;
pub mod onebot_message;
pub mod rewrite;
pub mod server;
pub mod store;

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use tokio::net::TcpListener;

use crate::global::App;
use crate::store::Store;

/// 命令行参数解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliAction {
    /// 运行程序，使用指定配置文件。
    Run(String),
    /// 打印版本号后退出。
    Version,
    /// 打印帮助后退出。
    Help,
}

/// `--help` 文本。
pub const USAGE: &str = "\
OneBotFilter — OneBot v11 WebSocket 消息过滤网关

用法: onebotfilter [选项]

选项:
  -c, --config <路径>   配置文件路径（默认 config.yaml）
  -V, --version         显示版本号
  -h, --help            显示本帮助
";

/// 解析命令行参数；未知参数直接报错，避免启动到一半才发现拼错。
pub fn parse_cli(args: &[String]) -> Result<CliAction, String> {
    let mut config = "config.yaml".to_string();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "-V" | "--version" => return Ok(CliAction::Version),
            "-h" | "--help" => return Ok(CliAction::Help),
            "-c" | "--config" => {
                i += 1;
                let Some(path) = args.get(i) else {
                    return Err(format!("--config 缺少路径\n\n{USAGE}"));
                };
                config = path.clone();
            }
            _ if arg.starts_with("--config=") => {
                config = arg["--config=".len()..].to_string();
            }
            _ => return Err(format!("未知参数: {arg}\n\n{USAGE}")),
        }
        i += 1;
    }
    if config.trim().is_empty() {
        return Err(format!("配置文件路径不能为空\n\n{USAGE}"));
    }
    Ok(CliAction::Run(config))
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn cli_defaults_to_config_yaml() {
        assert_eq!(
            parse_cli(&[]).unwrap(),
            CliAction::Run("config.yaml".into())
        );
    }

    #[test]
    fn cli_reads_config_path() {
        assert_eq!(
            parse_cli(&args(&["--config", "a.yaml"])).unwrap(),
            CliAction::Run("a.yaml".into())
        );
        assert_eq!(
            parse_cli(&args(&["-c", "b.yaml"])).unwrap(),
            CliAction::Run("b.yaml".into())
        );
        assert_eq!(
            parse_cli(&args(&["--config=c.yaml"])).unwrap(),
            CliAction::Run("c.yaml".into())
        );
    }

    #[test]
    fn cli_version_and_help() {
        for flag in ["--version", "-V"] {
            assert_eq!(parse_cli(&args(&[flag])).unwrap(), CliAction::Version);
        }
        for flag in ["--help", "-h"] {
            assert_eq!(parse_cli(&args(&[flag])).unwrap(), CliAction::Help);
        }
    }

    #[test]
    fn cli_rejects_bad_args() {
        assert!(parse_cli(&args(&["--nope"])).is_err());
        assert!(parse_cli(&args(&["--config"])).is_err());
        assert!(parse_cli(&args(&["--config="])).is_err());
    }
}
