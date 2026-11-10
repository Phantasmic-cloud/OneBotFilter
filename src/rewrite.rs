//! 出站文本改写、改写记录与回包正文抹除（对应 Go 版 `rewrite.go`）。

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use fancy_regex::Regex;
use serde_json::{json, Map, Value};

use crate::config::RewriteTextRule;
use crate::filters::Filter;
use crate::global::App;
use crate::message::OutboundMessage;
use crate::onebot_message::{MessageContent, OneBotMessage};

pub const REWRITE_TTL: Duration = Duration::from_secs(90);

pub struct CompiledRewrite {
    pub original: String,
    pub replace: String,
    pub re: Regex,
}

#[derive(Debug, Clone)]
pub struct RewriteRecord {
    pub bot_name: String,
    pub expire: Instant,
    pub echo: String,
    pub message_id: i64,
}

#[derive(Default)]
pub struct RewriteTracker {
    echo: HashMap<String, RewriteRecord>,
    msg_id: HashMap<i64, RewriteRecord>,
    fp: HashMap<String, RewriteRecord>,
}

pub fn compile_rewrite_rules(rules: &[RewriteTextRule]) -> Vec<CompiledRewrite> {
    let mut out = Vec::with_capacity(rules.len());
    for rule in rules {
        if rule.original.is_empty() {
            continue;
        }
        match Regex::new(&rule.original) {
            Ok(re) => out.push(CompiledRewrite {
                original: rule.original.clone(),
                replace: rule.replace.clone(),
                re,
            }),
            Err(e) => log::error!("rewrite-text 正则编译失败 {:?}: {}", rule.original, e),
        }
    }
    out
}

pub fn apply_rewrite_rules(s: &str, rules: &[CompiledRewrite]) -> (String, bool) {
    if s.is_empty() || rules.is_empty() {
        return (s.to_string(), false);
    }
    let mut out = s.to_string();
    for rule in rules {
        out = rule
            .re
            .replace_all(&out, rule.replace.as_str())
            .into_owned();
    }
    let changed = out != s;
    (out, changed)
}

impl Filter {
    /// 改写 bot → OneBot 的发言类 API 正文；返回 None 表示不需要改写。
    pub fn rewrite_outbound(&self, app: &App, raw: &[u8]) -> Option<Vec<u8>> {
        if self.rewrite.is_empty() || raw.is_empty() {
            return None;
        }
        let mut root: Map<String, Value> = serde_json::from_slice(raw).ok()?;
        let params_raw = root.get("params")?;
        let mut params: Map<String, Value> = params_raw.as_object()?.clone();
        let msg_raw = params.get("message")?;

        let (new_msg, fps) = rewrite_message_field(msg_raw, &self.rewrite)?;
        params.insert("message".to_string(), new_msg);
        let echo = root.get("echo").map(json_raw_to_string).unwrap_or_default();
        root.insert("params".to_string(), Value::Object(params));
        let out = serde_json::to_vec(&Value::Object(root)).ok()?;
        remember_rewrite(app, &self.name, &echo, &fps);
        if app.config().server.debug {
            log::info!(
                "rewrite-text: bot {} 已改写出站文本 echo={}",
                self.name,
                echo
            );
        }
        Some(out)
    }
}

fn rewrite_message_field(msg: &Value, rules: &[CompiledRewrite]) -> Option<(Value, Vec<String>)> {
    let message = OutboundMessage::from_json(msg)?;
    match message {
        OutboundMessage::Segments(mut segs) => {
            let mut changed = false;
            let mut texts: Vec<String> = Vec::new();
            for seg in segs.iter_mut() {
                if seg.ty != crate::global::MESSAGE_TYPE_TEXT || seg.data.is_empty() {
                    continue;
                }
                let t = match seg.data.get("text").and_then(|v| v.as_str()) {
                    Some(t) => t.to_string(),
                    None => continue,
                };
                let (nt, ok) = apply_rewrite_rules(&t, rules);
                let t = if ok {
                    seg.data
                        .insert("text".to_string(), Value::String(nt.clone()));
                    changed = true;
                    nt
                } else {
                    t
                };
                if !t.is_empty() {
                    texts.push(t);
                }
            }
            if !changed {
                return None;
            }
            let value = serde_json::to_value(&segs).ok()?;
            let concat = texts.join("");
            Some((value, rewrite_fingerprints(&concat, "")))
        }
        OutboundMessage::Text(s) => {
            let (ns, ok) = apply_rewrite_rules(&s, rules);
            if !ok {
                return None;
            }
            Some((Value::String(ns.clone()), rewrite_fingerprints(&ns, "")))
        }
    }
}

