//! 进程内集成测试：mock OneBot 上游 + mock bot 下游，验证闸门、指令、过滤与改写。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;

use onebotfilter::client::ws_client_handler;
use onebotfilter::config::{
    BotAppsConfig, Config, GroupIdConfig, MessageConfig, RewriteTextRule, ServerConfig,
};
use onebotfilter::global::{App, BLACKLIST, ON};
use onebotfilter::server::accept_loop;
use onebotfilter::store::Store;

const BOT_ID: &str = "123456789";

fn temp_db_path() -> String {
    let dir = std::env::temp_dir().join(format!(
        "obf-it-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("db.json").to_str().unwrap().to_string()
}

fn test_config(bot_uri: &str, message: MessageConfig, rewrite: Vec<RewriteTextRule>) -> Config {
    Config {
        server: ServerConfig {
            host: "127.0.0.1".into(),
            port: 3939,
            suffix: "/ws".into(),
            bot_id: BOT_ID.into(),
            user_agent: "OneBotFilter".into(),
            superusers: vec![BOT_ID.parse().unwrap()],
            sleep_time: 0.2,
            ..Default::default()
        },
        bot_apps: vec![BotAppsConfig {
            name: "alpha".into(),
            uri: bot_uri.into(),
            group_id: GroupIdConfig {
                mode: BLACKLIST.into(),
            },
            message,
            rewrite_text: rewrite,
            ..Default::default()
        }],
    }
}

async fn spawn_gateway(mut cfg: Config) -> (Arc<App>, SocketAddr) {
    cfg.check().expect("测试配置应合法");
    let store = Store::load(&temp_db_path(), &cfg).unwrap();
    let bot_cfgs = cfg.bot_apps.clone();
    let app = Arc::new(App::new(cfg, store));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app_accept = app.clone();
    tokio::spawn(async move {
        let _ = accept_loop(app_accept, listener).await;
    });
    for bot in bot_cfgs {
        let app = app.clone();
        tokio::spawn(ws_client_handler(app, bot));
    }
    (app, addr)
}

async fn wait_for_client(app: &Arc<App>, name: &str) {
    for _ in 0..100 {
        if app.find_client(name).is_some() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("bot {name} 未连接");
}

/// mock 下游 bot：本程序连它（它做 WS 服务端）。
struct MockBot {
    tx: mpsc::UnboundedSender<Message>,
    rx: mpsc::UnboundedReceiver<Message>,
}

async fn spawn_mock_bot() -> (String, MockBot) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (to_test_tx, to_test_rx) = mpsc::unbounded_channel();
    let (from_test_tx, mut from_test_rx) = mpsc::unbounded_channel::<Message>();
    tokio::spawn(async move {
        let (stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(_) => return,
        };
        let ws = match tokio_tungstenite::accept_async(stream).await {
            Ok(ws) => ws,
            Err(_) => return,
        };
        let (mut sink, mut stream) = ws.split();
        loop {
            tokio::select! {
                Some(m) = from_test_rx.recv() => {
                    if sink.send(m).await.is_err() {
                        break;
                    }
                }
                incoming = stream.next() => match incoming {
                    Some(Ok(Message::Ping(p))) => {
                        let _ = sink.send(Message::Pong(p)).await;
                    }
                    Some(Ok(m)) => {
                        if to_test_tx.send(m).is_err() {
                            break;
                        }
                    }
                    _ => break,
                }
            }
        }
    });
    (
        format!("ws://{}", addr),
        MockBot {
            tx: from_test_tx,
            rx: to_test_rx,
        },
    )
}

/// mock 上游 OneBot：本程序接受它连入；自动回 echo API 请求，其它消息交给测试。
struct MockUpstream {
    tx: mpsc::UnboundedSender<Message>,
    rx: mpsc::UnboundedReceiver<Value>,
}

async fn connect_upstream(addr: SocketAddr) -> MockUpstream {
    for _ in 0..100 {
        match tokio_tungstenite::connect_async(format!("ws://{}{}", addr, "/ws")).await {
            Ok((ws, _)) => {
                let (app_tx, app_rx) = mpsc::unbounded_channel();
                let (test_tx, mut test_rx) = mpsc::unbounded_channel::<Message>();
                let (mut sink, mut stream) = ws.split();
                tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            Some(m) = test_rx.recv() => {
                                if sink.send(m).await.is_err() {
                                    break;
                                }
                            }
                            incoming = stream.next() => match incoming {
                                Some(Ok(Message::Text(t))) => {
                                    let v: Value = match serde_json::from_str(t.as_str()) {
                                        Ok(v) => v,
                                        Err(_) => continue,
                                    };
                                    let echo = v.get("echo").and_then(|e| e.as_str()).unwrap_or("");
                                    if echo.starts_with("obf_") {
                                        let resp = json!({
                                            "status": "ok",
                                            "retcode": 0,
                                            "data": {},
                                            "echo": echo,
                                        });
                                        let _ = sink.send(Message::Text(resp.to_string().into())).await;
                                    } else {
                                        let _ = app_tx.send(v);
                                    }
                                }
                                Some(Ok(_)) => {}
                                _ => break,
                            }
                        }
                    }
                });
                return MockUpstream {
                    tx: test_tx,
                    rx: app_rx,
                };
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    panic!("无法连接 gateway");
}

fn send_json(up: &MockUpstream, v: Value) {
    up.tx
        .send(Message::Text(v.to_string().into()))
        .expect("发送失败");
}

fn text_message(text: &str) -> Message {
    Message::Text(text.to_string().into())
}

fn group_message(group_id: i64, user_id: i64, text: &str) -> Value {
    json!({
        "post_type": "message",
        "message_type": "group",
        "message_format": "array",
        "message_id": 1001,
        "user_id": user_id,
        "group_id": group_id,
        "raw_message": text,
        "message": [{"type": "text", "data": {"text": text}}],
    })
}

async fn recv_action(up: &mut MockUpstream, action: &str) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remain = deadline.saturating_duration_since(tokio::time::Instant::now());
        match timeout(remain, up.rx.recv()).await {
            Ok(Some(v)) => {
                if v.get("action").and_then(|a| a.as_str()) == Some(action) {
                    return v;
                }
            }
            _ => panic!("等不到 action={action}"),
        }
    }
}

