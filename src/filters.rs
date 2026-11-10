//! 内容过滤：mode / prefix / 正则。
//!
//! `Filter::filter_content` 根据消息类型自动选择 `message.group` 或 `message.private`，
//! 返回 `FilterDecision { allowed, modified }`，让 dispatch 可以在未改写时直接透传原始字节。

use std::fmt;

use fancy_regex::Regex;

use crate::config::{BotAppsConfig, MessageConfig};
use crate::global::{
    BLACKLIST, GROUP, MESSAGE_FORMAT_ARRAY, MESSAGE_FORMAT_STRING, MESSAGE_TYPE_TEXT, OFF, ON,
    PRIVATE, WHITELIST,
};
use crate::onebot_message::OneBotMessage;
use crate::rewrite::{compile_rewrite_rules, CompiledRewrite};

/// 内容过滤结果：`modified` 表示消息内容被改过（prefix-replace），需要重新序列化。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilterDecision {
    pub allowed: bool,
    pub modified: bool,
}

impl FilterDecision {
    pub fn pass() -> Self {
        Self {
            allowed: true,
            modified: false,
        }
    }

    pub fn pass_modified() -> Self {
        Self {
            allowed: true,
            modified: true,
        }
    }

    pub fn deny() -> Self {
        Self {
            allowed: false,
            modified: false,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct MessageFilter {
    pub config: MessageConfig,
    pub regexps: Vec<Regex>,
}

#[derive(Default)]
pub struct Filter {
    pub name: String,
    pub group_mode: String,
    pub private_message: MessageFilter,
    pub group_message: MessageFilter,
    pub rewrite: Vec<CompiledRewrite>,
}

impl Filter {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            ..Default::default()
        }
    }

    pub fn compile(&mut self, cfg: &BotAppsConfig) {
        self.name = cfg.name.clone();
        self.group_mode = cfg.mode.group.clone();
        self.private_message.compile(&cfg.message.private);
        self.group_message.compile(&cfg.message.group);
        self.rewrite = compile_rewrite_rules(&cfg.rewrite_text);
    }

    /// 仅做内容过滤（前缀/正则）。名单与全局开关由 gate 处理。
    pub fn filter_content(&self, msg: &mut OneBotMessage, debug: bool) -> FilterDecision {
        let used_filter = match msg.partial.message_type.as_str() {
            GROUP => &self.group_message,
            PRIVATE => &self.private_message,
            _ => return FilterDecision::pass(),
        };

        if used_filter.config.mode.is_empty() || used_filter.config.mode == ON {
            if debug {
                log::info!("{}：直接通过的消息：{}", self.name, msg.partial.raw_message);
            }
            return FilterDecision::pass();
        }
        if used_filter.config.mode == OFF {
            if debug {
                log::info!("{}：被禁止的消息：{}", self.name, msg.partial.raw_message);
            }
            return FilterDecision::deny();
        }
        if used_filter.prefix_pass(msg) {
            if debug {
                log::info!("{}：前缀通过的消息：{}", self.name, msg.partial.raw_message);
            }
            return FilterDecision::pass_modified();
        }
        if msg.partial.message_format == MESSAGE_FORMAT_ARRAY {
            for seg in &msg.partial.message_array {
                if seg.ty != MESSAGE_TYPE_TEXT {
                    continue;
                }
                let text = match seg.data.get("text").and_then(|v| v.as_str()) {
                    Some(t) => t.trim().to_string(),
                    None => continue,
                };
                if let Some(allowed) =
                    used_filter.process_filter(&self.name, &text, &msg.partial.raw_message, debug)
                {
                    return if allowed {
                        FilterDecision::pass()
                    } else {
                        FilterDecision::deny()
                    };
                }
            }
        } else if let Some(allowed) = used_filter.process_filter(
            &self.name,
            msg.partial.message_string.trim(),
            &msg.partial.raw_message,
            debug,
        ) {
            return if allowed {
                FilterDecision::pass()
            } else {
                FilterDecision::deny()
            };
        }

        match used_filter.config.mode.as_str() {
            WHITELIST => {
                if debug {
                    log::info!(
                        "{}：不在白名单中的消息：{}",
                        self.name,
                        msg.partial.raw_message
                    );
                }
                FilterDecision::deny()
            }
            BLACKLIST => {
                if debug {
                    log::info!(
                        "{}：不在黑名单中的消息：{}",
                        self.name,
                        msg.partial.raw_message
                    );
                }
                FilterDecision::pass()
            }
            _ => {
                log::error!(
                    "{}的message.mode配置异常，必须为whitelist或blacklist",
                    self.name
                );
                FilterDecision::deny()
            }
        }
    }
}

impl MessageFilter {
    pub fn compile(&mut self, cfg: &MessageConfig) {
        self.config = cfg.clone();
        let mut new_filters = Vec::new();
        for filter in &self.config.filters {
            match Regex::new(filter) {
                Ok(pattern) => new_filters.push(pattern),
                Err(e) => log::error!("编译正则表达式：{}，出错：{}", filter, e),
            }
        }
        self.regexps = new_filters;
    }

