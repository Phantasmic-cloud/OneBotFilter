//! 统一 OneBot v11 API 层。
//!
//! 所有 action 名、参数构造、echo 关联、回包解析、超时与“发送类”判定都收在这里，
//! 其它模块不再手拼 `{"action": ...}` 或各自维护 action 列表。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message;

use crate::global::{App, UpstreamWrite};
use crate::message::OutboundMessage;

/// 本程序发起的 API 调用的 echo 前缀；上游回包据此投递，不广播给 bot。
pub const ECHO_PREFIX: &str = "obf_";

static ECHO_SEQ: AtomicU64 = AtomicU64::new(0);

pub fn new_echo() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{}{}_{}",
        ECHO_PREFIX,
        nanos,
        ECHO_SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

#[derive(Debug)]
pub enum ApiError {
    NotConnected,
    Timeout(String),
    Failed(String),
    InvalidResponse(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // 保持旧日志文案
            ApiError::NotConnected => write!(f, "没有连接到OneBot客户端"),
            ApiError::Timeout(action) => write!(f, "api {} 超时", action),
            ApiError::Failed(msg) => write!(f, "{}", msg),
            ApiError::InvalidResponse(msg) => write!(f, "{}", msg),
        }
    }
}

impl std::error::Error for ApiError {}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct GroupInfo {
    pub group_id: i64,
    pub group_name: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct LoginInfo {
    pub user_id: i64,
    pub nickname: String,
}

#[derive(Debug, Default, Deserialize)]
struct Envelope {
    #[serde(default)]
    status: String,
    #[serde(default)]
    retcode: i64,
    #[serde(default)]
    data: Value,
    #[serde(default)]
    message: String,
    #[serde(default)]
    wording: String,
}

/// 需要等回包的 OneBot API。
pub trait Action {
    type Response: DeserializeOwned;
    const NAME: &'static str;
    fn params(&self) -> Value;
}

pub struct GetGroupInfo {
    pub group_id: i64,
    pub no_cache: bool,
}

impl Action for GetGroupInfo {
    type Response = GroupInfo;
    const NAME: &'static str = "get_group_info";

    fn params(&self) -> Value {
        json!({ "group_id": self.group_id, "no_cache": self.no_cache })
    }
}

pub struct GetGroupList;

impl Action for GetGroupList {
    type Response = Vec<GroupInfo>;
    const NAME: &'static str = "get_group_list";

    fn params(&self) -> Value {
        json!({})
    }
}

pub struct GetLoginInfo;

impl Action for GetLoginInfo {
    type Response = LoginInfo;
    const NAME: &'static str = "get_login_info";

    fn params(&self) -> Value {
        json!({})
    }
}

/// 向 OneBot 实现发送的出站 API（发言类）。
pub trait SendAction {
    const NAME: &'static str;
    fn params(&self) -> Value;
}

pub struct SendGroupMsg {
    pub group_id: i64,
    pub message: OutboundMessage,
}

impl SendAction for SendGroupMsg {
    const NAME: &'static str = "send_group_msg";

    fn params(&self) -> Value {
        json!({ "group_id": self.group_id, "message": self.message.to_json() })
    }
}

pub struct SendPrivateMsg {
    pub user_id: i64,
    pub message: OutboundMessage,
}

impl SendAction for SendPrivateMsg {
    const NAME: &'static str = "send_private_msg";

    fn params(&self) -> Value {
        json!({ "user_id": self.user_id, "message": self.message.to_json() })
    }
}

/// 需要回包的调用：写请求并等待 echo 对应的回包。
pub async fn call<A: Action>(
    app: &Arc<App>,
    action: A,
    timeout: Duration,
) -> Result<A::Response, ApiError> {
    if !app.is_onebot_connected() {
        return Err(ApiError::NotConnected);
    }
    let timeout = if timeout.is_zero() {
        Duration::from_secs(3)
    } else {
        timeout
    };
    let echo = new_echo();
    let (tx, rx) = oneshot::channel();
    let write_tx = {
        let mut upstream = app.upstream.write();
        upstream.pending.insert(echo.clone(), tx);
        match upstream.tx.clone() {
            Some(t) => t,
            None => {
                upstream.pending.remove(&echo);
                return Err(ApiError::NotConnected);
            }
        }
    };

    let req = json!({ "action": A::NAME, "params": action.params(), "echo": echo });
    let bytes = serde_json::to_vec(&req).map_err(|e| ApiError::InvalidResponse(e.to_string()))?;
    if write_tx
        .send(UpstreamWrite {
            msg: Message::Text(String::from_utf8_lossy(&bytes).into_owned().into()),
            inc_onebot_send: false,
            inc_bot_send: None,
        })
        .is_err()
    {
        app.upstream.write().pending.remove(&echo);
        return Err(ApiError::NotConnected);
    }

    let start = Instant::now();
    let resp = match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(resp)) => resp,
        Ok(Err(_)) => {
            app.upstream.write().pending.remove(&echo);
            return Err(ApiError::Failed(format!(
                "api {} 失败: 连接已断开",
                A::NAME
            )));
        }
        Err(_) => {
            app.upstream.write().pending.remove(&echo);
            return Err(ApiError::Timeout(A::NAME.to_string()));
        }
    };
    app.upstream.write().last_latency = start.elapsed();

    let envelope: Envelope = serde_json::from_slice(&resp)
        .map_err(|e| ApiError::InvalidResponse(format!("api {} 回包解析失败: {}", A::NAME, e)))?;
    if envelope.retcode != 0 && envelope.status != "ok" {
        let mut msg = envelope.wording;
        if msg.is_empty() {
            msg = envelope.message;
        }
        if msg.is_empty() {
            msg = format!("retcode={}", envelope.retcode);
        }
        return Err(ApiError::Failed(format!("api {} 失败: {}", A::NAME, msg)));
    }
    serde_json::from_value(envelope.data)
        .map_err(|e| ApiError::InvalidResponse(format!("api {} 回包结构不符: {}", A::NAME, e)))
}

