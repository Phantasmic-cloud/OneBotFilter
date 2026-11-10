//! 入站分发与闸门。
//!
//! 群聊：指令 → 群启用 → 黑用户 → per-bot 群名单 → message.group → 转发
//! 私聊：全局私聊白名单 → 黑用户 → per-bot 私聊名单 → message.private → 转发
//!
//! 群聊/私聊共用 `gate` 与 `deliver_to_bots`，只有“全局开关”和“名单”不同。

use std::sync::Arc;

use tokio_tungstenite::tungstenite::Message;

use crate::api;
use crate::command;
use crate::gate::{self, ChatKind};
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

    // 群 / 私聊 message 统一走闸门
    if let Some(kind) = gate::classify(&msg) {
        match kind {
            ChatKind::Group { .. } => {
                let app = app.clone();
                tokio::spawn(async move {
                    handle_group_event(app, msg).await;
                });
            }
            ChatKind::Private { .. } => {
                // 私聊流程不会调用 API，直接在当前任务处理，避免无意义的 spawn。
                handle_private_event(app, &msg);
            }
        }
        return;
    }

    // 无群号的 message / notice / request（好友戳一戳、私聊回执等）一律不转
    let pt = msg.partial.post_type.as_str();
    if pt == "message"
        || pt == "message_sent"
        || pt == "notice"
        || pt == "request"
        || msg.partial.message_type == PRIVATE
    {
        if app.config().server.debug {
            log::info!(
                "dispatch: 非群/非私聊消息事件丢弃 post_type={} message_type={} user={}",
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
            );
        }
        return;
    }

    let kind = ChatKind::Group {
        group_id: msg.partial.group_id,
    };
    match gate::global_gate(&app, kind, msg.partial.user_id) {
        Ok(()) => {}
        Err(reason) => {
            if app.config().server.debug {
                log::info!(
                    "dispatch: 丢弃（{}）group={}",
                    reason.as_str(),
                    msg.partial.group_id
                );
            }
            return;
        }
    }
    deliver_to_bots(&app, &msg, kind);
}

fn handle_private_event(app: &Arc<App>, msg: &OneBotMessage) {
    let kind = ChatKind::Private {
        user_id: msg.partial.user_id,
    };
    match gate::global_gate(app, kind, msg.partial.user_id) {
        Ok(()) => {}
        Err(reason) => {
            if app.config().server.debug {
                log::info!(
                    "dispatch: 丢弃（{}）private user={}",
                    reason.as_str(),
                    msg.partial.user_id
                );
            }
            return;
        }
    }
    deliver_to_bots(app, msg, kind);
}

/// 把一条已通过全局闸门的消息，按每个 bot 的名单与内容过滤投递出去。
///
/// 优化点：消息只 clone（不再每 bot 重新 parse）；内容过滤没有改写时直接透传
/// 原始字节，避免每 bot 重新序列化。
fn deliver_to_bots(app: &Arc<App>, msg: &OneBotMessage, kind: ChatKind) {
    let cfg = app.config();
    let debug = cfg.server.debug;

    for client in app.snapshot_clients() {
        let Some(bot) = cfg.bot_apps.iter().find(|b| b.name == client.name) else {
            continue;
        };
        if !gate::bot_list_gate(app, bot, kind) {
            if debug {
                log::info!(
                    "dispatch: bot {} 名单未通过（{}）",
                    bot.name,
                    kind.describe()
                );
            }
            continue;
        }

        let mut cloned = msg.clone();
        let decision = client.filter.read().filter_content(&mut cloned, debug);
        if !decision.allowed {
            continue;
        }

        let data: Vec<u8> = if decision.modified {
            match serde_json::to_vec(&cloned.intact) {
                Ok(d) => d,
                Err(e) => {
                    log::error!("dispatch: 序列化失败 {}: {}", bot.name, e);
                    continue;
                }
            }
        } else {
            cloned.raw
        };
        let _ = client.tx.send(ClientWrite {
            msg: Message::Text(String::from_utf8_lossy(&data).into_owned().into()),
            inc_bot_recv: true,
        });
    }
}

fn broadcast_raw(app: &Arc<App>, is_text: bool, raw: &[u8]) {
    for client in app.snapshot_clients() {
        let msg = if is_text {
            Message::Text(String::from_utf8_lossy(raw).into_owned().into())
        } else {
            Message::Binary(raw.to_vec().into())
        };
        let _ = client.tx.send(ClientWrite {
            msg,
            inc_bot_recv: false,
        });
    }
}
