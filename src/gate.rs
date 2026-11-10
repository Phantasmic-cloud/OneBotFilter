//! 群聊 / 私聊统一的「能不能继续往下走」判断。
//!
//! 群聊：群是否启用（enabled_groups）→ 全局黑用户 → 该 bot 的群名单（mode.group）
//! 私聊：用户是否在全局私聊白名单（enabled_private_users）→ 全局黑用户
//!       → 该 bot 的私聊用户名单（mode.private）
//!
//! 内容过滤（message.group / message.private）仍由 `filters::Filter` 根据消息类型选择。

use std::sync::Arc;

use crate::config::BotAppsConfig;
use crate::global::{App, GROUP, PRIVATE};
use crate::onebot_message::OneBotMessage;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatKind {
    Group { group_id: i64 },
    Private { user_id: i64 },
}

impl ChatKind {
    pub fn describe(&self) -> String {
        match self {
            ChatKind::Group { group_id } => format!("群 {}", group_id),
            ChatKind::Private { user_id } => format!("私聊用户 {}", user_id),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    /// 群没有 /enable。
    GroupDisabled,
    /// 私聊用户不在全局私聊白名单（没执行过 `/enable <qq> private`）。
    PrivateUserNotEnabled,
    /// 命中了全局黑名单。
    BlackUser,
    /// 被该 bot 的群名单过滤。
    GroupListFiltered,
    /// 被该 bot 的私聊名单过滤。
    PrivateListFiltered,
}

impl DropReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            DropReason::GroupDisabled => "群未 enable",
            DropReason::PrivateUserNotEnabled => "私聊用户未 enable",
            DropReason::BlackUser => "全局黑用户",
            DropReason::GroupListFiltered => "bot 群名单未通过",
            DropReason::PrivateListFiltered => "bot 私聊名单未通过",
        }
    }
}

/// 只认 `post_type=message` 的消息事件；notice/request/meta_event 返回 None。
pub fn classify(msg: &OneBotMessage) -> Option<ChatKind> {
    if msg.partial.post_type != "message" && !msg.partial.post_type.is_empty() {
        return None;
    }
    match msg.partial.message_type.as_str() {
        GROUP if msg.partial.group_id != 0 => Some(ChatKind::Group {
            group_id: msg.partial.group_id,
        }),
        PRIVATE => Some(ChatKind::Private {
            user_id: msg.partial.user_id,
        }),
        _ => None,
    }
}

/// 与具体 bot 无关的全局闸门。
pub fn global_gate(app: &App, kind: ChatKind, user_id: i64) -> Result<(), DropReason> {
    match kind {
        ChatKind::Group { group_id } => {
            if !app.store.is_group_enabled(group_id) {
                return Err(DropReason::GroupDisabled);
            }
        }
        ChatKind::Private { user_id } => {
            if !app.store.is_private_user_enabled(user_id) {
                return Err(DropReason::PrivateUserNotEnabled);
            }
        }
    }
    if app.store.is_black_user(user_id) {
        return Err(DropReason::BlackUser);
    }
    Ok(())
}

/// 某个 bot 自己的名单闸门。
pub fn bot_list_gate(app: &App, cfg: &BotAppsConfig, kind: ChatKind) -> bool {
    match kind {
        ChatKind::Group { group_id } => {
            app.store
                .is_bot_group_allowed(&cfg.name, &cfg.mode.group, group_id)
        }
        ChatKind::Private { user_id } => {
            app.store
                .is_bot_private_allowed(&cfg.name, &cfg.mode.private, user_id)
        }
    }
}

/// 调试日志用的拒绝原因（区分群名单/私聊名单）。
pub fn bot_filter_reason(kind: ChatKind) -> DropReason {
    match kind {
        ChatKind::Group { .. } => DropReason::GroupListFiltered,
        ChatKind::Private { .. } => DropReason::PrivateListFiltered,
    }
}