fn remember_rewrite(app: &App, bot_name: &str, echo: &str, fps: &[String]) {
    let now = Instant::now();
    let rec = RewriteRecord {
        bot_name: bot_name.to_string(),
        expire: now + REWRITE_TTL,
        echo: echo.to_string(),
        message_id: 0,
    };
    let mut tracker = app.rewrite.lock();
    prune_locked(&mut tracker, now);
    if !echo.is_empty() {
        tracker.echo.insert(echo.to_string(), rec.clone());
    }
    for fp in fps {
        if fp.is_empty() {
            continue;
        }
        tracker.fp.insert(fp.clone(), rec.clone());
    }
}

fn prune_locked(tracker: &mut RewriteTracker, now: Instant) {
    tracker.echo.retain(|_, r| now < r.expire);
    tracker.fp.retain(|_, r| now < r.expire);
    tracker.msg_id.retain(|_, r| now < r.expire);
}

/// 改写过的发言，其 API 回包只保留 message_id，不再回传正文。
pub fn sanitize_rewritten_api_response(app: &App, raw: &[u8]) -> Vec<u8> {
    let mut root: Map<String, Value> = match serde_json::from_slice(raw) {
        Ok(v) => v,
        Err(_) => return raw.to_vec(),
    };
    if root
        .get("post_type")
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false)
    {
        return raw.to_vec();
    }
    if !root.contains_key("retcode") && !root.contains_key("status") {
        return raw.to_vec();
    }

    let echo = root.get("echo").map(json_raw_to_string).unwrap_or_default();
    let data_raw = root.get("data").cloned().unwrap_or(Value::Null);
    let mid = extract_message_id(&data_raw);
    let fp = fingerprint_from_data_message(&data_raw);

    let now = Instant::now();
    let found = {
        let mut tracker = app.rewrite.lock();
        prune_locked(&mut tracker, now);
        let mut rec = if echo.is_empty() {
            None
        } else {
            tracker.echo.get(&echo).cloned()
        };
        if rec.is_none() && mid != 0 {
            rec = tracker.msg_id.get(&mid).cloned();
        }
        if rec.is_none() && !fp.is_empty() {
            rec = tracker.fp.get(&fp).cloned();
        }
        if let Some(mut r) = rec.clone() {
            if mid != 0 {
                r.message_id = mid;
                tracker.msg_id.insert(mid, r);
            }
        }
        rec
    };
    if found.is_none() {
        return raw.to_vec();
    }

    let new_data = if mid != 0 {
        json!({ "message_id": mid })
    } else {
        json!({})
    };
    root.insert("data".to_string(), new_data);
    let out = match serde_json::to_vec(&Value::Object(root)) {
        Ok(v) => v,
        Err(_) => return raw.to_vec(),
    };
    if app.config().server.debug {
        log::info!(
            "rewrite-text: 已剥离 API 回包正文 echo={} message_id={}",
            echo,
            mid
        );
    }
    out
}

/// bot 自己上报的、已经被改写过的消息。
pub fn is_rewritten_self_report(app: &App, msg: &OneBotMessage) -> bool {
    let mid = msg.partial.message_id;
    let fps = event_fingerprints(msg);

    let found = {
        let mut tracker = app.rewrite.lock();
        prune_locked(&mut tracker, Instant::now());
        let mut rec = if mid != 0 {
            tracker.msg_id.get(&mid).cloned()
        } else {
            None
        };
        if rec.is_none() {
            for fp in &fps {
                if let Some(r) = tracker.fp.get(fp) {
                    rec = Some(r.clone());
                    break;
                }
            }
        }
        if rec.is_none() && !msg.partial.echo.is_empty() {
            rec = tracker.echo.get(&msg.partial.echo).cloned();
        }
        if let Some(mut r) = rec.clone() {
            if mid != 0 {
                r.message_id = mid;
                tracker.msg_id.insert(mid, r);
            }
        }
        rec
    };
    let rec = match found {
        Some(r) => r,
        None => return false,
    };
    let _ = rec;

    let pt = msg.partial.post_type.as_str();
    if pt == "message_sent" {
        return true;
    }
    // 少数实现把自身发送报成普通 message（不当成 API 回包）
    if pt == "message" && msg.partial.self_id != 0 && msg.partial.user_id == msg.partial.self_id {
        return true;
    }
    false
}

pub fn looks_like_api_response(raw: &[u8]) -> bool {
    let peek: Value = match serde_json::from_slice(raw) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let obj = match peek.as_object() {
        Some(o) => o,
        None => return false,
    };
    if obj
        .get("post_type")
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false)
    {
        return false;
    }
    obj.contains_key("retcode")
        || obj
            .get("status")
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false)
        || obj
            .get("echo")
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false)
}

