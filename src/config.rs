//! 配置结构、校验与热更新。
//!
//! 第三代 schema：
//! - bot 级 `mode` / `message` 是群聊和私聊共用的默认值；
//! - `group` / `private` 块是单侧覆盖，`message` 按字段深合并（只写要改的字段）；
//! - 旧字段（v1 的 `group-id` / `group-message` / `private-message`、v2 的
//!   `mode.group` / `message.group`）不兼容，加载时给出明确的迁移提示。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::global::App;
use crate::global::{BLACKLIST, OFF, ON, WHITELIST};

// ---- 运行时结构（解析后已把共用配置与单侧覆盖合并成两侧各一份）----

#[derive(Debug, Clone, Default)]
pub struct Config {
    pub server: ServerConfig,
    pub bot_apps: Vec<BotAppsConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub suffix: String,
    pub bot_id: String,
    pub user_agent: String,
    pub superusers: Vec<i64>,
    pub command_prefix: String,
    pub db_path: String,
    pub buffer_size: i32,
    pub sleep_time: f32,
    pub debug: bool,
    /// 是否自动清理 db.json 里已经不存在的 bot / 群相关状态。
    pub prune_runtime_state: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 0,
            suffix: String::new(),
            bot_id: String::new(),
            user_agent: String::new(),
            superusers: Vec::new(),
            command_prefix: "/".to_string(),
            db_path: "db.json".to_string(),
            buffer_size: 4096,
            sleep_time: 5.0,
            debug: false,
            prune_runtime_state: true,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct BotAppsConfig {
    pub name: String,
    pub alias: Vec<String>,
    pub uri: String,
    pub access_token: String,
    /// 群名单 / 私聊用户名单的白黑模式（已合并共用模式与单侧覆盖）。
    pub mode: ModeConfig,
    /// 群消息 / 私聊消息的过滤配置（已合并共用配置与单侧覆盖）。
    pub message: BotMessageConfig,
    pub rewrite_text: Vec<RewriteTextRule>,
}

#[derive(Debug, Clone, Default)]
pub struct ModeConfig {
    pub group: String,
    pub private: String,
}

#[derive(Debug, Clone, Default)]
pub struct BotMessageConfig {
    pub group: MessageConfig,
    pub private: MessageConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct MessageConfig {
    pub mode: String,
    pub filters: Vec<String>,
    pub prefix: Vec<String>,
    pub prefix_replace: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct RewriteTextRule {
    pub original: String,
    pub replace: String,
}

// ---- 原始配置：bot 级共用 + group/private 单侧覆盖 ----

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
struct RawConfig {
    server: ServerConfig,
    bot_apps: Vec<RawBotAppsConfig>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
struct RawBotAppsConfig {
    name: String,
    alias: Vec<String>,
    uri: String,
    access_token: String,
    /// 群聊与私聊共用的名单模式，可被 group.mode / private.mode 覆盖。
    mode: Option<String>,
    /// 群聊与私聊共用的消息过滤，可被 group.message / private.message 按字段覆盖。
    message: Option<MessageConfig>,
    group: Option<RawSide>,
    private: Option<RawSide>,
    rewrite_text: Vec<RewriteTextRule>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
struct RawSide {
    mode: Option<String>,
    message: Option<MessageOverride>,
}

/// 单侧 message 覆盖：只写要改的字段，其余沿用共用配置。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
struct MessageOverride {
    mode: Option<String>,
    filters: Option<Vec<String>>,
    prefix: Option<Vec<String>>,
    prefix_replace: Option<String>,
}

impl RawBotAppsConfig {
    fn resolve(&self, index: usize) -> Result<BotAppsConfig, String> {
        let who = if self.name.is_empty() {
            format!("bot-apps[{index}]")
        } else {
            format!("bot-apps[{index}]（{}）", self.name)
        };

        let shared_mode = self.mode.as_deref();
        let group_mode = self
            .group
            .as_ref()
            .and_then(|side| side.mode.as_deref())
            .or(shared_mode)
            .ok_or_else(|| format!("{who} 缺少群聊名单模式：写 bot 级 mode 或 group.mode"))?;
        let private_mode = self
            .private
            .as_ref()
            .and_then(|side| side.mode.as_deref())
            .or(shared_mode)
            .ok_or_else(|| format!("{who} 缺少私聊名单模式：写 bot 级 mode 或 private.mode"))?;

        let shared_message = self.message.clone().unwrap_or_default();
        let group_message = merge_message(
            &shared_message,
            self.group.as_ref().and_then(|side| side.message.as_ref()),
        );
        let private_message = merge_message(
            &shared_message,
            self.private.as_ref().and_then(|side| side.message.as_ref()),
        );

        Ok(BotAppsConfig {
            name: self.name.clone(),
            alias: self.alias.clone(),
            uri: self.uri.clone(),
            access_token: self.access_token.clone(),
            mode: ModeConfig {
                group: group_mode.to_string(),
                private: private_mode.to_string(),
            },
            message: BotMessageConfig {
                group: group_message,
                private: private_message,
            },
            rewrite_text: self.rewrite_text.clone(),
        })
    }
}

/// 单侧 message 覆盖：逐字段取覆盖值，没写的字段沿用共用配置。
fn merge_message(base: &MessageConfig, over: Option<&MessageOverride>) -> MessageConfig {
    let Some(over) = over else {
        return base.clone();
    };
    MessageConfig {
        mode: over.mode.clone().unwrap_or_else(|| base.mode.clone()),
        filters: over.filters.clone().unwrap_or_else(|| base.filters.clone()),
        prefix: over.prefix.clone().unwrap_or_else(|| base.prefix.clone()),
        prefix_replace: over
            .prefix_replace
            .clone()
            .unwrap_or_else(|| base.prefix_replace.clone()),
    }
}

impl ServerConfig {
    pub fn check(&mut self) -> Result<(), String> {
        if self.host.is_empty() {
            return Err("server.host不能为空".to_string());
        }
        if self.port == 0 {
            return Err("server.port不能为0".to_string());
        }
        if self.bot_id.is_empty() {
            return Err("server.bot-id不能为空".to_string());
        }
        if self.user_agent.is_empty() {
            return Err("server.user-agent不能为空".to_string());
        }
        if self.command_prefix.is_empty() {
            self.command_prefix = "/".to_string();
        }
        if self.db_path.is_empty() {
            self.db_path = "db.json".to_string();
        }
        if self.sleep_time <= 0.0 {
            self.sleep_time = 5.0;
        }
        if self.buffer_size <= 0 {
            self.buffer_size = 4096;
        }
        if self.superusers.is_empty() {
            log::warn!("警告: server.superusers 为空，管理指令将无人可用");
        }
        Ok(())
    }
}

fn reserved_bot_token(s: &str) -> Result<(), String> {
    match s.trim().to_lowercase().as_str() {
        "status" => Err("不能为 status（与 /bot status、/group status 冲突）".to_string()),
        "enable" | "disable" | "bu" | "bot" | "group" | "on" | "off" => {
            Err(format!("不能为 {}（与内置指令冲突）", s))
        }
        _ => Ok(()),
    }
}

fn check_message_config(prefix: &str, cfg: &MessageConfig) -> Result<(), String> {
    match cfg.mode.as_str() {
        "" | ON | OFF | WHITELIST | BLACKLIST => Ok(()),
        _ => Err(format!(
            "{}.mode 配置错误，只能是 on、off、whitelist 或 blacklist",
            prefix
        )),
    }
}

impl BotAppsConfig {
    pub fn check(&mut self) -> Result<(), String> {
        if self.name.is_empty() {
            return Err("bot-apps.name不能为空".to_string());
        }
        if self.name.contains(' ') || self.name.contains('\t') {
            return Err(format!(
                "bot-apps.name {:?} 不能包含空格（指令按空格分词）",
                self.name
            ));
        }
        if let Err(e) = reserved_bot_token(&self.name) {
            return Err(format!("bot-apps.name {}", e));
        }
        if self.uri.is_empty() {
            return Err(format!("{}.uri不能为空", self.name));
        }
        match self.mode.group.as_str() {
            WHITELIST | BLACKLIST => {}
            _ => {
                return Err(format!(
                    "{}.mode.group 必须是 whitelist 或 blacklist",
                    self.name
                ))
            }
        }
        match self.mode.private.as_str() {
            WHITELIST | BLACKLIST => {}
            _ => {
                return Err(format!(
                    "{}.mode.private 必须是 whitelist 或 blacklist",
                    self.name
                ))
            }
        }
        check_message_config(&format!("{}.message.group", self.name), &self.message.group)?;
        check_message_config(
            &format!("{}.message.private", self.name),
            &self.message.private,
        )?;
        for (i, rule) in self.rewrite_text.iter().enumerate() {
            if rule.original.trim().is_empty() {
                return Err(format!(
                    "{}.rewrite-text[{}].original 不能为空",
                    self.name, i
                ));
            }
            if let Err(e) = fancy_regex::Regex::new(&rule.original) {
                return Err(format!(
                    "{}.rewrite-text[{}].original 正则无效: {}",
                    self.name, i, e
                ));
            }
        }
        Ok(())
    }
}

impl Config {
    pub fn check(&mut self) -> Result<(), String> {
        let mut errors: Vec<String> = Vec::new();
        if let Err(e) = self.server.check() {
            errors.push(e);
        }

        // lower(token) -> 来源说明，用于 name / alias 全局互斥
        let mut taken: std::collections::HashMap<String, String> = std::collections::HashMap::new();

        for bot in self.bot_apps.iter_mut() {
            if let Err(e) = bot.check() {
                errors.push(e);
            }
            if bot.name.is_empty() {
                continue;
            }
            let name_key = bot.name.to_lowercase();
            match taken.get(&name_key) {
                Some(prev) => errors.push(format!(
                    "bot-apps.name 重复: {}（与 {} 冲突）",
                    bot.name, prev
                )),
                None => {
                    taken.insert(name_key.clone(), format!("name {}", bot.name));
                }
            }

            let mut cleaned: Vec<String> = Vec::new();
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            for raw in bot.alias.clone() {
                let alias = raw.trim().to_string();
                if alias.is_empty() {
                    errors.push(format!("{}.alias 不能为空", bot.name));
                    continue;
                }
                if alias.contains(' ') || alias.contains('\t') {
                    errors.push(format!(
                        "{}.alias {:?} 不能包含空格（指令按空格分词）",
                        bot.name, alias
                    ));
                    continue;
                }
                if let Err(e) = reserved_bot_token(&alias) {
                    errors.push(format!("{}.alias {:?} {}", bot.name, alias, e));
                    continue;
                }
                let alias_key = alias.to_lowercase();
                if alias_key == name_key {
                    errors.push(format!("{}.alias {:?} 与自身 name 重复", bot.name, alias));
                    continue;
                }
                if seen.contains(&alias_key) {
                    errors.push(format!("{}.alias 重复: {}", bot.name, alias));
                    continue;
                }
                if let Some(prev) = taken.get(&alias_key) {
                    errors.push(format!("{}.alias {:?} 与 {} 冲突", bot.name, alias, prev));
                    continue;
                }
                seen.insert(alias_key.clone());
                taken.insert(alias_key, format!("{} 的 alias {}", bot.name, alias));
                cleaned.push(alias);
            }
            bot.alias = cleaned;
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("\n"))
        }
    }
}

/// 旧版配置字段检测：给出明确迁移提示，而不是静默忽略。
fn detect_legacy_fields(raw: &serde_json::Value) -> Result<(), String> {
    let Some(bots) = raw.get("bot-apps").and_then(|b| b.as_array()) else {
        return Ok(());
    };
    for (i, bot) in bots.iter().enumerate() {
        let Some(map) = bot.as_object() else { continue };
        // 第一代（Go 原版）
        for old in ["group-id", "group-message", "private-message"] {
            if map.contains_key(old) {
                return Err(format!(
                    "bot-apps[{i}] 使用了第一代字段 {old:?}；迁移：group-id.mode → bot 级 mode，\
                     group-message → group.message，private-message → private.message；\
                     群聊私聊配置一样时只写 bot 级 message"
                ));
            }
        }
        // 第二代（两侧各写一份）
        if let Some(serde_json::Value::Object(mode)) = map.get("mode") {
            if mode.contains_key("group") || mode.contains_key("private") {
                return Err(format!(
                    "bot-apps[{i}].mode 是第二代写法；现在 bot 级 mode 是群聊/私聊共用的默认值，\
                     两侧不同时写 group.mode / private.mode"
                ));
            }
        }
        if let Some(serde_json::Value::Object(message)) = map.get("message") {
            if message.contains_key("group") || message.contains_key("private") {
                return Err(format!(
                    "bot-apps[{i}].message 是第二代写法；现在 bot 级 message 是共用的默认值，\
                     两侧不同时写 group.message / private.message"
                ));
            }
        }
    }
    Ok(())
}

/// 读取并校验配置文件。
pub fn load_config(path: &str) -> Result<Config, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    parse_config_text(&text)
}

/// 解析配置文本：先把 bot 级共用配置与 group/private 覆盖合并成两侧各一份。
fn parse_config_text(text: &str) -> Result<Config, String> {
    let probe: serde_json::Value = serde_yaml_ng::from_str(text).map_err(|e| e.to_string())?;
    detect_legacy_fields(&probe)?;

    let raw: RawConfig = serde_yaml_ng::from_str(text).map_err(|e| e.to_string())?;
    let bot_apps = raw
        .bot_apps
        .iter()
        .enumerate()
        .map(|(i, bot)| bot.resolve(i))
        .collect::<Result<Vec<_>, _>>()?;
    let mut cfg = Config {
        server: raw.server,
        bot_apps,
    };
    cfg.check()?;
    Ok(cfg)
}

/// 监听配置文件变化并热更（校验失败保留旧配置）。
pub fn spawn_watcher(app: Arc<App>, path: String) {
    std::thread::spawn(move || {
        use notify::{RecursiveMode, Watcher};

        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = match notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        }) {
            Ok(w) => w,
            Err(e) => {
                log::error!("监听配置文件失败: {}", e);
                return;
            }
        };
        let file = PathBuf::from(&path);
        let dir = match file.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        if let Err(e) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
            log::error!("监听配置文件失败: {}", e);
            return;
        }

        let file_name = file.file_name().map(|n| n.to_owned());
        let mut last = Instant::now()
            .checked_sub(Duration::from_secs(60))
            .unwrap_or_else(Instant::now);
        loop {
            match rx.recv() {
                Ok(Ok(event)) => {
                    let matched = match &file_name {
                        Some(n) => event
                            .paths
                            .iter()
                            .any(|p| p.file_name() == Some(n.as_os_str())),
                        None => true,
                    };
                    if !matched {
                        continue;
                    }
                    let now = Instant::now();
                    if now.duration_since(last) < Duration::from_millis(200) {
                        continue;
                    }
                    last = now;
                    reload(&app, &path);
                }
                Ok(Err(e)) => log::error!("监听配置出错: {}", e),
                Err(_) => break,
            }
        }
        drop(watcher);
    });
}

