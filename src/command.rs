//! 内置管理指令与状态文案（对应 Go 版 `command.go`）。文案逐字对齐。

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

use crate::api;
use crate::client::measure_client_latency;
use crate::config::BotAppsConfig;
use crate::global::{App, BotConnStat, UpstreamWrite, GROUP, WHITELIST};

/// 自带指令。返回 `Some(reply)` 表示已消费，不再转 bot；`None` 表示放行。
pub async fn try_handle_command(
    app: Arc<App>,
    msg: &crate::onebot_message::OneBotMessage,
) -> Option<String> {
    if msg.partial.message_type != GROUP {
        return None;
    }
    let text = msg.extract_command_text();
    if text.is_empty() {
        return None;
    }
    let prefix = {
        let p = app.config().server.command_prefix.clone();
        if p.is_empty() {
            "/".to_string()
        } else {
            p
        }
    };
    if !text.starts_with(&prefix) {
        return None;
    }
    let body = text.strip_prefix(&prefix).unwrap_or("").trim();
    if body.is_empty() {
        return None;
    }
    let parts: Vec<&str> = body.split_whitespace().collect();
    if parts.is_empty() {
        return None;
    }
    let cmd = parts[0].to_lowercase();
    let args = &parts[1..];
    let cur_group = msg.partial.group_id;
    let user_id = msg.partial.user_id;
    let bot_id = app.config().server.bot_id.clone();
    let at_me = msg.is_at_me(&bot_id);
    let su = is_superuser(&app, user_id);

    // ---- 识别是否自带指令形态 ----
    let is_enable_disable = cmd == "enable" || cmd == "disable";
    let is_bu = cmd == "bu";
    let is_bot = cmd == "bot";
    let is_group = cmd == "group";
    let is_bot_on_off = parts.len() >= 2
        && (parts[1].eq_ignore_ascii_case("on") || parts[1].eq_ignore_ascii_case("off"))
        && app.find_bot_app(parts[0]).is_some();

    if is_enable_disable && !at_me {
        // 没 @ 不当自带指令，放行给 bot
        return None;
    }

    let needs_super = is_enable_disable || is_bu || is_group || is_bot_on_off;
    // /bot 系列不需要 superuser

    let is_builtin = is_enable_disable || is_bu || is_bot || is_group || is_bot_on_off;
    if !is_builtin {
        return None;
    }

    if needs_super && !su {
        return Some(String::new()); // 静默
    }

    // enable/disable 不要求当前群 enable
    if !is_enable_disable && !app.store.is_group_enabled(cur_group) {
        return Some(String::new()); // 静默
    }

    match cmd.as_str() {
        "enable" | "disable" => {
            let target = resolve_target_group(&app, cur_group, args).await;
            if cmd == "enable" {
                if let Err(e) = app.store.enable_group(target) {
                    return Some(format!("enable 失败: {}", e));
                }
                let name = api::resolve_group_name(&app, target).await;
                Some(format!("已启用群聊 {} ({}) BOT服务", name, target))
            } else {
                if let Err(e) = app.store.disable_group(target) {
                    return Some(format!("disable 失败: {}", e));
                }
                let name = api::resolve_group_name(&app, target).await;
                Some(format!("已禁用群聊 {} ({}) BOT服务", name, target))
            }
        }
        "bu" => Some(handle_bu_command(&app, args)),
        "bot" => Some(handle_bot_command(&app, args, cur_group).await),
        "group" => Some(handle_group_command(&app, args).await),
        _ => {
            if is_bot_on_off {
                let cfg = app.find_bot_app(parts[0])?;
                let enable = parts[1].eq_ignore_ascii_case("on");
                if let Err(e) =
                    app.store
                        .set_bot_group(&cfg.name, &cfg.group_id.mode, cur_group, enable)
                {
                    return Some(e);
                }
                if enable {
                    Some(format!("已启用本群的{}", cfg.name))
                } else {
                    Some(format!("已禁用本群的{}", cfg.name))
                }
            } else {
                None
            }
        }
    }
}