/// 序列化一条出站 API 请求（用于 bot → 上游透传/改写后发送）。
pub fn encode_send<A: SendAction>(action: &A) -> Result<Vec<u8>, ApiError> {
    serde_json::to_vec(&json!({
        "action": A::NAME,
        "params": action.params(),
    }))
    .map_err(|e| ApiError::InvalidResponse(e.to_string()))
}

/// 把已序列化的出站帧放进上游写队列。
pub fn enqueue_payload(
    app: &App,
    payload: Vec<u8>,
    inc_onebot_send: bool,
    inc_bot_send: Option<String>,
) -> Result<(), ApiError> {
    let tx = app
        .upstream
        .read()
        .tx
        .clone()
        .ok_or(ApiError::NotConnected)?;
    tx.send(UpstreamWrite {
        msg: Message::Text(String::from_utf8_lossy(&payload).into_owned().into()),
        inc_onebot_send,
        inc_bot_send,
    })
    .map_err(|_| ApiError::NotConnected)
}

pub fn enqueue_send<A: SendAction>(
    app: &App,
    action: &A,
    inc_onebot_send: bool,
    inc_bot_send: Option<String>,
) -> Result<(), ApiError> {
    let payload = encode_send(action)?;
    enqueue_payload(app, payload, inc_onebot_send, inc_bot_send)
}

/// 判断 bot → OneBot 的 action 是否为「发言类」。
pub fn is_send_action(action: &str) -> bool {
    let action = action.trim().to_lowercase();
    match action.as_str() {
        "send_msg"
        | "send_private_msg"
        | "send_group_msg"
        | "send_discuss_msg"
        | "send_group_forward_msg"
        | "send_private_forward_msg"
        | "send_forward_msg"
        | "send_guild_channel_msg" => true,
        _ => action.contains("send_") && action.contains("msg"),
    }
}

/// 判断一帧原始 JSON 是否为发言类 API（只读解析，不改包）。
pub fn is_send_frame(raw: &[u8]) -> bool {
    let head: Value = match serde_json::from_slice(raw) {
        Ok(v) => v,
        Err(_) => return false,
    };
    match head.get("action").and_then(|v| v.as_str()) {
        Some(action) => is_send_action(action),
        None => false,
    }
}

/// 若是本程序发起的 API 回包则投递并返回 true。
pub fn try_deliver_response(app: &App, raw: &[u8]) -> bool {
    let peek: Value = match serde_json::from_slice(raw) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let echo = match peek.get("echo").and_then(|v| v.as_str()) {
        Some(e) if e.starts_with(ECHO_PREFIX) => e.to_string(),
        _ => return false,
    };
    let sender = app.upstream.write().pending.remove(&echo);
    match sender {
        Some(tx) => {
            let _ = tx.send(raw.to_vec());
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echo_unique_and_prefixed() {
        let a = new_echo();
        let b = new_echo();
        assert!(a.starts_with(ECHO_PREFIX));
        assert_ne!(a, b);
    }

    #[test]
    fn send_action_detection() {
        for action in [
            "send_msg",
            "send_group_msg",
            "send_private_msg",
            "send_group_forward_msg",
            "send_guild_channel_msg",
        ] {
            assert!(is_send_action(action), "{action}");
        }
        assert!(is_send_frame(br#"{"action":"send_group_msg","params":{}}"#));
        assert!(!is_send_frame(br#"{"action":"get_group_list"}"#));
        assert!(!is_send_frame(b"not json"));
    }

    #[test]
    fn encode_send_group_msg() {
        let payload = encode_send(&SendGroupMsg {
            group_id: 100,
            message: OutboundMessage::Segments(vec![crate::message::Segment::text("hi")]),
        })
        .unwrap();
        let v: Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(v["action"], "send_group_msg");
        assert_eq!(v["params"]["group_id"], 100);
        assert_eq!(v["params"]["message"][0]["data"]["text"], "hi");
    }
}