    /// 前缀命中时放行，并把前缀替换成 `prefix-replace` 后再交给 bot。
    pub fn prefix_pass(&self, msg: &mut OneBotMessage) -> bool {
        if self.config.prefix.is_empty() {
            return false;
        }

        let (text_old, index) = match msg.partial.message_format.as_str() {
            MESSAGE_FORMAT_ARRAY => {
                let mut found = None;
                for (i, seg) in msg.partial.message_array.iter().enumerate() {
                    if seg.ty != MESSAGE_TYPE_TEXT {
                        continue;
                    }
                    if let Some(t) = seg.data.get("text").and_then(|v| v.as_str()) {
                        found = Some((t.trim().to_string(), i));
                        break;
                    }
                }
                match found {
                    Some(v) => v,
                    None => return false,
                }
            }
            MESSAGE_FORMAT_STRING => (msg.partial.message_string.trim().to_string(), 0),
            _ => return false,
        };
        if text_old.is_empty() {
            return false;
        }

        let mut prefix = "";
        for p in &self.config.prefix {
            if !p.is_empty() && text_old.starts_with(p.as_str()) {
                prefix = p;
                break;
            }
        }
        if prefix.is_empty() {
            return false;
        }

        let text = format!(
            "{}{}",
            self.config.prefix_replace,
            &text_old[prefix.len()..]
        );

        match msg.partial.message_format.as_str() {
            MESSAGE_FORMAT_ARRAY => {
                if let Some(seg) = msg.partial.message_array.get_mut(index) {
                    seg.data
                        .insert("text".to_string(), serde_json::Value::String(text.clone()));
                }
                if text.trim().is_empty() {
                    msg.partial.message_array.remove(index);
                }
                match serde_json::to_value(&msg.partial.message_array) {
                    Ok(v) => {
                        msg.intact.insert("message".to_string(), v);
                    }
                    Err(e) => {
                        log::error!("将修改后的消息转为json字符串出错 {}", e);
                        return false;
                    }
                }
            }
            MESSAGE_FORMAT_STRING => {
                msg.partial.message_string = text.clone();
                msg.intact.insert(
                    "message".to_string(),
                    serde_json::Value::String(text.clone()),
                );
            }
            _ => return false,
        }

        msg.partial.raw_message = msg.partial.raw_message.replacen(&text_old, &text, 1);
        msg.intact.insert(
            "raw_message".to_string(),
            serde_json::Value::String(msg.partial.raw_message.clone()),
        );
        true
    }