/// 有群号且已加入 → 用该群；否则当前群。
async fn resolve_target_group(app: &Arc<App>, cur_group: i64, args: &[&str]) -> i64 {
    if args.is_empty() {
        return cur_group;
    }
    let gid: i64 = match args[0].parse() {
        Ok(v) if v > 0 => v,
        _ => return cur_group,
    };
    if !app.store.is_joined_group(gid) {
        api::refresh_joined_groups(app).await;
    }
    if app.store.is_joined_group(gid) {
        gid
    } else {
        cur_group
    }
}

fn handle_bu_command(app: &App, args: &[&str]) -> String {
    if args.is_empty() {
        return String::new();
    }
    let sub = args[0].to_lowercase();
    if sub == "list" {
        let list = app.store.snapshot_black_users();
        let mut b = String::from("黑名单用户列表:");
        for qq in list {
            b.push_str(&format!("\n{}", qq));
        }
        return b;
    }
    if args.len() < 2 {
        return "用法: /bu add|del <qq> 或 /bu list".to_string();
    }
    let qq: i64 = match args[1].parse() {
        Ok(v) if v > 0 => v,
        _ => return "qq 无效".to_string(),
    };
    match sub.as_str() {
        "add" => match app.store.add_black_user(qq) {
            Ok(true) => format!("已将 {} 加入全局黑用户", qq),
            Ok(false) => format!("{} 已在黑用户列表", qq),
            Err(e) => format!("bu add 失败: {}", e),
        },
        "del" => match app.store.del_black_user(qq) {
            Ok(true) => format!("已将 {} 移出全局黑用户", qq),
            Ok(false) => format!("{} 不在黑用户列表", qq),
            Err(e) => format!("bu del 失败: {}", e),
        },
        _ => "用法: /bu add|del <qq> 或 /bu list".to_string(),
    }
}

async fn handle_bot_command(app: &Arc<App>, args: &[&str], group_id: i64) -> String {
    if args.is_empty() {
        return build_bot_list(app, group_id);
    }
    let sub = args[0];
    if sub.eq_ignore_ascii_case("status") {
        return build_bot_status(app).await;
    }
    match app.find_bot_app(sub) {
        Some(cfg) => build_bot_detail(app, &cfg, group_id).await,
        None => build_bot_list(app, group_id),
    }
}

async fn handle_group_command(app: &Arc<App>, args: &[&str]) -> String {
    if args.is_empty() {
        return String::new(); // 裸 /group 静默
    }
    let sub = args[0];
    if sub.eq_ignore_ascii_case("status") {
        return build_group_status(app).await;
    }
    match app.find_bot_app(sub) {
        Some(cfg) => build_group_bot_detail(app, &cfg).await,
        None => String::new(), // bot 不存在静默
    }
}

async fn build_group_status(app: &Arc<App>) -> String {
    api::refresh_joined_groups(app).await;
    let enabled = app.store.snapshot_enabled_groups();
    let joined = app.store.snapshot_joined_groups();
    let en_set: std::collections::HashSet<i64> = enabled.iter().copied().collect();
    let mut ids: Vec<i64> = enabled.clone();
    ids.extend(joined.iter().copied());
    ensure_group_names(app, &ids).await;

    let mut b = String::from("【已启用的群聊】\n");
    if enabled.is_empty() {
        b.push('\n');
    } else {
        for id in &enabled {
            b.push_str(&app.store.format_group_line(*id));
            b.push('\n');
        }
    }
    b.push_str("\n【已禁用的群聊】\n");
    for id in &joined {
        if en_set.contains(id) {
            continue;
        }
        b.push_str(&app.store.format_group_line(*id));
        b.push('\n');
    }
    b.trim_end_matches('\n').to_string()
}