async fn expect_no_action(up: &mut MockUpstream, action: &str, wait: Duration) {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let remain = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remain.is_zero() {
            return;
        }
        match timeout(remain, up.rx.recv()).await {
            Ok(Some(v)) => {
                assert_ne!(
                    v.get("action").and_then(|a| a.as_str()),
                    Some(action),
                    "不应出现 action={action}: {v}"
                );
            }
            _ => return,
        }
    }
}

async fn expect_no_bot_message(bot: &mut MockBot, wait: Duration) {
    if let Ok(Some(m)) = timeout(wait, bot.rx.recv()).await {
        panic!("bot 不应收到消息: {:?}", m);
    }
}

#[tokio::test]
async fn single_upstream_group_gate_and_commands() {
    let (bot_uri, mut bot) = spawn_mock_bot().await;
    let cfg = test_config(
        &bot_uri,
        MessageConfig {
            mode: ON.into(),
            ..Default::default()
        },
        vec![],
    );
    let (app, addr) = spawn_gateway(cfg).await;
    wait_for_client(&app, "alpha").await;
    let mut up = connect_upstream(addr).await;

    // 第二个上游连接必须被拒绝
    let second = tokio_tungstenite::connect_async(format!("ws://{}/ws", addr)).await;
    assert!(second.is_err(), "第二个上游连接应被拒绝");

    // 群未启用：消息不转发
    send_json(&up, group_message(100, BOT_ID.parse().unwrap(), "hello"));
    expect_no_bot_message(&mut bot, Duration::from_millis(400)).await;

    // @bot /enable：superuser 指令，回包应带「已启用群聊」
    let enable = json!({
        "post_type": "message",
        "message_type": "group",
        "message_format": "array",
        "message_id": 1002,
        "user_id": BOT_ID.parse::<i64>().unwrap(),
        "group_id": 100,
        "raw_message": format!("[CQ:at,qq={}] /enable", BOT_ID),
        "message": [
            {"type": "at", "data": {"qq": BOT_ID}},
            {"type": "text", "data": {"text": "/enable"}}
        ],
    });
    send_json(&up, enable);
    let reply = recv_action(&mut up, "send_group_msg").await;
    assert!(reply.to_string().contains("已启用群聊"), "reply = {reply}");

    // 群已启用，消息应转发给 bot
    send_json(&up, group_message(100, BOT_ID.parse().unwrap(), "hello"));
    let forwarded = timeout(Duration::from_secs(3), bot.rx.recv())
        .await
        .expect("bot 应收到转发消息")
        .expect("channel");
    let v: Value = match forwarded {
        Message::Text(t) => serde_json::from_str(t.as_str()).unwrap(),
        other => panic!("意外消息: {:?}", other),
    };
    assert_eq!(v["message"][0]["data"]["text"], "hello");

    // /bot status 与 /bot 列表
    send_json(
        &up,
        group_message(100, BOT_ID.parse().unwrap(), "/bot status"),
    );
    let status = recv_action(&mut up, "send_group_msg").await;
    let text = status.to_string();
    assert!(text.contains("「OneBotFilter」"), "status = {text}");
    assert!(text.contains("BOT-Apps"), "status = {text}");
    send_json(&up, group_message(100, BOT_ID.parse().unwrap(), "/bot"));
    let list = recv_action(&mut up, "send_group_msg").await;
    assert!(list.to_string().contains("alpha"), "list = {list}");
}