    pub fn process_filter(
        &self,
        name: &str,
        text: &str,
        raw_message: &str,
        debug: bool,
    ) -> Option<bool> {
        for pattern in &self.regexps {
            match pattern.is_match(text) {
                Ok(true) => match self.config.mode.as_str() {
                    WHITELIST => {
                        if debug {
                            log::info!("{}：白名单的消息：{}", name, raw_message);
                        }
                        return Some(true);
                    }
                    BLACKLIST => {
                        if debug {
                            log::info!("{}：黑名单的消息：{}", name, raw_message);
                        }
                        return Some(false);
                    }
                    _ => {}
                },
                Ok(false) => {}
                Err(e) => log::error!(
                    "过滤器{}正则匹配出错的消息：{}（{}）",
                    pattern.as_str(),
                    raw_message,
                    e
                ),
            }
        }
        None
    }
}

impl fmt::Display for Filter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "\nname: {}\nmode.group: {}\nprivate-message: {}\n\tfilters: [ {} ]\n\tprefix: [ {} ], replace: {}\ngroup-message: {}\n\tfilters: [ {} ]\n\tprefix: [ {} ], replace: {}",
            self.name,
            self.group_mode,
            self.private_message.config.mode,
            self.private_message.config.filters.join(", "),
            self.private_message.config.prefix.join(", "),
            self.private_message.config.prefix_replace,
            self.group_message.config.mode,
            self.group_message.config.filters.join(", "),
            self.group_message.config.prefix.join(", "),
            self.group_message.config.prefix_replace,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BotAppsConfig, BotMessageConfig, MessageConfig, ModeConfig};
    use crate::onebot_message::parse;

    fn compile(mut cfg: BotAppsConfig) -> Filter {
        cfg.check().unwrap();
        let mut f = Filter::new(&cfg.name);
        f.compile(&cfg);
        f
    }

    fn bot_with_group_message(message: MessageConfig) -> BotAppsConfig {
        BotAppsConfig {
            name: "alpha".into(),
            uri: "ws://127.0.0.1:8080/onebot/v11/ws".into(),
            mode: ModeConfig {
                group: BLACKLIST.into(),
                private: WHITELIST.into(),
            },
            message: BotMessageConfig {
                group: message,
                private: MessageConfig::default(),
            },
            ..Default::default()
        }
    }

    fn group_text_message(text: &str) -> crate::onebot_message::OneBotMessage {
        let raw = format!(
            r#"{{"post_type":"message","message_type":"group","message_format":"array","message":[{{"type":"text","data":{{"text":"{}"}}}}],"raw_message":"{}"}}"#,
            text, text
        );
        parse(raw.as_bytes()).unwrap()
    }

    fn private_text_message(text: &str) -> crate::onebot_message::OneBotMessage {
        let raw = format!(
            r#"{{"post_type":"message","message_type":"private","message_format":"array","message":[{{"type":"text","data":{{"text":"{}"}}}}],"raw_message":"{}"}}"#,
            text, text
        );
        parse(raw.as_bytes()).unwrap()
    }

    #[test]
    fn filter_modes() {
        let cases: Vec<(MessageConfig, &str, bool)> = vec![
            (
                MessageConfig {
                    mode: ON.into(),
                    ..Default::default()
                },
                "随便",
                true,
            ),
            (
                MessageConfig {
                    mode: OFF.into(),
                    ..Default::default()
                },
                "随便",
                false,
            ),
            (
                MessageConfig {
                    mode: WHITELIST.into(),
                    filters: vec!["^hello".into()],
                    ..Default::default()
                },
                "hello world",
                true,
            ),
            (
                MessageConfig {
                    mode: WHITELIST.into(),
                    filters: vec!["^hello".into()],
                    ..Default::default()
                },
                "bye",
                false,
            ),
            (
                MessageConfig {
                    mode: BLACKLIST.into(),
                    filters: vec!["^/".into()],
                    ..Default::default()
                },
                "/cmd",
                false,
            ),
            (
                MessageConfig {
                    mode: BLACKLIST.into(),
                    filters: vec!["^/".into()],
                    ..Default::default()
                },
                "hello",
                true,
            ),
        ];
        for (message, text, want) in cases {
            let f = compile(bot_with_group_message(message));
            let mut msg = group_text_message(text);
            let decision = f.filter_content(&mut msg, false);
            assert_eq!(decision.allowed, want, "text={text}");
            assert!(!decision.modified, "未改写的消息不应标记 modified");
        }
    }

    #[test]
    fn prefix_pass_rewrites() {
        let f = compile(bot_with_group_message(MessageConfig {
            mode: BLACKLIST.into(),
            filters: vec!["^never-match".into()],
            prefix: vec!["#".into()],
            prefix_replace: "/".into(),
        }));
        let mut msg = group_text_message("#help");
        let decision = f.filter_content(&mut msg, false);
        assert!(decision.allowed);
        assert!(decision.modified);
        assert_eq!(
            msg.partial.message_array[0].data.get("text").unwrap(),
            &serde_json::Value::String("/help".into())
        );
        assert_eq!(msg.partial.raw_message, "/help");
    }

    #[test]
    fn private_uses_private_message_config() {
        let mut bot = bot_with_group_message(MessageConfig {
            mode: ON.into(),
            ..Default::default()
        });
        bot.message.private = MessageConfig {
            mode: BLACKLIST.into(),
            filters: vec!["^secret".into()],
            ..Default::default()
        };
        let f = compile(bot);
        let mut allowed = private_text_message("hello");
        assert!(f.filter_content(&mut allowed, false).allowed);
        let mut blocked = private_text_message("secret");
        assert!(!f.filter_content(&mut blocked, false).allowed);
    }
}
