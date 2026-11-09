//! 下游 bot 应用连接（对应 Go 版 `client.go`）。

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;

use crate::config::BotAppsConfig;
use crate::filters::Filter;
use crate::global::{App, Client, ClientWrite, LatencyRequest, UpstreamWrite};
use crate::stats::is_outbound_send_action;

type PongWaiters = Arc<Mutex<Vec<oneshot::Sender<Instant>>>>;

/// 连接失败后按 `sleep-time` 重试；建连后再断则固定 5s 重连。
pub async fn ws_client_handler(app: Arc<App>, mut cfg: BotAppsConfig) {
    if let Err(e) = cfg.check() {
        log::error!("{}的配置有问题: {}", cfg.name, e);
        return;
    }
    let filter = Arc::new(parking_lot::RwLock::new(Filter::new(&cfg.name)));
    filter.write().compile(&cfg);
    app.add_filter(filter.clone());
    app.get_or_create_stat(&cfg.name);

    loop {
        log::info!("正在连接：{}", cfg.name);
        let ws = match connect(&app, &cfg).await {
            Ok(ws) => ws,
            Err(e) => {
                log::error!("连接{}异常: {}", cfg.name, e);
                let sleep = app.config().server.sleep_time.max(0.0);
                tokio::time::sleep(Duration::from_secs_f32(sleep)).await;
                continue;
            }
        };
        run_connection(app.clone(), cfg.clone(), filter.clone(), ws).await;
        // run_connection 内部已经 sleep 5s 并清理，回到外层立即重连
    }
}

async fn connect(
    app: &App,
    cfg: &BotAppsConfig,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    String,
> {
    let mut req = cfg
        .uri
        .clone()
        .into_client_request()
        .map_err(|e| e.to_string())?;
    {
        let headers = req.headers_mut();
        let server = app.config();
        headers.insert(
            "x-self-id",
            HeaderValue::from_str(&server.server.bot_id).unwrap_or(HeaderValue::from_static("")),
        );
        headers.insert(
            "authorization",
            HeaderValue::from_str(&format!("Bearer {}", cfg.access_token))
                .unwrap_or(HeaderValue::from_static("")),
        );
        headers.insert(
            "user-agent",
            HeaderValue::from_str(&server.server.user_agent)
                .unwrap_or(HeaderValue::from_static("OneBotFilter")),
        );
        headers.insert("x-client-role", HeaderValue::from_static("Universal"));
    }
    let buffer = app.config().server.buffer_size.max(1) as usize;
    let config = WebSocketConfig::default()
        .read_buffer_size(buffer.max(128))
        .write_buffer_size(buffer.max(128))
        .max_message_size(None)
        .max_frame_size(None);
    let (ws, _) = tokio_tungstenite::connect_async_with_config(req, Some(config), false)
        .await
        .map_err(|e| e.to_string())?;
    Ok(ws)
}

async fn run_connection(
    app: Arc<App>,
    cfg: BotAppsConfig,
    filter: Arc<parking_lot::RwLock<Filter>>,
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) {
    let name = cfg.name.clone();
    let (mut sink, mut reader) = ws.split();
    let (write_tx, mut write_rx) = mpsc::unbounded_channel::<ClientWrite>();
    let (latency_tx, mut latency_rx) = mpsc::unbounded_channel::<LatencyRequest>();
    let pong_waiters: PongWaiters = Arc::new(Mutex::new(Vec::new()));

    let client = Arc::new(Client {
        name: name.clone(),
        filter: filter.clone(),
        tx: write_tx.clone(),
        latency: latency_tx,
    });
    if !app.add_client(client) {
        log::error!("连接{}异常: 已经连接过{}", name, name);
        return;
    }
    app.reset_bot_traffic(&name);
    app.mark_bot_connected(&name);

    let app_w = app.clone();
    let name_w = name.clone();
    let writer = tokio::spawn(async move {
        while let Some(w) = write_rx.recv().await {
            if let Err(e) = sink.send(w.msg).await {
                log::error!("向{}发送消息出错：{}", name_w, e);
                break;
            }
            if w.inc_bot_recv {
                app_w.inc_bot_recv(&name_w);
            }
        }
    });

    let app_p = app.clone();
    let name_p = name.clone();
    let tx_p = write_tx.clone();
    let waiters_p = pong_waiters.clone();
    let ping = tokio::spawn(async move {
        let _ = measure_latency(&app_p, &name_p, &tx_p, &waiters_p).await;
        let mut ticker = tokio::time::interval(Duration::from_secs(30));
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let _ = measure_latency(&app_p, &name_p, &tx_p, &waiters_p).await;
                }
                Some(req) = latency_rx.recv() => {
                    let res = measure_latency(&app_p, &name_p, &tx_p, &waiters_p).await;
                    let _ = req.send(res);
                }
                else => break,
            }
        }
    });

    log::info!("已连接到：{}，加载的过滤器：{}", name, filter.read());

    let err_msg = loop {
        match reader.next().await {
            Some(Ok(Message::Text(t))) => {
                let raw = t.as_str().as_bytes().to_vec();
                handle_bot_frame(&app, &name, &filter, &write_tx, &raw, true);
            }
            Some(Ok(Message::Binary(b))) => {
                let raw = b.as_ref().to_vec();
                handle_bot_frame(&app, &name, &filter, &write_tx, &raw, false);
            }
            Some(Ok(Message::Ping(p))) => {
                let _ = write_tx.send(ClientWrite {
                    msg: Message::Pong(p),
                    inc_bot_recv: false,
                });
            }
            Some(Ok(Message::Pong(_))) => {
                let mut waiters = pong_waiters.lock();
                for w in waiters.drain(..) {
                    let _ = w.send(Instant::now());
                }
            }
            Some(Ok(Message::Close(frame))) => {
                break Some(match frame {
                    Some(f) => format!("websocket closed: {:?}", f.code),
                    None => "websocket closed".to_string(),
                });
            }
            Some(Ok(Message::Frame(_))) => {}
            Some(Err(e)) => break Some(e.to_string()),
            None => break Some("connection closed".to_string()),
        }
    };

    if let Some(e) = &err_msg {
        log::error!("从{}读取消息出错：{}", name, e);
    }
    writer.abort();
    ping.abort();
    app.remove_client(&name);
    app.mark_bot_disconnected(&name);
    app.reset_bot_traffic(&name);
    tokio::time::sleep(Duration::from_secs(5)).await;
}