#[allow(dead_code)]
fn _assert_arc(a: &Arc<App>) -> &App {
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BotAppsConfig, BotMessageConfig, Config, MessageConfig, ModeConfig};
    use crate::global::{BLACKLIST, WHITELIST};
    use crate::onebot_message::parse;
    use crate::store::Store;

    fn test_app() -> Arc<App> {
        let dir = std::env::temp_dir().join(format!(
            "obf-gate-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = Config::default();
        cfg.server.host = "127.0.0.1".into();
        cfg.server.port = 3939;
        cfg.server.bot_id = "123456789".into();
        cfg.server.user_agent = "OneBotFilter".into();
        cfg.bot_apps = vec![BotAppsConfig {
            name: "alpha".into(),
            uri: "ws://127.0.0.1:1/ws".into(),
            mode: ModeConfig {
                group: WHITELIST.into(),
                private: WHITELIST.into(),
            },
            message: BotMessageConfig {
                group: MessageConfig::default(),
                private: MessageConfig::default(),
            },
            ..Default::default()
        }];
        let store = Store::load(dir.join("db.json").to_str().unwrap(), &cfg).unwrap();
        Arc::new(App::new(cfg, store))
    }

    fn msg(raw: &str) -> OneBotMessage {
        parse(raw.as_bytes()).unwrap()
    }

    #[test]
    fn classifies_group_and_private() {
        let g = msg(r#"{"post_type":"message","message_type":"group","group_id":100,"user_id":1}"#);
        assert_eq!(classify(&g), Some(ChatKind::Group { group_id: 100 }));
        let p = msg(r#"{"post_type":"message","message_type":"private","user_id":1}"#);
        assert_eq!(classify(&p), Some(ChatKind::Private { user_id: 1 }));
        let n = msg(r#"{"post_type":"notice","notice_type":"friend_add","user_id":1}"#);
        assert_eq!(classify(&n), None);
    }

    #[test]
    fn group_global_gate() {
        let app = test_app();
        let kind = ChatKind::Group { group_id: 100 };
        assert_eq!(global_gate(&app, kind, 1), Err(DropReason::GroupDisabled));
        app.store.enable_group(100).unwrap();
        assert!(global_gate(&app, kind, 1).is_ok());
        app.store.add_black_user(1).unwrap();
        assert_eq!(global_gate(&app, kind, 1), Err(DropReason::BlackUser));
    }

    #[test]
    fn private_global_gate() {
        let app = test_app();
        let kind = ChatKind::Private { user_id: 1 };
        assert_eq!(
            global_gate(&app, kind, 1),
            Err(DropReason::PrivateUserNotEnabled)
        );
        app.store.enable_private_user(1).unwrap();
        assert!(global_gate(&app, kind, 1).is_ok());
        app.store.add_black_user(1).unwrap();
        assert_eq!(global_gate(&app, kind, 1), Err(DropReason::BlackUser));
    }

    #[test]
    fn bot_list_gates() {
        let app = test_app();
        let cfg = app.config().bot_apps[0].clone();
        let group = ChatKind::Group { group_id: 100 };
        assert!(!bot_list_gate(&app, &cfg, group));
        app.store
            .set_bot_group("alpha", WHITELIST, 100, true)
            .unwrap();
        assert!(bot_list_gate(&app, &cfg, group));

        let private = ChatKind::Private { user_id: 123456789 };
        assert!(!bot_list_gate(&app, &cfg, private));
        app.store
            .set_bot_private_user("alpha", WHITELIST, 123456789, true)
            .unwrap();
        assert!(bot_list_gate(&app, &cfg, private));
    }

    #[test]
    fn blacklist_mode_allows_by_default() {
        let app = test_app();
        let mut cfg = app.config().bot_apps[0].clone();
        cfg.mode.group = BLACKLIST.into();
        cfg.mode.private = BLACKLIST.into();
        assert!(bot_list_gate(&app, &cfg, ChatKind::Group { group_id: 100 }));
        assert!(bot_list_gate(&app, &cfg, ChatKind::Private { user_id: 1 }));
    }
}