async fn build_group_bot_detail(app: &Arc<App>, cfg: &BotAppsConfig) -> String {
    api::refresh_joined_groups(app).await;
    let mut joined = app.store.snapshot_joined_groups();
    if joined.is_empty() {
        joined = app.store.snapshot_enabled_groups();
    }
    ensure_group_names(app, &joined).await;

    let rule = if cfg.group_id.mode == WHITELIST {
        "群白名单"
    } else {
        "群黑名单"
    };

    let mut on: Vec<i64> = Vec::new();
    let mut off: Vec<i64> = Vec::new();
    for gid in &joined {
        if app
            .store
            .is_bot_allowed(&cfg.name, &cfg.group_id.mode, *gid)
        {
            on.push(*gid);
        } else {
            off.push(*gid);
        }
    }

    let mut b = String::new();
    b.push_str(&format!("{}使用的规则是{}\n", cfg.name, rule));
    b.push_str(&format!("开启{}的群聊有:\n", cfg.name));
    for id in &on {
        b.push_str(&app.store.format_group_line(*id));
        b.push('\n');
    }
    if on.is_empty() {
        b.push('\n');
    }
    b.push_str(&format!("\n关闭{}的群聊有:\n", cfg.name));
    for id in &off {
        b.push_str(&app.store.format_group_line(*id));
        b.push('\n');
    }
    b.trim_end_matches('\n').to_string()
}

async fn ensure_group_names(app: &Arc<App>, ids: &[i64]) {
    for id in ids {
        if app.store.get_group_name(*id).is_empty() {
            let _ = api::resolve_group_name(app, *id).await;
        }
    }
}

fn build_bot_list(app: &Arc<App>, group_id: i64) -> String {
    let cfg = app.config();
    let mut on: Vec<String> = Vec::new();
    let mut off: Vec<String> = Vec::new();
    for bot in &cfg.bot_apps {
        if app
            .store
            .is_bot_allowed(&bot.name, &bot.group_id.mode, group_id)
        {
            on.push(bot.name.clone());
        } else {
            off.push(bot.name.clone());
        }
    }
    let mut b = String::new();
    b.push_str("本群开启的BOT:\n");
    if on.is_empty() {
        b.push('\n');
    } else {
        b.push_str(&on.join("  "));
        b.push('\n');
    }
    b.push_str("本群关闭的BOT:\n");
    if !off.is_empty() {
        b.push_str(&off.join("  "));
    }
    b.trim_end_matches('\n').to_string()
}

struct StatusRow {
    name: String,
    yaml_idx: usize,
    /// 0 online, 1 offline(ever), 2 never
    kind: u8,
    since: SystemTime,
    recv: u64,
    send: u64,
    lat: Duration,
}