fn handle_bot_frame(
    app: &Arc<App>,
    name: &str,
    filter: &Arc<parking_lot::RwLock<Filter>>,
    write_tx: &mpsc::UnboundedSender<ClientWrite>,
    raw: &[u8],
    is_text: bool,
) {
    let upstream_tx = match app.upstream.read().tx.clone() {
        Some(tx) => tx,
        None => {
            log::error!("写入到OneBot客户端出错：没有连接到OneBot客户端");
            return;
        }
    };

    if is_text && is_outbound_send_action(raw) {
        let data = {
            let f = filter.read();
            f.rewrite_outbound(app, raw).unwrap_or_else(|| raw.to_vec())
        };
        let _ = upstream_tx.send(UpstreamWrite {
            msg: Message::Text(String::from_utf8_lossy(&data).into_owned().into()),
            inc_onebot_send: true,
            inc_bot_send: Some(name.to_string()),
        });
    } else {
        let msg = if is_text {
            Message::Text(String::from_utf8_lossy(raw).into_owned().into())
        } else {
            Message::Binary(raw.to_vec().into())
        };
        let _ = upstream_tx.send(UpstreamWrite {
            msg,
            inc_onebot_send: false,
            inc_bot_send: None,
        });
    }
    let _ = write_tx;
}

async fn measure_latency(
    app: &App,
    name: &str,
    tx: &mpsc::UnboundedSender<ClientWrite>,
    waiters: &PongWaiters,
) -> Result<Duration, String> {
    let (otx, orx) = oneshot::channel();
    waiters.lock().push(otx);
    if tx
        .send(ClientWrite {
            msg: Message::Ping(Vec::new().into()),
            inc_bot_recv: false,
        })
        .is_err()
    {
        return Err("未连接".to_string());
    }
    let start = Instant::now();
    match timeout(Duration::from_secs(2), orx).await {
        Ok(Ok(_)) => {
            let d = start.elapsed();
            app.update_stat(name, |s| s.last_latency = d);
            Ok(d)
        }
        _ => Err("ping 超时".to_string()),
    }
}

/// 给指令层用：请求一次延迟测量。
pub async fn measure_client_latency(app: &Arc<App>, client: &Client) -> Option<Duration> {
    let (tx, rx) = oneshot::channel();
    if client.latency.send(tx).is_err() {
        return None;
    }
    match timeout(Duration::from_millis(2500), rx).await {
        Ok(Ok(Ok(d))) => {
            app.update_stat(&client.name, |s| s.last_latency = d);
            Some(d)
        }
        _ => None,
    }
}
