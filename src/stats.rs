//! 流量计数与「发言类 API」判定（对应 Go 版 `stats.go`）。

use serde_json::Value;

/// 判断 bot → OneBot 是否为「发言类」API（只读解析，不改包）。
pub fn is_outbound_send_action(raw: &[u8]) -> bool {
    let head: Value = match serde_json::from_slice(raw) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let action = match head.get("action").and_then(|v| v.as_str()) {
        Some(a) => a.trim().to_lowercase(),
        None => return false,
    };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_actions() {
        for action in [
            "send_msg",
            "send_group_msg",
            "send_private_msg",
            "send_group_forward_msg",
            "send_guild_channel_msg",
        ] {
            let raw = format!(r#"{{"action":"{}","params":{{}}}}"#, action);
            assert!(is_outbound_send_action(raw.as_bytes()), "{action}");
        }
        assert!(is_outbound_send_action(
            br#"{"action":"custom_send_x_msg","params":{}}"#
        ));
        assert!(!is_outbound_send_action(br#"{"action":"get_group_list"}"#));
        assert!(!is_outbound_send_action(b"not json"));
    }
}