async fn build_bot_status(app: &Arc<App>) -> String {
    let mut b = String::new();
    b.push_str("「OneBotFilter」\n");
    b.push_str("[ OneBotClient ]\n");
    let connected = app.is_onebot_connected() && app.upstream.read().connected_at.is_some();
    if connected {
        let connected_at = app
            .upstream
            .read()
            .connected_at
            .unwrap_or(SystemTime::now());
        let mut lat = app.upstream.read().last_latency;
        let (d, res) = api::measure_onebot_latency(app).await;
        if res.is_ok() {
            lat = d;
        }
        b.push_str(&format!("❖ 在线: {}\n", format_uptime(connected_at)));
        if !lat.is_zero() {
            b.push_str(&format!("❖ 延迟: {}\n", format_latency(lat)));
        }
        let (recv, send) = app.onebot_traffic();
        b.push_str(&format!("❖ 收{} / 发{}\n", recv, send));
    } else {
        b.push_str("❖ 未连接\n");
    }
    b.push_str("═══ BOT-Apps ═══\n");

    let cfg = app.config();
    let mut rows: Vec<StatusRow> = Vec::with_capacity(cfg.bot_apps.len());
    for (i, bot) in cfg.bot_apps.iter().enumerate() {
        let stat = app.get_or_create_stat(&bot.name);
        let client = app.find_client(&bot.name);
        let online = client.is_some() && stat.connected_at.is_some();
        let mut row = StatusRow {
            name: bot.name.clone(),
            yaml_idx: i,
            kind: 2,
            since: SystemTime::now(),
            recv: 0,
            send: 0,
            lat: Duration::ZERO,
        };
        if online {
            row.kind = 0;
            row.since = stat.connected_at.unwrap_or_else(SystemTime::now);
            let (recv, send) = app.bot_traffic(&bot.name);
            row.recv = recv;
            row.send = send;
            row.lat = stat.last_latency;
            if let Some(c) = &client {
                if let Some(d) = measure_client_latency(app, c).await {
                    row.lat = d;
                }
            }
        } else if stat.ever_connected {
            row.kind = 1;
            row.since = stat.disconnected_at.unwrap_or_else(SystemTime::now);
        }
        rows.push(row);
    }

    rows.sort_by(|a, b| {
        if a.kind != b.kind {
            return a.kind.cmp(&b.kind);
        }
        match a.kind {
            0 => {
                let sum_a = a.recv + a.send;
                let sum_b = b.recv + b.send;
                if sum_a != sum_b {
                    return sum_b.cmp(&sum_a);
                }
                if a.since != b.since {
                    return a.since.cmp(&b.since);
                }
                a.yaml_idx.cmp(&b.yaml_idx)
            }
            1 => {
                if a.since != b.since {
                    return b.since.cmp(&a.since);
                }
                a.yaml_idx.cmp(&b.yaml_idx)
            }
            _ => a.yaml_idx.cmp(&b.yaml_idx),
        }
    });

    for row in rows {
        match row.kind {
            0 => {
                let mut mark = "✔⊗";
                if row.recv > 0 && row.send > 0 {
                    mark = "✔◉";
                } else if row.recv > 0 || row.send > 0 {
                    mark = "✔◎";
                }
                b.push_str(&format!("{}[ {} ]\n", mark, row.name));
                b.push_str(&format!("在线:   收{} / 发{}\n", row.recv, row.send));
                let mut line = format_uptime(row.since);
                if !row.lat.is_zero() {
                    line.push_str(" | ");
                    line.push_str(&format_latency(row.lat));
                }
                b.push_str(&line);
                b.push('\n');
            }
            1 => {
                b.push_str(&format!("✘ ⟳[ {} ]\n", row.name));
                b.push_str(&format!("掉线: {}\n", format_uptime(row.since)));
            }
            _ => {
                b.push_str(&format!("¿  ⟳[ {} ]\n", row.name));
                b.push_str("未连接\n");
            }
        }
    }
    b.trim_end_matches('\n').to_string()
}

async fn build_bot_detail(app: &Arc<App>, cfg: &BotAppsConfig, group_id: i64) -> String {
    let stat = app.get_or_create_stat(&cfg.name);
    let mut b = String::new();
    b.push_str(&format!("「 {} 」\n", cfg.name));

    let client = app.find_client(&cfg.name);
    let online = client.is_some() && stat.connected_at.is_some();
    if online {
        let mut lat = stat.last_latency;
        if let Some(c) = &client {
            if let Some(d) = measure_client_latency(app, c).await {
                lat = d;
            }
        }
        let mut line = format!(
            "❖ 在线: {}",
            format_uptime(stat.connected_at.unwrap_or_else(SystemTime::now))
        );
        if !lat.is_zero() {
            line.push_str(" | ");
            line.push_str(&format_latency(lat));
        }
        b.push_str(&line);
        b.push('\n');
        b.push_str(&format!(
            "❖ 建连时间: {}\n",
            format_wall(stat.connected_at.unwrap_or_else(SystemTime::now))
        ));
        let (recv, send) = app.bot_traffic(&cfg.name);
        b.push_str(&format!("❖ 收{} / 发{}\n", recv, send));
    } else if stat.ever_connected {
        b.push_str(&format!(
            "❖ 掉线: {}\n",
            format_uptime(stat.disconnected_at.unwrap_or_else(SystemTime::now))
        ));
        if let Some(t) = stat.disconnected_at {
            b.push_str(&format!("❖ 断连时间: {}\n", format_wall(t)));
        }
    } else {
        b.push_str("❖ 未连接\n");
    }

    let local_on = app
        .store
        .is_bot_allowed(&cfg.name, &cfg.group_id.mode, group_id);
    if local_on {
        b.push_str("本群状态: 启用\n");
    } else {
        b.push_str("本群状态: 禁用\n");
    }

    let mc = if cfg.group_message.mode.is_empty() {
        &cfg.message
    } else {
        &cfg.group_message
    };
    let prefix_str = if mc.prefix.is_empty() {
        "无".to_string()
    } else {
        mc.prefix.join(", ")
    };
    let mode_str = if mc.mode.is_empty() {
        "无".to_string()
    } else {
        mc.mode.clone()
    };
    let filter_str = if mc.filters.is_empty() {
        "无".to_string()
    } else {
        mc.filters.join(", ")
    };
    b.push_str(&format!("前缀: {}\n", prefix_str));
    b.push_str(&format!("模式: {}\n", mode_str));
    b.push_str(&format!("过滤: {}", filter_str));
    b
}