#[tokio::test]
async fn filters_and_prefix_replace() {
    let (bot_uri, mut bot) = spawn_mock_bot().await;
    let cfg = test_config(
        &bot_uri,
        MessageConfig {
            mode: BLACKLIST.into(),
            filters: vec!["^/".into()],
            prefix: vec!["#".into()],
            prefix_replace: "/".into(),
        },
        vec![],
    );
    let (app, addr) = spawn_gateway(cfg).await;
    app.store.enable_group(100).unwrap();
    wait_for_client(&app, "alpha").await;
    let up = connect_upstream(addr).await;

    // 命中黑名单：不转发
    send_json(&up, group_message(100, 111, "/help"));
    expect_no_bot_message(&mut bot, Duration::from_millis(400)).await;

    // 未命中：转发
    send_json(&up, group_message(100, 111, "hello"));
    let forwarded = timeout(Duration::from_secs(3), bot.rx.recv())
        .await
        .expect("应转发")
        .expect("channel");
    assert!(forwarded.to_string().contains("hello"));

    // 前缀命中：放行且前缀被替换
    send_json(&up, group_message(100, 111, "#help"));
    let forwarded = timeout(Duration::from_secs(3), bot.rx.recv())
        .await
        .expect("前缀消息应放行")
        .expect("channel");
    let v: Value = match forwarded {
        Message::Text(t) => serde_json::from_str(t.as_str()).unwrap(),
        other => panic!("意外消息: {:?}", other),
    };
    assert_eq!(v["message"][0]["data"]["text"], "/help");

    // 黑用户：直接丢弃
    app.store.add_black_user(222).unwrap();
    send_json(&up, group_message(100, 222, "hello"));
    expect_no_bot_message(&mut bot, Duration::from_millis(400)).await;
}

#[tokio::test]
async fn rewrite_scrubs_echo_and_drops_self_report() {
    let (bot_uri, mut bot) = spawn_mock_bot().await;
    let cfg = test_config(
        &bot_uri,
        MessageConfig {
            mode: ON.into(),
            ..Default::default()
        },
        vec![RewriteTextRule {
            original: r"v[0-9]+\.[0-9]+\.[0-9]+".into(),
            replace: "v1.0.0".into(),
        }],
    );
    let (app, addr) = spawn_gateway(cfg).await;
    app.store.enable_group(100).unwrap();
    wait_for_client(&app, "alpha").await;
    let mut up = connect_upstream(addr).await;

    // bot 发言：上游应收到改写后的文本
    bot.tx
        .send(text_message(
            r#"{"action":"send_group_msg","params":{"group_id":100,"message":"客户端 v9.9.9"},"echo":"bot-echo"}"#,
        ))
        .unwrap();
    let sent = recv_action(&mut up, "send_group_msg").await;
    assert!(sent.to_string().contains("客户端 v1.0.0"), "sent = {sent}");
    assert!(!sent.to_string().contains("v9.9.9"), "sent = {sent}");

    // 上游回包：正文应被剥掉，只留 message_id
    send_json(
        &up,
        json!({
            "status": "ok",
            "retcode": 0,
            "data": {
                "message_id": 777,
                "message": [{"type": "text", "data": {"text": "客户端 v9.9.9"}}]
            },
            "echo": "bot-echo",
        }),
    );
    let resp = timeout(Duration::from_secs(3), bot.rx.recv())
        .await
        .expect("bot 应收到回包")
        .expect("channel");
    let v: Value = match resp {
        Message::Text(t) => serde_json::from_str(t.as_str()).unwrap(),
        other => panic!("意外消息: {:?}", other),
    };
    assert_eq!(v["data"]["message_id"], 777);
    assert!(v["data"].get("message").is_none(), "正文应被抹除: {v}");

    // 改写后的自身上报应被丢弃（上游不再收到）
    send_json(
        &up,
        json!({
            "post_type": "message_sent",
            "message_type": "group",
            "message_format": "array",
            "message_id": 777,
            "self_id": BOT_ID.parse::<i64>().unwrap(),
            "user_id": BOT_ID.parse::<i64>().unwrap(),
            "group_id": 100,
            "raw_message": "客户端 v1.0.0",
            "message": [{"type": "text", "data": {"text": "客户端 v1.0.0"}}],
        }),
    );
    expect_no_action(&mut up, "send_group_msg", Duration::from_millis(600)).await;
}