fn reload(app: &App, path: &str) {
    log::info!("config file changed: {}", path);
    match load_config(path) {
        Ok(cfg) => {
            app.store.prune_config_bots(&cfg);
            app.store.sync_bots_from_config(&cfg);
            app.set_config(cfg);
            app.reload_filters();
        }
        Err(e) => log::error!("配置文件校验失败: {}", e),
    }
}

impl Config {
    pub fn listen_addr(&self) -> String {
        format!("{}:{}", self.server.host, self.server.port)
    }

    /// 配置文件同目录路径。
    pub fn same_dir(path: &str) -> PathBuf {
        Path::new(path)
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_server() -> ServerConfig {
        ServerConfig {
            host: "127.0.0.1".into(),
            port: 3939,
            suffix: "/ws".into(),
            bot_id: "123456789".into(),
            user_agent: "OneBotFilter".into(),
            superusers: vec![123456789],
            ..Default::default()
        }
    }

    fn valid_bot(name: &str) -> BotAppsConfig {
        BotAppsConfig {
            name: name.into(),
            uri: "ws://127.0.0.1:8080/onebot/v11/ws".into(),
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
        }
    }

    fn valid_config() -> Config {
        Config {
            server: valid_server(),
            bot_apps: vec![valid_bot("alpha"), valid_bot("beta")],
        }
    }

    #[test]
    fn accepts_valid() {
        let mut cfg = valid_config();
        assert!(cfg.check().is_ok(), "{:?}", cfg.check().err());
    }

    #[test]
    fn server_required_fields() {
        for (name, mutate) in [
            (
                "host",
                (|s: &mut ServerConfig| s.host.clear()) as fn(&mut ServerConfig),
            ),
            ("port", |s: &mut ServerConfig| s.port = 0),
            ("bot-id", |s: &mut ServerConfig| s.bot_id.clear()),
            ("user-agent", |s: &mut ServerConfig| s.user_agent.clear()),
        ] {
            let mut cfg = valid_config();
            mutate(&mut cfg.server);
            let err = cfg.check().unwrap_err();
            assert!(err.contains(name), "{name}: {err}");
        }
    }

    #[test]
    fn server_defaults() {
        let mut sc = valid_server();
        sc.command_prefix.clear();
        sc.db_path.clear();
        sc.check().unwrap();
        assert_eq!(sc.command_prefix, "/");
        assert_eq!(sc.db_path, "db.json");
        assert!(sc.prune_runtime_state);
    }

    #[test]
    fn bot_app_rules() {
        type BotCase = (&'static str, Box<dyn Fn(&mut BotAppsConfig)>);

        let cases: Vec<BotCase> = vec![
            (
                "name 带空格",
                Box::new(|b: &mut BotAppsConfig| b.name = "a b".into()),
            ),
            (
                "内置词",
                Box::new(|b: &mut BotAppsConfig| b.name = "status".into()),
            ),
            (
                "name 为空",
                Box::new(|b: &mut BotAppsConfig| b.name.clear()),
            ),
            ("uri 为空", Box::new(|b: &mut BotAppsConfig| b.uri.clear())),
            (
                "mode.group 非法",
                Box::new(|b: &mut BotAppsConfig| b.mode.group = "all".into()),
            ),
            (
                "mode.private 缺失",
                Box::new(|b: &mut BotAppsConfig| b.mode.private.clear()),
            ),
            (
                "message.group.mode 非法",
                Box::new(|b: &mut BotAppsConfig| b.message.group.mode = "yes".into()),
            ),
            (
                "message.private.mode 非法",
                Box::new(|b: &mut BotAppsConfig| b.message.private.mode = "yes".into()),
            ),
            (
                "正则非法",
                Box::new(|b: &mut BotAppsConfig| {
                    b.rewrite_text = vec![RewriteTextRule {
                        original: "([".into(),
                        replace: "x".into(),
                    }]
                }),
            ),
        ];
        for (name, mutate) in cases {
            let mut bot = valid_bot("alpha");
            mutate(&mut bot);
            let mut cfg = Config {
                server: valid_server(),
                bot_apps: vec![bot],
            };
            assert!(cfg.check().is_err(), "{name} 应报错");
        }
    }

    #[test]
    fn alias_conflicts() {
        let mut alpha = valid_bot("alpha");
        alpha.alias = vec!["a".into(), "共同".into()];
        let mut beta = valid_bot("beta");
        beta.alias = vec!["共同".into()];
        let mut cfg = Config {
            server: valid_server(),
            bot_apps: vec![alpha, beta],
        };
        let err = cfg.check().unwrap_err();
        assert!(err.contains("冲突"), "{err}");
    }

    #[test]
    fn alias_reserved_and_empty() {
        let mut bot = valid_bot("alpha");
        bot.alias = vec!["".into(), "enable".into()];
        let mut cfg = Config {
            server: valid_server(),
            bot_apps: vec![bot],
        };
        let err = cfg.check().unwrap_err();
        assert!(err.contains("alias"), "{err}");
    }

    #[test]
    fn duplicate_name_case_insensitive() {
        let mut cfg = Config {
            server: valid_server(),
            bot_apps: vec![valid_bot("alpha"), valid_bot("Alpha")],
        };
        assert!(cfg.check().is_err());
    }

    fn parse_bot_yaml(bots: &str) -> Result<Config, String> {
        let text = format!(
            "server:\n  host: \"127.0.0.1\"\n  port: 3939\n  suffix: \"/ws\"\n  \
             bot-id: \"123456789\"\n  user-agent: \"OneBotFilter\"\n  superusers: [ 123456789 ]\n\
             bot-apps:\n{bots}"
        );
        parse_config_text(&text)
    }

    #[test]
    fn parses_shared_config() {
        let cfg = parse_bot_yaml(
            "  - name: \"alpha\"\n    uri: \"ws://127.0.0.1:8080/onebot\"\n    \
             mode: \"blacklist\"\n    message:\n      mode: \"on\"\n      filters: [\"^hi\"]\n",
        )
        .unwrap();
        let bot = &cfg.bot_apps[0];
        assert_eq!(bot.mode.group, BLACKLIST);
        assert_eq!(bot.mode.private, BLACKLIST);
        assert_eq!(bot.message.group.mode, ON);
        assert_eq!(bot.message.private.mode, ON);
        assert_eq!(bot.message.group.filters, vec!["^hi"]);
        assert_eq!(bot.message.private.filters, vec!["^hi"]);
    }

    #[test]
    fn parses_side_overrides() {
        let cfg = parse_bot_yaml(
            "  - name: \"alpha\"\n    uri: \"ws://127.0.0.1:8080/onebot\"\n    \
             mode: \"blacklist\"\n    message:\n      mode: \"on\"\n      filters: [\"^hi\"]\n      \
             prefix: [\"#\"]\n    private:\n      mode: \"whitelist\"\n      message:\n        \
             mode: \"off\"\n        prefix: [\"!\"]\n",
        )
        .unwrap();
        let bot = &cfg.bot_apps[0];
        assert_eq!(bot.mode.group, BLACKLIST);
        assert_eq!(bot.mode.private, WHITELIST);
        assert_eq!(bot.message.group.mode, ON);
        assert_eq!(bot.message.private.mode, OFF);
        // 没写的字段沿用共用配置
        assert_eq!(bot.message.private.filters, vec!["^hi"]);
        // 写了的字段覆盖
        assert_eq!(bot.message.group.prefix, vec!["#"]);
        assert_eq!(bot.message.private.prefix, vec!["!"]);
    }

    #[test]
    fn parses_split_only() {
        let cfg = parse_bot_yaml(
            "  - name: \"alpha\"\n    uri: \"ws://127.0.0.1:8080/onebot\"\n    group:\n      \
             mode: \"whitelist\"\n      message:\n        mode: \"on\"\n    private:\n      \
             mode: \"blacklist\"\n      message:\n        mode: \"off\"\n",
        )
        .unwrap();
        let bot = &cfg.bot_apps[0];
        assert_eq!(bot.mode.group, WHITELIST);
        assert_eq!(bot.mode.private, BLACKLIST);
        assert_eq!(bot.message.group.mode, ON);
        assert_eq!(bot.message.private.mode, OFF);
        assert!(bot.message.group.filters.is_empty());
    }

    #[test]
    fn missing_mode_errors() {
        let err = parse_bot_yaml(
            "  - name: \"alpha\"\n    uri: \"ws://127.0.0.1:8080/onebot\"\n    group:\n      \
             mode: \"blacklist\"\n",
        )
        .unwrap_err();
        assert!(err.contains("缺少私聊名单模式"), "{err}");

        let err = parse_bot_yaml(
            "  - name: \"alpha\"\n    uri: \"ws://127.0.0.1:8080/onebot\"\n    message:\n      \
             mode: \"on\"\n",
        )
        .unwrap_err();
        assert!(err.contains("缺少群聊名单模式"), "{err}");
    }

    #[test]
    fn detects_legacy_fields() {
        // 第一代：Go 原版字段
        let v1 = serde_json::json!({
            "bot-apps": [{
                "name": "alpha",
                "uri": "ws://127.0.0.1:8080/onebot/v11/ws",
                "group-id": { "mode": "blacklist" },
                "private-message": { "mode": "on" }
            }]
        });
        let err = detect_legacy_fields(&v1).unwrap_err();
        assert!(err.contains("第一代"), "{err}");

        // 第二代：mode/message 各写两侧
        let v2_mode = serde_json::json!({
            "bot-apps": [{
                "name": "alpha",
                "uri": "ws://127.0.0.1:8080/onebot/v11/ws",
                "mode": { "group": "blacklist", "private": "whitelist" }
            }]
        });
        assert!(detect_legacy_fields(&v2_mode).is_err());
        let v2_message = serde_json::json!({
            "bot-apps": [{
                "name": "alpha",
                "uri": "ws://127.0.0.1:8080/onebot/v11/ws",
                "message": { "group": { "mode": "on" }, "private": { "mode": "on" } }
            }]
        });
        assert!(detect_legacy_fields(&v2_message).is_err());

        // 第三代：共用 + 单侧覆盖
        let v3 = serde_json::json!({
            "bot-apps": [{
                "name": "alpha",
                "uri": "ws://127.0.0.1:8080/onebot/v11/ws",
                "mode": "blacklist",
                "message": { "mode": "on" },
                "private": { "mode": "whitelist" }
            }]
        });
        assert!(detect_legacy_fields(&v3).is_ok());
    }
}
