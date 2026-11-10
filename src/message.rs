//! 双向消息模型：入站事件解析与出站 API 共用。
//!
//! OneBot v11 的 `message` 字段有两种形态：字符串（CQ 码）或消息段数组。

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    #[serde(rename = "type", default)]
    pub ty: String,
    #[serde(default)]
    pub data: Map<String, Value>,
}

impl Segment {
    pub fn text(text: impl Into<String>) -> Self {
        let mut data = Map::new();
        data.insert("text".to_string(), Value::String(text.into()));
        Self {
            ty: "text".to_string(),
            data,
        }
    }

    pub fn reply(id: i64) -> Self {
        let mut data = Map::new();
        data.insert("id".to_string(), Value::String(id.to_string()));
        Self {
            ty: "reply".to_string(),
            data,
        }
    }

    pub fn from_value(value: &Value) -> Option<Self> {
        let obj = value.as_object()?;
        let ty = obj
            .get("type")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let data = obj
            .get("data")
            .and_then(|x| x.as_object())
            .cloned()
            .unwrap_or_default();
        Some(Self { ty, data })
    }

    pub fn text_value(&self) -> Option<&str> {
        if self.ty != "text" {
            return None;
        }
        self.data.get("text").and_then(|v| v.as_str())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum OutboundMessage {
    /// 字符串形态（可含 CQ 码）。
    Text(String),
    /// 消息段数组形态。
    Segments(Vec<Segment>),
}

impl OutboundMessage {
    pub fn from_json(value: &Value) -> Option<Self> {
        if let Some(s) = value.as_str() {
            return Some(OutboundMessage::Text(s.to_string()));
        }
        serde_json::from_value::<Vec<Segment>>(value.clone())
            .ok()
            .map(OutboundMessage::Segments)
    }

    pub fn to_json(&self) -> Value {
        match self {
            OutboundMessage::Text(s) => Value::String(s.clone()),
            OutboundMessage::Segments(segs) => {
                serde_json::to_value(segs).unwrap_or(Value::Array(Vec::new()))
            }
        }
    }

    /// 提取所有可见文本（用于日志和指纹）。
    pub fn text_content(&self) -> String {
        match self {
            OutboundMessage::Text(s) => s.clone(),
            OutboundMessage::Segments(segs) => segs
                .iter()
                .filter_map(|s| s.text_value().filter(|t| !t.is_empty()))
                .collect::<Vec<_>>()
                .join(""),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_message_roundtrip() {
        let v = json!("hello");
        let msg = OutboundMessage::from_json(&v).unwrap();
        assert_eq!(msg, OutboundMessage::Text("hello".into()));
        assert_eq!(msg.to_json(), v);
        assert_eq!(msg.text_content(), "hello");
    }

    #[test]
    fn segments_roundtrip() {
        let v = json!([
            {"type": "reply", "data": {"id": "42"}},
            {"type": "text", "data": {"text": "hello"}}
        ]);
        let msg = OutboundMessage::from_json(&v).unwrap();
        assert_eq!(msg.text_content(), "hello");
        assert_eq!(msg.to_json(), v);
    }

    #[test]
    fn invalid_json_returns_none() {
        assert!(OutboundMessage::from_json(&json!(123)).is_none());
        assert!(OutboundMessage::from_json(&json!({"a": 1})).is_none());
    }
}
