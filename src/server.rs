//! 上游 OneBot 连接（对应 Go 版 `server.go` + `main.go` 的 handleLocal）。
//!
//! 本程序对上游是服务端：只接受一个 WebSocket 连接，路径必须等于 `server.suffix`。

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::handshake::server::Request;
use tokio_tungstenite::tungstenite::http::{Response, StatusCode};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;

use crate::api;
use crate::dispatch::dispatch_incoming;
use crate::global::{App, UpstreamWrite};

fn error_response(status: StatusCode, body: &str) -> Response<Option<String>> {
    let mut resp = Response::new(Some(body.to_string()));
    *resp.status_mut() = status;
    resp
}

fn ws_config(app: &App) -> WebSocketConfig {
    let buffer = app.config().server.buffer_size.max(1) as usize;
    WebSocketConfig::default()
        .read_buffer_size(buffer.max(128))
        .write_buffer_size(buffer.max(128))
        // Go 版 gorilla 不限制消息大小，这里同样放开
        .max_message_size(None)
        .max_frame_size(None)
}

pub async fn accept_loop(app: Arc<App>, listener: TcpListener) -> anyhow::Result<()> {
    loop {
        let (stream, _peer) = listener.accept().await?;
        let app = app.clone();
        tokio::spawn(async move {
            handle_connection(app, stream).await;
        });
    }
}

#[allow(clippy::result_large_err)]
async fn handle_connection(app: Arc<App>, stream: tokio::net::TcpStream) {
    let suffix = app.config().server.suffix.clone();
    let app_cb = app.clone();
    let attach = tokio_tungstenite::accept_hdr_async_with_config(
        stream,
        move |req: &Request, resp: Response<()>| {
            if req.uri().path() != suffix {
                return Err(error_response(
                    StatusCode::NOT_FOUND,
                    "404 page not found\n",
                ));
            }
            if app_cb.upstream.read().tx.is_some() {
                return Err(error_response(
                    StatusCode::FORBIDDEN,
                    "只能连接一个OneBot客户端\n",
                ));
            }
            Ok(resp)
        },
        Some(ws_config(&app)),
    )
    .await;

    let ws = match attach {
        Ok(w) => w,
        // 被拒绝的连接（403/404）与 Go 版一样安静返回
        Err(_) => return,
    };
    log::info!("已连接到OneBot客户端");

    let (write_tx, mut write_rx) = tokio::sync::mpsc::unbounded_channel::<UpstreamWrite>();
    {
        let mut upstream = app.upstream.write();
        upstream.tx = Some(write_tx.clone());
        upstream.connected_since = Some(Instant::now());
        upstream.connected_at = Some(SystemTime::now());
        upstream.pending.clear();
    }
    app.reset_onebot_traffic();

    let (mut sink, mut reader) = ws.split();

    let app_w = app.clone();
    let writer = tokio::spawn(async move {
        while let Some(w) = write_rx.recv().await {
            if sink.send(w.msg).await.is_err() {
                break;
            }
            if w.inc_onebot_send {
                app_w.inc_onebot_send();
            }
            if let Some(name) = w.inc_bot_send {
                app_w.inc_bot_send(&name);
            }
        }
    });

    let app_refresh = app.clone();
    let refresh = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        api::refresh_joined_groups(&app_refresh).await;
    });

    let close_reason = loop {
        match reader.next().await {
            Some(Ok(Message::Text(t))) => {
                dispatch_incoming(&app, true, t.as_str().as_bytes());
            }
            Some(Ok(Message::Binary(b))) => {
                dispatch_incoming(&app, false, b.as_ref());
            }
            Some(Ok(Message::Ping(p))) => {
                let _ = write_tx.send(UpstreamWrite {
                    msg: Message::Pong(p),
                    inc_onebot_send: false,
                    inc_bot_send: None,
                });
            }
            Some(Ok(Message::Pong(_))) | Some(Ok(Message::Frame(_))) => {}
            Some(Ok(Message::Close(frame))) => {
                break Some(match frame {
                    Some(f) => format!("websocket closed: {:?}", f.code),
                    None => "websocket closed".to_string(),
                });
            }
            Some(Err(e)) => break Some(e.to_string()),
            None => break Some("connection closed".to_string()),
        }
    };

    if let Some(reason) = close_reason {
        log::error!("OneBot客户端连接异常：{}", reason);
    }
    refresh.abort();
    writer.abort();
    {
        let mut upstream = app.upstream.write();
        upstream.tx = None;
        upstream.connected_since = None;
        upstream.connected_at = None;
        upstream.pending.clear();
    }
    app.reset_onebot_traffic();
    log::info!("OneBot客户端连接已断开");
}
