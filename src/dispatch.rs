//! 入站分发与闸门（对应 Go 版 `dispatch.go`）。

use std::sync::Arc;

use tokio_tungstenite::tungstenite::Message;

use crate::api;
use crate::command;
use crate::global::{App, ClientWrite, GROUP, PRIVATE};
use crate::onebot_message::{parse, OneBotMessage};
use crate::rewrite::{
    is_rewritten_self_report, looks_like_api_response, sanitize_rewritten_api_response,
};

/// 上游读到的第一层分流。
pub fn dispatch_incoming(app: &Arc<App>, is_text: bool, raw: &[u8]) {
    if !is_text {
        broadcast_raw(app, false, raw);
        return;
    }

    // 本程序发起的 API 回包：不广播给 bot
    if api::try_deliver_api_response(app, raw) {
        return;
    }

    if looks_like_api_response(raw) {
        let sanitized = sanitize_rewritten_api_response(app, raw);
        broadcast_raw(app, true, &sanitized);
        return;
    }

    let msg = match parse(raw) {
        Some(m) => m,
        None => {
            broadcast_raw(app, true, raw);
            return;
        }
    };

    if is_rewritten_self_report(app, &msg) {
        if app.config().server.debug {
            log::info!(
                "dispatch: 丢弃改写后的自身上报 post_type={} message_id={}",
                msg.partial.post_type,
                msg.partial.message_id
            );
        }
        return;
    }

    // 账号真实收到的 message 事件（含私聊）计入 OneBot 收
    if msg.partial.message_type == GROUP || msg.partial.message_type == PRIVATE {
        let pt = msg.partial.post_type.as_str();
        if pt.is_empty() || pt == "message" {
            app.inc_onebot_recv();
        }
    }

    // 事件里若带 group_name 扩展，顺手缓存
    if msg.partial.group_id != 0 && !msg.partial.group_name.is_empty() {
        app.store
            .update_group_name_if_changed(msg.partial.group_id, &msg.partial.group_name);
    }

    // 带 group_id 的事件一律过闸；指令可能调 API，必须放独立任务
    if msg.partial.group_id != 0 {
        let app = app.clone();
        tokio::spawn(async move {
            handle_group_event(app, msg).await;
        });
        return;
    }

    // 无群号的 message / notice / request（私聊、好友戳一戳等）一律不转
    let pt = msg.partial.post_type.as_str();
    if pt == "message"
        || pt == "message_sent"
        || pt == "notice"
        || pt == "request"
        || msg.partial.message_type == PRIVATE
    {
        if app.config().server.debug {
            log::info!(
                "dispatch: 非群事件丢弃 post_type={} message_type={} user={}",
                pt,
                msg.partial.message_type,
                msg.partial.user_id
            );
        }
        return;
    }

    // meta_event、无群号的 API 回包等：原样广播（改写过的剥掉正文）
    let sanitized = sanitize_rewritten_api_response(app, raw);
    broadcast_raw(app, true, &sanitized);
}

async fn handle_group_event(app: Arc<App>, msg: OneBotMessage) {
    let handled = command::try_handle_command(app.clone(), &msg).await;
    if let Some(reply) = handled {
        if !reply.is_empty() {
            command::reply_group_text(
                app.clone(),
                msg.partial.group_id,
                msg.partial.message_id,
                &reply,
            )
            .await;
        }
        return;
    }

    if app.store.is_black_user(msg.partial.user_id) {
        if app.config().server.debug {
            log::info!("dispatch: 黑用户 {} 丢弃", msg.partial.user_id);
        }
        return;
    }

    if !app.store.is_group_enabled(msg.partial.group_id) {
        if app.config().server.debug {
            log::info!(
                "dispatch: 群 {} 未 enable，丢弃 post_type={}",
                msg.partial.group_id,
                msg.partial.post_type
            );
        }
        return;
    }

    let is_chat = msg.partial.message_type == GROUP || msg.partial.message_type == PRIVATE;

    for client in app.snapshot_clients() {
        let cfg = match app.find_bot_app(&client.name) {
            Some(c) => c,
            None => continue,
        };
        if !app
            .store
            .is_bot_allowed(&cfg.name, &cfg.group_id.mode, msg.partial.group_id)
        {
            if app.config().server.debug {
                log::info!(
                    "dispatch: bot {} 名单未通过 group={} post_type={}",
                    cfg.name,
                    msg.partial.group_id,
                    msg.partial.post_type
                );
            }
            continue;
        }
        if is_chat {
            let mut cloned = match parse(&msg.raw) {
                Some(c) => c,
                None => continue,
            };
            {
                let filter = client.filter.read();
                if !filter.filter_content(&mut cloned) {
                    if app.config().server.debug {
                        log::info!("dispatch: bot {} 内容过滤未通过", cfg.name);
                    }
                    continue;
                }
            }
            let data = match serde_json::to_vec(&cloned.intact) {
                Ok(d) => d,
                Err(e) => {
                    log::error!("dispatch: 序列化失败 {}: {}", cfg.name, e);
                    continue;
                }
            };
            let _ = client.tx.send(ClientWrite {
                msg: Message::Text(String::from_utf8_lossy(&data).into_owned().into()),
                inc_bot_recv: true,
            });
        } else {
            // notice / request 等：过闸后原样转发，不计 bot 收
            let _ = client.tx.send(ClientWrite {
                msg: text_message(&msg.raw),
                inc_bot_recv: false,
            });
        }
    }
}

fn text_message(raw: &[u8]) -> Message {
    Message::Text(String::from_utf8_lossy(raw).into_owned().into())
}

fn broadcast_raw(app: &Arc<App>, is_text: bool, raw: &[u8]) {
    for client in app.snapshot_clients() {
        let msg = if is_text {
            text_message(raw)
        } else {
            Message::Binary(raw.to_vec().into())
        };
        let _ = client.tx.send(ClientWrite {
            msg,
            inc_bot_recv: false,
        });
    }
}
