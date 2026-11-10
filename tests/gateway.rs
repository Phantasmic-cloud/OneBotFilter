//! 进程内集成测试：mock OneBot 上游 + mock bot 下游。
//!
//! 覆盖：群/私聊闸门、名单模式、内置指令（含 private 参数）、出站改写与回包抹除、
//! db.json 无用状态清理、单上游并发限制。

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
    BotAppsConfig, BotMessageConfig, Config, MessageConfig, ModeConfig, RewriteTextRule,
    ServerConfig,
};
use onebotfilter::global::{App, BLACKLIST, ON, WHITELIST};
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

fn base_config(bot_uri: &str) -> Config {
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
            mode: ModeConfig {
                group: BLACKLIST.into(),
                private: WHITELIST.into(),
            },
            message: BotMessageConfig {
                group: MessageConfig {
                    mode: ON.into(),
                    ..Default::default()
                },
                private: MessageConfig {
                    mode: ON.into(),
                    ..Default::default()
                },
            },
            ..Default::default()
        }],
    }
}

async fn spawn_gateway(mut cfg: Config, initial_db: Option<&str>) -> (Arc<App>, SocketAddr) {
    cfg.check().expect("测试配置应合法");
    let db_path = temp_db_path();
    if let Some(db) = initial_db {
        std::fs::write(&db_path, db).unwrap();
    }
    let store = Store::load(&db_path, &cfg).unwrap();
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
                                        let action = v.get("action").and_then(|a| a.as_str()).unwrap_or("");
                                        let data = match action {
                                            "get_group_info" => json!({"group_id": 100, "group_name": "测试群"}),
                                            "get_group_list" => json!([
                                                {"group_id": 100, "group_name": "测试群"},
                                                {"group_id": 200, "group_name": "测试群200"}
                                            ]),
                                            "get_login_info" => json!({"user_id": 123456789, "nickname": "tester"}),
                                            _ => json!({}),
                                        };
                                        let resp = json!({
                                            "status": "ok",
                                            "retcode": 0,
                                            "data": data,
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

fn private_message(user_id: i64, text: &str) -> Value {
    json!({
        "post_type": "message",
        "message_type": "private",
        "message_format": "array",
        "message_id": 2001,
        "user_id": user_id,
        "raw_message": text,
        "message": [{"type": "text", "data": {"text": text}}],
    })
}

/// 带 @bot 的指令消息（enable/disable 需要 @）。
fn at_command(group_id: i64, user_id: i64, text: &str) -> Value {
    json!({
        "post_type": "message",
        "message_type": "group",
        "message_format": "array",
        "message_id": 1002,
        "user_id": user_id,
        "group_id": group_id,
        "raw_message": format!("[CQ:at,qq={}] {}", BOT_ID, text),
        "message": [
            {"type": "at", "data": {"qq": BOT_ID}},
            {"type": "text", "data": {"text": text}}
        ],
    })
}

async fn recv_action(up: &mut MockUpstream, action: &str) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
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

async fn recv_reply_text(up: &mut MockUpstream) -> String {
    let v = recv_action(up, "send_group_msg").await;
    v["params"]["message"]
        .as_array()
        .map(|segs| {
            segs.iter()
                .filter_map(|s| {
                    if s["type"] == "text" {
                        s["data"]["text"].as_str().map(|t| t.to_string())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

async fn recv_bot_message(bot: &mut MockBot, wait: Duration) -> Option<Value> {
    match timeout(wait, bot.rx.recv()).await {
        Ok(Some(Message::Text(t))) => serde_json::from_str(t.as_str()).ok(),
        _ => None,
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
    let cfg = base_config(&bot_uri);
    let (app, addr) = spawn_gateway(cfg, None).await;
    wait_for_client(&app, "alpha").await;
    let mut up = connect_upstream(addr).await;

    // 第二个上游连接必须被拒绝
    let second = tokio_tungstenite::connect_async(format!("ws://{}/ws", addr)).await;
    assert!(second.is_err(), "第二个上游连接应被拒绝");

    // 群未启用：消息不转发
    send_json(&up, group_message(100, 123, "hello"));
    expect_no_bot_message(&mut bot, Duration::from_millis(400)).await;

    // @bot /enable：superuser 指令，回包应带「已启用群聊」
    send_json(&up, at_command(100, 123456789, "/enable"));
    let reply = recv_reply_text(&mut up).await;
    assert!(reply.contains("已启用群聊"), "reply = {reply}");

    // 群已启用，消息应转发给 bot
    send_json(&up, group_message(100, 123, "hello"));
    let forwarded = recv_bot_message(&mut bot, Duration::from_secs(3))
        .await
        .expect("bot 应收到转发消息");
    assert_eq!(forwarded["message"][0]["data"]["text"], "hello");

    // /bot status 与 /bot 列表
    send_json(&up, group_message(100, 123456789, "/bot status"));
    let status = recv_reply_text(&mut up).await;
    assert!(status.contains("「OneBotFilter」"), "status = {status}");
    assert!(status.contains("BOT-Apps"), "status = {status}");
    send_json(&up, group_message(100, 123456789, "/bot"));
    let list = recv_reply_text(&mut up).await;
    assert!(list.contains("alpha"), "list = {list}");
}

#[tokio::test]
async fn filters_and_prefix_replace() {
    let (bot_uri, mut bot) = spawn_mock_bot().await;
    let mut cfg = base_config(&bot_uri);
    cfg.bot_apps[0].message.group = MessageConfig {
        mode: BLACKLIST.into(),
        filters: vec!["^/".into()],
        prefix: vec!["#".into()],
        prefix_replace: "/".into(),
    };
    let (app, addr) = spawn_gateway(cfg, None).await;
    app.store.enable_group(100).unwrap();
    wait_for_client(&app, "alpha").await;
    let up = connect_upstream(addr).await;

    // 命中黑名单：不转发
    send_json(&up, group_message(100, 111, "/help"));
    expect_no_bot_message(&mut bot, Duration::from_millis(400)).await;

    // 未命中：转发
    send_json(&up, group_message(100, 111, "hello"));
    let forwarded = recv_bot_message(&mut bot, Duration::from_secs(3))
        .await
        .expect("应转发");
    assert_eq!(forwarded["message"][0]["data"]["text"], "hello");

    // 前缀命中：放行且前缀被替换
    send_json(&up, group_message(100, 111, "#help"));
    let forwarded = recv_bot_message(&mut bot, Duration::from_secs(3))
        .await
        .expect("前缀消息应放行");
    assert_eq!(forwarded["message"][0]["data"]["text"], "/help");

    // 黑用户：直接丢弃
    app.store.add_black_user(222).unwrap();
    send_json(&up, group_message(100, 222, "hello"));
    expect_no_bot_message(&mut bot, Duration::from_millis(400)).await;
}

#[tokio::test]
async fn rewrite_scrubs_echo_and_drops_self_report() {
    let (bot_uri, mut bot) = spawn_mock_bot().await;
    let mut cfg = base_config(&bot_uri);
    cfg.bot_apps[0].rewrite_text = vec![RewriteTextRule {
        original: r"v[0-9]+\.[0-9]+\.[0-9]+".into(),
        replace: "v1.0.0".into(),
    }];
    let (app, addr) = spawn_gateway(cfg, None).await;
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
    let resp = recv_bot_message(&mut bot, Duration::from_secs(3))
        .await
        .expect("bot 应收到回包");
    assert_eq!(resp["data"]["message_id"], 777);
    assert!(
        resp["data"].get("message").is_none(),
        "正文应被抹除: {resp}"
    );

    // 改写后的自身上报应被丢弃（上游不再收到）
    send_json(
        &up,
        json!({
            "post_type": "message_sent",
            "message_type": "group",
            "message_format": "array",
            "message_id": 777,
            "self_id": 123456789,
            "user_id": 123456789,
            "group_id": 100,
            "raw_message": "客户端 v1.0.0",
            "message": [{"type": "text", "data": {"text": "客户端 v1.0.0"}}],
        }),
    );
    if let Ok(Some(v)) = timeout(Duration::from_millis(600), up.rx.recv()).await {
        panic!("自身上报不应再转发: {v}");
    }
}

#[tokio::test]
async fn private_messages_require_global_enable_and_list() {
    let (bot_uri, mut bot) = spawn_mock_bot().await;
    let cfg = base_config(&bot_uri);
    let (app, addr) = spawn_gateway(cfg, None).await;
    wait_for_client(&app, "alpha").await;
    let mut up = connect_upstream(addr).await;
    let user: i64 = 123456789;

    // 默认：私聊用户不在全局白名单，不转发
    send_json(&up, private_message(user, "hi"));
    expect_no_bot_message(&mut bot, Duration::from_millis(400)).await;

    // @bot /enable <qq> private：全局私聊白名单
    send_json(
        &up,
        at_command(100, user, &format!("/enable {} private", user)),
    );
    let reply = recv_reply_text(&mut up).await;
    assert!(reply.contains("已启用私聊用户"), "reply = {reply}");

    // 全局白名单通过，但 per-bot 私聊名单是 whitelist 且为空 → 仍不转发
    send_json(&up, private_message(user, "hi"));
    expect_no_bot_message(&mut bot, Duration::from_millis(400)).await;

    // /alpha on <qq> private：per-bot 私聊名单
    send_json(
        &up,
        group_message(100, user, &format!("/alpha on {} private", user)),
    );
    let reply = recv_reply_text(&mut up).await;
    assert!(reply.contains("已允许"), "reply = {reply}");

    // 现在应转发
    send_json(&up, private_message(user, "hi"));
    let forwarded = recv_bot_message(&mut bot, Duration::from_secs(3))
        .await
        .expect("私聊应转发");
    assert_eq!(forwarded["message"][0]["data"]["text"], "hi");

    // /disable <qq> private 关闭全局白名单后不再转发
    send_json(
        &up,
        at_command(100, user, &format!("/disable {} private", user)),
    );
    let reply = recv_reply_text(&mut up).await;
    assert!(reply.contains("已禁用私聊用户"), "reply = {reply}");
    send_json(&up, private_message(user, "hi"));
    expect_no_bot_message(&mut bot, Duration::from_millis(400)).await;
}

#[tokio::test]
async fn private_message_filter_and_black_user() {
    let (bot_uri, mut bot) = spawn_mock_bot().await;
    let mut cfg = base_config(&bot_uri);
    cfg.bot_apps[0].message.private = MessageConfig {
        mode: BLACKLIST.into(),
        filters: vec!["^secret".into()],
        ..Default::default()
    };
    let (app, addr) = spawn_gateway(cfg, None).await;
    app.store.enable_private_user(123456789).unwrap();
    app.store
        .set_bot_private_user("alpha", WHITELIST, 123456789, true)
        .unwrap();
    wait_for_client(&app, "alpha").await;
    let up = connect_upstream(addr).await;

    // 命中私聊内容黑名单：不转发
    send_json(&up, private_message(123456789, "secret 内容"));
    expect_no_bot_message(&mut bot, Duration::from_millis(400)).await;

    // 未命中：转发
    send_json(&up, private_message(123456789, "hello"));
    let forwarded = recv_bot_message(&mut bot, Duration::from_secs(3))
        .await
        .expect("私聊应转发");
    assert_eq!(forwarded["message"][0]["data"]["text"], "hello");

    // 全局黑用户优先：即使白名单/名单都通过也不转发
    app.store.add_black_user(123456789).unwrap();
    send_json(&up, private_message(123456789, "hello"));
    expect_no_bot_message(&mut bot, Duration::from_millis(400)).await;
}

#[tokio::test]
async fn private_queries_and_commands() {
    let (bot_uri, mut bot) = spawn_mock_bot().await;
    let cfg = base_config(&bot_uri);
    let (app, addr) = spawn_gateway(cfg, None).await;
    app.store.enable_private_user(123456789).unwrap();
    wait_for_client(&app, "alpha").await;
    let mut up = connect_upstream(addr).await;

    // /group status private
    send_json(&up, group_message(100, 123456789, "/group status private"));
    let text = recv_reply_text(&mut up).await;
    assert!(text.contains("已启用的私聊用户"), "text = {text}");
    assert!(text.contains("123456789"), "text = {text}");

    // /group alpha private
    send_json(&up, group_message(100, 123456789, "/group alpha private"));
    let text = recv_reply_text(&mut up).await;
    assert!(text.contains("私聊"), "text = {text}");
    assert!(text.contains("名单模式"), "text = {text}");

    // 非 superuser 执行管理指令：静默
    send_json(&up, group_message(100, 999, "/alpha on 123456789 private"));
    if let Ok(Some(v)) = timeout(Duration::from_millis(400), up.rx.recv()).await {
        panic!("非 superuser 不应收到指令回复: {v}");
    }
    let _ = &mut bot;
}

#[tokio::test]
async fn command_targets_and_private_args() {
    let (bot_uri, mut bot) = spawn_mock_bot().await;
    let cfg = base_config(&bot_uri);
    let initial_db = r#"{
        "enabled_groups": [100],
        "enabled_private_users": [],
        "black_users": [],
        "bots": {"alpha": {"groups": [], "users": []}},
        "group_names": {"100": "本群", "200": "测试群200"},
        "joined_groups": [100, 200]
    }"#;
    let (app, addr) = spawn_gateway(cfg, Some(initial_db)).await;
    wait_for_client(&app, "alpha").await;
    let mut up = connect_upstream(addr).await;

    // /alpha off 200：显式指定已加入的群，改的是 200 而不是当前群
    send_json(&up, group_message(100, 123456789, "/alpha off 200"));
    let reply = recv_reply_text(&mut up).await;
    assert!(reply.contains("200"), "reply = {reply}");
    assert!(
        !app.store.is_bot_group_allowed("alpha", BLACKLIST, 200),
        "群 200 应对 alpha 关闭"
    );
    assert!(
        app.store.is_bot_group_allowed("alpha", BLACKLIST, 100),
        "当前群不应受影响"
    );

    // 缺 qq 的私聊开关：提醒缺参，且不能当普通消息转给 bot
    send_json(&up, at_command(100, 123456789, "/enable private"));
    let reply = recv_reply_text(&mut up).await;
    assert!(reply.contains("缺少 QQ 号"), "reply = {reply}");
    send_json(&up, group_message(100, 123456789, "/alpha on private"));
    let reply = recv_reply_text(&mut up).await;
    assert!(reply.contains("缺少 QQ 号"), "reply = {reply}");
    expect_no_bot_message(&mut bot, Duration::from_millis(300)).await;

    // /alpha off 999：群未加入 → 解析成当前群
    send_json(&up, group_message(100, 123456789, "/alpha off 999"));
    let reply = recv_reply_text(&mut up).await;
    assert!(reply.contains("本群"), "reply = {reply}");
    assert!(
        !app.store.is_bot_group_allowed("alpha", BLACKLIST, 100),
        "未加入的群号应回退到当前群"
    );

    // /alpha on 777 private：数字 + private → 改私聊名单
    send_json(&up, group_message(100, 123456789, "/alpha on 777 private"));
    let reply = recv_reply_text(&mut up).await;
    assert!(reply.contains("已允许"), "reply = {reply}");
    assert!(app.store.is_bot_private_allowed("alpha", WHITELIST, 777));

    // /enable 200：显式群号开启
    send_json(&up, at_command(100, 123456789, "/enable 200"));
    let reply = recv_reply_text(&mut up).await;
    assert!(reply.contains("已启用群聊"), "reply = {reply}");
    assert!(reply.contains("200"), "reply = {reply}");
    assert!(app.store.is_group_enabled(200));
}

#[tokio::test]
async fn prunes_stale_runtime_state() {
    let (bot_uri, _bot) = spawn_mock_bot().await;
    // 初始 db 里塞入不存在的群 / bot 群名单 / 群名
    let initial_db = r#"{
        "enabled_groups": [100, 999],
        "enabled_private_users": [],
        "black_users": [42],
        "bots": {"alpha": {"groups": [999], "users": []}},
        "group_names": {"999": "旧群"},
        "joined_groups": [100, 999]
    }"#;
    let cfg = base_config(&bot_uri);
    let (app, addr) = spawn_gateway(cfg, Some(initial_db)).await;
    wait_for_client(&app, "alpha").await;
    let _up = connect_upstream(addr).await;
    // 上游连接 500ms 后会拉 get_group_list（mock 返回 [100, 200]）
    tokio::time::sleep(Duration::from_millis(1200)).await;

    assert!(app.store.is_group_enabled(100), "存在的群应保留启用状态");
    assert!(!app.store.is_group_enabled(999), "不存在的群应被清理");
    assert!(
        app.store.snapshot_bot_groups("alpha").is_empty(),
        "bot 群名单应被清理"
    );
    assert_eq!(app.store.get_group_name(999), "", "旧群名应被清理");
    assert!(app.store.is_black_user(42), "全局黑用户不应被清理");
}

#[tokio::test]
async fn rejects_concurrent_second_upstream() {
    let (bot_uri, _bot) = spawn_mock_bot().await;
    let cfg = base_config(&bot_uri);
    let (_app, addr) = spawn_gateway(cfg, None).await;
    wait_for_client(&_app, "alpha").await;

    let url = format!("ws://{}/ws", addr);
    let (a, b) = tokio::join!(
        tokio_tungstenite::connect_async(url.clone()),
        tokio_tungstenite::connect_async(url)
    );
    let ok_count = [&a, &b].iter().filter(|r| r.is_ok()).count();
    assert_eq!(ok_count, 1, "并发连接应恰好成功一个");
}