fn event_fingerprints(msg: &OneBotMessage) -> Vec<String> {
    let mut texts: Vec<String> = Vec::new();
    for seg in &msg.partial.message_array {
        if seg.ty != crate::global::MESSAGE_TYPE_TEXT || seg.data.is_empty() {
            continue;
        }
        if let Some(t) = seg.data.get("text").and_then(|v| v.as_str()) {
            if !t.is_empty() {
                texts.push(t.to_string());
            }
        }
    }
    let mut concat = texts.join("");
    if concat.is_empty() && !msg.partial.message_string.is_empty() {
        concat = msg.partial.message_string.clone();
    }
    rewrite_fingerprints(&concat, &msg.partial.raw_message)
}

fn fingerprint_from_data_message(data_raw: &Value) -> String {
    let data = match data_raw.as_object() {
        Some(o) => o,
        None => return String::new(),
    };
    if let Some(msg_raw) = data.get("message") {
        let fps = fingerprints_from_message_raw(msg_raw);
        if let Some(first) = fps.first() {
            return first.clone();
        }
    }
    if let Some(rm) = data.get("raw_message") {
        if let Some(s) = rm.as_str() {
            return normalize_fp(s);
        }
    }
    String::new()
}

fn fingerprints_from_message_raw(msg_raw: &Value) -> Vec<String> {
    if msg_raw.is_array() {
        let segs: Vec<MessageContent> = match serde_json::from_value(msg_raw.clone()) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        let mut texts: Vec<String> = Vec::new();
        for seg in &segs {
            if seg.ty != crate::global::MESSAGE_TYPE_TEXT || seg.data.is_empty() {
                continue;
            }
            if let Some(t) = seg.data.get("text").and_then(|v| v.as_str()) {
                if !t.is_empty() {
                    texts.push(t.to_string());
                }
            }
        }
        return rewrite_fingerprints(&texts.join(""), "");
    }
    match msg_raw.as_str() {
        Some(s) => rewrite_fingerprints(s, ""),
        None => Vec::new(),
    }
}

fn rewrite_fingerprints(concat: &str, raw_message: &str) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for s in [concat, raw_message] {
        let fp = normalize_fp(s);
        if fp.is_empty() || !seen.insert(fp.clone()) {
            continue;
        }
        out.push(fp);
    }
    out
}

fn normalize_fp(s: &str) -> String {
    s.replace("\r\n", "\n").trim().to_string()
}

fn extract_message_id(data_raw: &Value) -> i64 {
    let data = match data_raw.as_object() {
        Some(o) => o,
        None => return 0,
    };
    let mid = match data.get("message_id") {
        Some(v) => v,
        None => return 0,
    };
    if let Some(n) = mid.as_i64() {
        return n;
    }
    if let Some(s) = mid.as_str() {
        let mut ok = !s.is_empty();
        for c in s.chars() {
            if !c.is_ascii_digit() {
                ok = false;
                break;
            }
        }
        if ok {
            return s.parse().unwrap_or(0);
        }
    }
    0
}

