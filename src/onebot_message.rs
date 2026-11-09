//! OneBot v11 消息事件的宽松解析与辅助判断（对应 Go 版 `onebotMessage.go`）。

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::global::{MESSAGE_FORMAT_ARRAY, MESSAGE_FORMAT_STRING, MESSAGE_TYPE_TEXT};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MessageContent {
    #[serde(rename = "type", default)]
    pub r#type: String,
    #[serde(default)]
    pub data: Map<String, Value>,
}

impl MessageContent {
    pub fn from_value(v: &Value) -> Option<Self> {
        let obj = v.as_object()?;
        let r#type = obj
            .get("type")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let data = obj
            .get("data")
            .and_then(|x| x.as_object())
            .cloned()
            .unwrap_or_default();
        Some(Self { r#type, data })
    }
}

#[derive(Debug, Clone, Default)]
pub struct Partial {
    pub post_type: String,
    pub message_type: String,
    pub message_format: String,
    pub message_id: i64,
    pub message_array: Vec<MessageContent>,
    pub message_string: String,
    pub user_id: i64,
    pub group_id: i64,
    pub group_name: String,
    pub self_id: i64,
    pub raw_message: String,
    pub echo: String,
}

#[derive(Debug, Clone)]
pub struct OneBotMessage {
    pub raw: Vec<u8>,
    pub partial: Partial,
    pub intact: Map<String, Value>,
}

fn str_field(map: &Map<String, Value>, key: &str) -> String {
    map.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn i64_field(map: &Map<String, Value>, key: &str) -> i64 {
    match map.get(key) {
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

fn parse_array(v: &Value) -> Option<Vec<MessageContent>> {
    let arr = v.as_array()?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        out.push(MessageContent::from_value(item)?);
    }
    Some(out)
}

fn parse_string(v: &Value) -> Option<String> {
    v.as_str().map(|s| s.to_string())
}

pub fn parse(raw: &[u8]) -> Option<OneBotMessage> {
    let intact: Map<String, Value> = serde_json::from_slice(raw).ok()?;
    let mut p = Partial {
        post_type: str_field(&intact, "post_type"),
        message_type: str_field(&intact, "message_type"),
        message_format: str_field(&intact, "message_format"),
        message_id: i64_field(&intact, "message_id"),
        user_id: i64_field(&intact, "user_id"),
        group_id: i64_field(&intact, "group_id"),
        group_name: str_field(&intact, "group_name"),
        self_id: i64_field(&intact, "self_id"),
        raw_message: str_field(&intact, "raw_message"),
        echo: str_field(&intact, "echo"),
        ..Default::default()
    };

    if let Some(message) = intact.get("message") {
        if !message.is_null() {
            match p.message_format.as_str() {
                MESSAGE_FORMAT_ARRAY => match parse_array(message) {
                    Some(arr) => p.message_array = arr,
                    None => log::error!("将{}解析为array失败", message),
                },
                MESSAGE_FORMAT_STRING => match parse_string(message) {
                    Some(s) => p.message_string = s,
                    None => log::error!("将{}解析为string失败", message),
                },
                _ => {
                    if let Some(arr) = parse_array(message) {
                        p.message_array = arr;
                        p.message_format = MESSAGE_FORMAT_ARRAY.to_string();
                    } else if let Some(s) = parse_string(message) {
                        p.message_string = s;
                        p.message_format = MESSAGE_FORMAT_STRING.to_string();
                    }
                }
            }
        }
    }

    Some(OneBotMessage {
        raw: raw.to_vec(),
        partial: p,
        intact,
    })
}

impl OneBotMessage {
    /// 判断这条消息是否 @ 了本 bot。
    pub fn is_at_me(&self, bot_id: &str) -> bool {
        let self_id = bot_id.to_string();
        if self_id.is_empty() {
            return false;
        }
        for seg in &self.partial.message_array {
            if seg.r#type != "at" {
                continue;
            }
            if qq_from_data(&seg.data) == self_id {
                return true;
            }
        }
        let cq = format!("[CQ:at,qq={}]", self_id);
        if self.partial.raw_message.contains(&cq) {
            return true;
        }
        if self.partial.message_string.contains(&cq) {
            return true;
        }
        if self.partial.self_id != 0 {
            let sid = self.partial.self_id.to_string();
            let cq_self = format!("[CQ:at,qq={}]", sid);
            if self.partial.raw_message.contains(&cq_self) {
                return true;
            }
            for seg in &self.partial.message_array {
                if seg.r#type == "at" && qq_from_data(&seg.data) == sid {
                    return true;
                }
            }
        }
        false
    }

    /// 取指令文本：array 拼 text 段，string 去 CQ at，其它用 raw_message。
    pub fn extract_command_text(&self) -> String {
        match self.partial.message_format.as_str() {
            MESSAGE_FORMAT_ARRAY => {
                let parts: Vec<String> = self
                    .partial
                    .message_array
                    .iter()
                    .filter(|seg| seg.r#type == MESSAGE_TYPE_TEXT)
                    .filter_map(|seg| {
                        let t = seg
                            .data
                            .get("text")
                            .and_then(|v| v.as_str())?
                            .trim()
                            .to_string();
                        if t.is_empty() {
                            None
                        } else {
                            Some(t)
                        }
                    })
                    .collect();
                parts.join(" ").trim().to_string()
            }
            MESSAGE_FORMAT_STRING => strip_cq_at(&self.partial.message_string).trim().to_string(),
            _ => strip_cq_at(&self.partial.raw_message).trim().to_string(),
        }
    }
}

pub fn qq_from_data(data: &Map<String, Value>) -> String {
    match data.get("qq") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(u) = n.as_u64() {
                u.to_string()
            } else {
                match n.as_f64() {
                    Some(f) => (f as i64).to_string(),
                    None => String::new(),
                }
            }
        }
        _ => String::new(),
    }
}

pub fn strip_cq_at(s: &str) -> String {
    let mut out = s.to_string();
    while let Some(start) = out.find("[CQ:at,") {
        let Some(end) = out[start..].find(']') else {
            break;
        };
        out.replace_range(start..=start + end, "");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn at_me_detection() {
        let raw = r#"{"message_type":"group","message_format":"array","message":[{"type":"at","data":{"qq":"123456789"}},{"type":"text","data":{"text":"hi"}}],"raw_message":"[CQ:at,qq=123456789] hi"}"#;
        let msg = parse(raw.as_bytes()).unwrap();
        assert!(msg.is_at_me("123456789"));
        assert!(!msg.is_at_me("987654321"));
    }

    #[test]
    fn parses_array_message() {
        let raw = br#"{"post_type":"message","message_type":"group","message_format":"array",
            "message":[{"type":"text","data":{"text":"hi"}}],"raw_message":"hi",
            "user_id":123456789,"group_id":100,"self_id":123456789,"message_id":9}"#;
        let msg = parse(raw).unwrap();
        assert_eq!(msg.partial.message_format, "array");
        assert_eq!(msg.partial.message_array.len(), 1);
        assert_eq!(
            msg.partial.message_array[0].data.get("text").unwrap(),
            &Value::String("hi".into())
        );
        assert_eq!(msg.partial.group_id, 100);
        assert_eq!(msg.partial.raw_message, "hi");
    }

    #[test]
    fn parses_string_message() {
        let raw = br#"{"message_type":"group","message_format":"string","message":"hello"}"#;
        let msg = parse(raw).unwrap();
        assert_eq!(msg.partial.message_string, "hello");
    }

    #[test]
    fn parses_format_guess() {
        let raw = br#"{"message_type":"group","message":[{"type":"text","data":{"text":"a"}}]}"#;
        let msg = parse(raw).unwrap();
        assert_eq!(msg.partial.message_format, "array");
        let raw = br#"{"message_type":"group","message":"a"}"#;
        let msg = parse(raw).unwrap();
        assert_eq!(msg.partial.message_format, "string");
    }

    #[test]
    fn strip_cq_at_works() {
        assert_eq!(strip_cq_at("x[CQ:at,qq=123]y[CQ:at,qq=456]"), "xy");
        assert_eq!(strip_cq_at("没有 CQ"), "没有 CQ");
    }

    #[test]
    fn qq_from_data_variants() {
        let mut m = Map::new();
        m.insert("qq".into(), Value::String("123".into()));
        assert_eq!(qq_from_data(&m), "123");
        let mut m = Map::new();
        m.insert("qq".into(), Value::from(456));
        assert_eq!(qq_from_data(&m), "456");
        assert_eq!(qq_from_data(&Map::new()), "");
    }
}
