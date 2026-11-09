//! OneBot API 同步调用与群信息缓存（对应 Go 版 `api.go`）。

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message;

use crate::global::{App, UpstreamWrite};
use crate::store::GroupInfoItem;

pub const API_ECHO_PREFIX: &str = "obf_";

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

/// 向 OneBot 客户端发同步 API，靠 echo 等回包。
pub async fn call_onebot_api(
    app: &Arc<App>,
    action: &str,
    params: Value,
    timeout: Duration,
) -> Result<Value, String> {
    if !app.is_onebot_connected() {
        return Err("OneBot 客户端未连接".to_string());
    }
    let timeout = if timeout.is_zero() {
        Duration::from_secs(3)
    } else {
        timeout
    };
    let echo = format!(
        "{}{}",
        API_ECHO_PREFIX,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let (tx, rx) = oneshot::channel();
    let write_tx = {
        let mut upstream = app.upstream.write();
        upstream.pending.insert(echo.clone(), tx);
        match upstream.tx.clone() {
            Some(t) => t,
            None => {
                upstream.pending.remove(&echo);
                return Err("OneBot 客户端未连接".to_string());
            }
        }
    };

    let req = json!({ "action": action, "params": params, "echo": echo });
    let bytes = serde_json::to_vec(&req).map_err(|e| e.to_string())?;
    if write_tx
        .send(UpstreamWrite {
            msg: Message::Text(String::from_utf8_lossy(&bytes).into_owned().into()),
            inc_onebot_send: false,
            inc_bot_send: None,
        })
        .is_err()
    {
        app.upstream.write().pending.remove(&echo);
        return Err("OneBot 客户端未连接".to_string());
    }

    let start = Instant::now();
    let resp = match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(resp)) => resp,
        Ok(Err(_)) => {
            app.upstream.write().pending.remove(&echo);
            return Err(format!("api {} 失败: 连接已断开", action));
        }
        Err(_) => {
            app.upstream.write().pending.remove(&echo);
            return Err(format!("api {} 超时", action));
        }
    };
    app.upstream.write().last_latency = start.elapsed();

    let envelope: Envelope =
        serde_json::from_slice(&resp).map_err(|e| format!("api {} 回包解析失败: {}", action, e))?;
    if envelope.retcode != 0 && envelope.status != "ok" {
        let mut msg = envelope.wording;
        if msg.is_empty() {
            msg = envelope.message;
        }
        if msg.is_empty() {
            msg = format!("retcode={}", envelope.retcode);
        }
        return Err(format!("api {} 失败: {}", action, msg));
    }
    Ok(envelope.data)
}

/// 若是本程序发起的 API 回包则投递并返回 true。
pub fn try_deliver_api_response(app: &App, raw: &[u8]) -> bool {
    let peek: Value = match serde_json::from_slice(raw) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let echo = match peek.get("echo").and_then(|v| v.as_str()) {
        Some(e) if !e.is_empty() => e.to_string(),
        _ => return false,
    };
    if !echo.starts_with(API_ECHO_PREFIX) {
        return false;
    }
    let sender = app.upstream.write().pending.remove(&echo);
    match sender {
        Some(tx) => {
            let _ = tx.send(raw.to_vec());
            true
        }
        None => false,
    }
}

/// 优先 API，成功则与 RAM 比对更新；失败退回缓存。
pub async fn resolve_group_name(app: &Arc<App>, group_id: i64) -> String {
    let data = call_onebot_api(
        app,
        "get_group_info",
        json!({ "group_id": group_id, "no_cache": false }),
        Duration::from_secs(3),
    )
    .await;
    if let Ok(data) = data {
        if !data.is_null() {
            if let Ok(info) = serde_json::from_value::<GroupInfoItem>(data.clone()) {
                if !info.group_name.is_empty() {
                    app.store
                        .update_group_name_if_changed(group_id, &info.group_name);
                    return info.group_name;
                }
            }
            // 有的实现 group_id 在 data 里是 number，再试 map
            if let Some(map) = data.as_object() {
                if let Some(name) = map.get("group_name").and_then(|v| v.as_str()) {
                    if !name.is_empty() {
                        app.store.update_group_name_if_changed(group_id, name);
                        return name.to_string();
                    }
                }
            }
        }
    }
    let cached = app.store.get_group_name(group_id);
    if !cached.is_empty() {
        return cached;
    }
    group_id.to_string()
}

/// 拉 get_group_list 写缓存。
pub async fn refresh_joined_groups(app: &Arc<App>) {
    let data = call_onebot_api(app, "get_group_list", json!({}), Duration::from_secs(5)).await;
    let data = match data {
        Ok(d) => d,
        Err(_) => return,
    };

    let list: Vec<GroupInfoItem> = match serde_json::from_value::<Vec<GroupInfoItem>>(data.clone())
    {
        Ok(list) => list,
        Err(_) => {
            // 兼容 [{group_id, group_name}, ...] 的宽松结构
            let arr = match data.as_array() {
                Some(a) => a.clone(),
                None => return,
            };
            let mut list = Vec::new();
            for item in arr {
                let map = match item.as_object() {
                    Some(m) => m,
                    None => continue,
                };
                let group_id = map.get("group_id").map(int_from_value).unwrap_or(0);
                let group_name = map
                    .get("group_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if group_id != 0 {
                    list.push(GroupInfoItem {
                        group_id,
                        group_name,
                    });
                }
            }
            list
        }
    };
    app.store.set_joined_groups(&list);
}

fn int_from_value(v: &Value) -> i64 {
    if let Some(i) = v.as_i64() {
        return i;
    }
    if let Some(u) = v.as_u64() {
        return u as i64;
    }
    if let Some(f) = v.as_f64() {
        return f as i64;
    }
    if let Some(s) = v.as_str() {
        return s.parse().unwrap_or(0);
    }
    0
}

/// 轻量 API 测上游 RTT。
pub async fn measure_onebot_latency(app: &Arc<App>) -> (Duration, Result<(), String>) {
    let start = Instant::now();
    let res = call_onebot_api(app, "get_login_info", json!({}), Duration::from_secs(3)).await;
    let d = start.elapsed();
    match res {
        Ok(_) => {
            app.upstream.write().last_latency = d;
            (d, Ok(()))
        }
        Err(e) => (d, Err(e)),
    }
}