/// `36d 3h 23m 56s` / `3h 3m 4s` / `12s`
fn format_uptime(t: SystemTime) -> String {
    let d = SystemTime::now()
        .duration_since(t)
        .unwrap_or(Duration::ZERO);
    let sec = d.as_secs() as i64;
    let days = sec / 86400;
    let sec = sec % 86400;
    let hours = sec / 3600;
    let sec = sec % 3600;
    let mins = sec / 60;
    let secs = sec % 60;
    let mut parts: Vec<String> = Vec::new();
    if days > 0 {
        parts.push(format!("{}d", days));
    }
    if hours > 0 || days > 0 {
        parts.push(format!("{}h", hours));
    }
    if mins > 0 || hours > 0 || days > 0 {
        parts.push(format!("{}m", mins));
    }
    parts.push(format!("{}s", secs));
    parts.join(" ")
}

fn format_latency(d: Duration) -> String {
    if d.is_zero() {
        return String::new();
    }
    let ms = d.as_secs_f64() * 1000.0;
    format!("{:.2}ms", ms)
}

fn format_wall(t: SystemTime) -> String {
    let utc: chrono::DateTime<chrono::Utc> = t.into();
    let local = utc.with_timezone(&chrono::Local);
    local.format("%Y-%-m-%-d %H:%M:%S").to_string()
}

fn is_superuser(app: &App, user_id: i64) -> bool {
    app.config().server.superusers.contains(&user_id)
}

/// 引用原消息回复。
pub async fn reply_group_text(app: Arc<App>, group_id: i64, message_id: i64, text: &str) {
    if text.is_empty() {
        return;
    }
    let mut segs: Vec<Value> = Vec::new();
    if message_id != 0 {
        segs.push(json!({
            "type": "reply",
            "data": { "id": message_id.to_string() },
        }));
    }
    segs.push(json!({
        "type": "text",
        "data": { "text": text },
    }));
    let payload = json!({
        "action": "send_group_msg",
        "params": { "group_id": group_id, "message": segs },
    });
    let bytes = match serde_json::to_vec(&payload) {
        Ok(b) => b,
        Err(e) => {
            log::error!("构造回复失败: {}", e);
            return;
        }
    };
    let tx = app.upstream.read().tx.clone();
    match tx {
        Some(tx) => {
            let _ = tx.send(UpstreamWrite {
                msg: Message::Text(String::from_utf8_lossy(&bytes).into_owned().into()),
                inc_onebot_send: true,
                inc_bot_send: None,
            });
        }
        None => log::error!("回复群消息失败: 没有连接到OneBot客户端"),
    }
}

/// 调试/测试用：拿到 bot 的统计快照。
pub fn bot_stat(app: &App, name: &str) -> BotConnStat {
    app.get_or_create_stat(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uptime_formats() {
        let now = SystemTime::now();
        assert_eq!(format_uptime(now), "0s");
        assert_eq!(format_latency(Duration::ZERO), "");
        assert!(format_latency(Duration::from_millis(1234)).starts_with("1234."));
        let thirty_six_days = now
            .checked_sub(Duration::from_secs(36 * 86400 + 3 * 3600 + 23 * 60 + 56))
            .unwrap();
        assert_eq!(format_uptime(thirty_six_days), "36d 3h 23m 56s");
    }
}