fn json_raw_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        _ => v.to_string().trim().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        BotAppsConfig, BotMessageConfig, Config, MessageConfig, ModeConfig, RewriteTextRule,
    };
    use crate::global::App;
    use crate::onebot_message::parse;
    use crate::store::Store;

    fn test_app() -> App {
        let dir = std::env::temp_dir().join(format!(
            "obf-rewrite-test-{}-{}",
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
        let store = Store::load(dir.join("db.json").to_str().unwrap(), &cfg).unwrap();
        App::new(cfg, store)
    }

    fn test_filter(rules: Vec<RewriteTextRule>) -> Filter {
        let mut cfg = BotAppsConfig {
            name: "alpha".into(),
            uri: "ws://127.0.0.1:8080/onebot/v11/ws".into(),
            mode: ModeConfig {
                group: crate::global::BLACKLIST.into(),
                private: crate::global::WHITELIST.into(),
            },
            message: BotMessageConfig {
                group: MessageConfig::default(),
                private: MessageConfig::default(),
            },
            rewrite_text: rules,
            ..Default::default()
        };
        cfg.check().unwrap();
        let mut f = Filter::new("alpha");
        f.compile(&cfg);
        f
    }

    #[test]
    fn rewrite_rules_apply() {
        let rules = compile_rewrite_rules(&[
            RewriteTextRule {
                original: r"v[0-9]+\.[0-9]+\.[0-9]+".into(),
                replace: "v1.0.0".into(),
            },
            RewriteTextRule {
                original: "牛牛".into(),
                replace: "哞哞".into(),
            },
        ]);
        assert_eq!(
            apply_rewrite_rules("客户端 v2.3.4 上线", &rules),
            ("客户端 v1.0.0 上线".to_string(), true)
        );
        assert_eq!(
            apply_rewrite_rules("牛牛真可爱", &rules),
            ("哞哞真可爱".to_string(), true)
        );
        assert_eq!(
            apply_rewrite_rules("没有命中", &rules),
            ("没有命中".to_string(), false)
        );
    }

    #[test]
    fn rewrite_outbound_string_and_array() {
        let app = test_app();
        let f = test_filter(vec![RewriteTextRule {
            original: r"v[0-9]+\.[0-9]+\.[0-9]+".into(),
            replace: "v1.0.0".into(),
        }]);

        let raw = r#"{"action":"send_group_msg","params":{"group_id":100,"message":"客户端 v9.9.9"},"echo":"e1"}"#.as_bytes();
        let out = f.rewrite_outbound(&app, raw).expect("应发生改写");
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["params"]["message"].as_str().unwrap(), "客户端 v1.0.0");

        let raw = r#"{"action":"send_group_msg","params":{"group_id":100,"message":[{"type":"text","data":{"text":"客户端 v9.9.9"}}]},"echo":"e2"}"#.as_bytes();
        let out = f.rewrite_outbound(&app, raw).expect("应发生改写");
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            v["params"]["message"][0]["data"]["text"].as_str().unwrap(),
            "客户端 v1.0.0"
        );
    }

    #[test]
    fn sanitize_strips_response_body() {
        let app = test_app();
        let f = test_filter(vec![RewriteTextRule {
            original: "牛牛".into(),
            replace: "哞哞".into(),
        }]);
        let req = r#"{"action":"send_group_msg","params":{"group_id":100,"message":"牛牛"},"echo":"echo-1"}"#.as_bytes();
        assert!(f.rewrite_outbound(&app, req).is_some());

        let resp = r#"{"status":"ok","retcode":0,"data":{"message_id":12345,"message":[{"type":"text","data":{"text":"牛牛"}}]},"echo":"echo-1"}"#.as_bytes();
        let got = sanitize_rewritten_api_response(&app, resp);
        let v: Value = serde_json::from_slice(&got).unwrap();
        assert!(v["data"].get("message").is_none());
        assert_eq!(v["data"]["message_id"].as_i64().unwrap(), 12345);
    }

    #[test]
    fn foreign_response_untouched() {
        let app = test_app();
        let resp =
            r#"{"status":"ok","retcode":0,"data":{"message_id":1},"echo":"other"}"#.as_bytes();
        assert_eq!(sanitize_rewritten_api_response(&app, resp), resp.to_vec());
    }

    #[test]
    fn self_report_detected() {
        let app = test_app();
        let f = test_filter(vec![RewriteTextRule {
            original: "牛牛".into(),
            replace: "哞哞".into(),
        }]);
        let req = r#"{"action":"send_group_msg","params":{"group_id":100,"message":"牛牛"},"echo":"echo-2"}"#.as_bytes();
        assert!(f.rewrite_outbound(&app, req).is_some());

        let self_msg = parse(
            r#"{"post_type":"message_sent","message_type":"group","message_format":"array","message":[{"type":"text","data":{"text":"哞哞"}}],"raw_message":"哞哞"}"#.as_bytes(),
        )
        .unwrap();
        assert!(is_rewritten_self_report(&app, &self_msg));

        let other = parse(
            r#"{"post_type":"message_sent","message_type":"group","message_format":"array","message":[{"type":"text","data":{"text":"别的"}}],"raw_message":"别的"}"#.as_bytes(),
        )
        .unwrap();
        assert!(!is_rewritten_self_report(&app, &other));
    }

    #[test]
    fn ttl_prune() {
        let app = test_app();
        remember_rewrite(&app, "alpha", "echo-old", &["正文指纹".to_string()]);
        {
            let mut tracker = app.rewrite.lock();
            for r in tracker.echo.values_mut() {
                r.expire = r.expire.checked_sub(2 * REWRITE_TTL).unwrap_or(r.expire);
            }
            for r in tracker.fp.values_mut() {
                r.expire = r.expire.checked_sub(2 * REWRITE_TTL).unwrap_or(r.expire);
            }
        }
        remember_rewrite(&app, "alpha", "echo-new", &[]);
        let tracker = app.rewrite.lock();
        assert!(!tracker.echo.contains_key("echo-old"));
    }
}
