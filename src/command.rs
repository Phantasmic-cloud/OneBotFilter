//! 内置管理指令与状态文案。
//!
//! 私聊相关操作不新增顶层指令，统一在原有指令后面加 `private` 参数：
//! - `@bot /enable <qq> private` / `@bot /disable <qq> private`：全局私聊白名单
//! - `/<bot> on|off <qq> private`：该 bot 的私聊用户名单
//! - `/group status private`：全局私聊白名单列表
//! - `/group <bot> private`：某 bot 的私聊详情

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::api;
use crate::client::measure_client_latency;
use crate::config::BotAppsConfig;
use crate::global::{App, BotConnStat, GROUP, WHITELIST};
use crate::message::{OutboundMessage, Segment};
use crate::onebot_api::{self, SendGroupMsg};

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

    // 末位 `private` 参数：私聊相关操作
    let private_flag = args
        .last()
        .map(|a| a.eq_ignore_ascii_case("private"))
        .unwrap_or(false);
    let args_no_private: &[&str] = if private_flag {
        &args[..args.len().saturating_sub(1)]
    } else {
        args
    };

    // ---- 识别是否自带指令形态 ----
    let is_enable_disable = cmd == "enable" || cmd == "disable";
    let is_bu = cmd == "bu";
    let is_bot = cmd == "bot";
    let is_group = cmd == "group";

    // /enable <qq> private / /disable <qq> private
    let is_private_user_switch = is_enable_disable && private_flag && args_no_private.len() == 1;

    // /<bot> on|off <qq> private
    let bot_on_off_private = parts.len() >= 2
        && app.find_bot_app(parts[0]).is_some()
        && (parts[1].eq_ignore_ascii_case("on") || parts[1].eq_ignore_ascii_case("off"))
        && private_flag
        && args_no_private.len() == 2
        && args_no_private[0].eq_ignore_ascii_case(parts[1])
        && args_no_private[1]
            .parse::<i64>()
            .map(|v| v > 0)
            .unwrap_or(false);

    // /<bot> on|off（当前群）
    let is_bot_on_off = parts.len() >= 2
        && !private_flag
        && (parts[1].eq_ignore_ascii_case("on") || parts[1].eq_ignore_ascii_case("off"))
        && app.find_bot_app(parts[0]).is_some();

    // /group status private / /group <bot> private
    let is_private_query = is_group && private_flag && args_no_private.len() == 1;

    if is_enable_disable && !at_me {
        // 没 @ 不当自带指令，放行给 bot
        return None;
    }

    let needs_super = is_enable_disable || is_bu || is_group || is_bot_on_off || bot_on_off_private;

    let is_builtin =
        is_enable_disable || is_bu || is_bot || is_group || is_bot_on_off || bot_on_off_private;
    if !is_builtin {
        return None;
    }

    if needs_super && !su {
        return Some(String::new()); // 静默
    }

    // enable/disable 与私聊相关的管理/查询不要求当前群已启用
    let private_related = is_private_user_switch || bot_on_off_private || is_private_query;
    if !is_enable_disable && !private_related && !app.store.is_group_enabled(cur_group) {
        return Some(String::new()); // 静默
    }

    if is_private_user_switch {
        return Some(handle_private_user_switch(
            &app,
            args_no_private[0],
            cmd == "enable",
        ));
    }

    if bot_on_off_private {
        return Some(handle_bot_private_switch(
            &app,
            parts[0],
            parts[1].eq_ignore_ascii_case("on"),
            args_no_private[1],
        ));
    }

    match cmd.as_str() {
        "enable" | "disable" => {
            let target = resolve_target_group(&app, cur_group, args_no_private).await;
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
        "group" => {
            if is_private_query {
                Some(handle_group_private_query(&app, args_no_private[0]).await)
            } else {
                Some(handle_group_command(&app, args).await)
            }
        }
        _ => {
            if is_bot_on_off {
                let cfg = app.find_bot_app(parts[0])?;
                let enable = parts[1].eq_ignore_ascii_case("on");
                if let Err(e) =
                    app.store
                        .set_bot_group(&cfg.name, &cfg.mode.group, cur_group, enable)
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

fn parse_positive_qq(s: &str) -> Option<i64> {
    s.parse::<i64>().ok().filter(|v| *v > 0)
}

fn handle_private_user_switch(app: &App, qq: &str, enable: bool) -> String {
    let Some(qq) = parse_positive_qq(qq) else {
        return "qq 无效".to_string();
    };
    if enable {
        match app.store.enable_private_user(qq) {
            Ok(true) => format!("已启用私聊用户 {}", qq),
            Ok(false) => format!("{} 已在私聊白名单", qq),
            Err(e) => format!("enable 失败: {}", e),
        }
    } else {
        match app.store.disable_private_user(qq) {
            Ok(true) => format!("已禁用私聊用户 {}", qq),
            Ok(false) => format!("{} 不在私聊白名单", qq),
            Err(e) => format!("disable 失败: {}", e),
        }
    }
}

fn handle_bot_private_switch(app: &App, bot_name: &str, enable: bool, qq: &str) -> String {
    let Some(cfg) = app.find_bot_app(bot_name) else {
        return String::new();
    };
    let Some(qq) = parse_positive_qq(qq) else {
        return "qq 无效".to_string();
    };
    match app
        .store
        .set_bot_private_user(&cfg.name, &cfg.mode.private, qq, enable)
    {
        Ok(true) => {
            if enable {
                format!("已允许 {} 私聊 {}", qq, cfg.name)
            } else {
                format!("已禁止 {} 私聊 {}", qq, cfg.name)
            }
        }
        Ok(false) => {
            if enable {
                format!("{} 已允许私聊 {}", qq, cfg.name)
            } else {
                format!("{} 已禁止私聊 {}", qq, cfg.name)
            }
        }
        Err(e) => e,
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

async fn handle_group_private_query(app: &Arc<App>, arg: &str) -> String {
    if arg.eq_ignore_ascii_case("status") {
        return build_group_status_private(app);
    }
    match app.find_bot_app(arg) {
        Some(cfg) => build_group_private_detail(app, &cfg),
        None => String::new(),
    }
}

fn build_group_status_private(app: &Arc<App>) -> String {
    let users = app.store.snapshot_private_users();
    let mut b = String::from("【已启用的私聊用户】\n");
    for qq in &users {
        b.push_str(&format!("{}\n", qq));
    }
    b.trim_end_matches('\n').to_string()
}

fn build_group_private_detail(app: &Arc<App>, cfg: &BotAppsConfig) -> String {
    let rule = if cfg.mode.private == WHITELIST {
        "私聊白名单"
    } else {
        "私聊黑名单"
    };
    let users = app.store.snapshot_private_users();
    let mut allowed = Vec::new();
    let mut denied = Vec::new();
    for qq in users {
        if app
            .store
            .is_bot_private_allowed(&cfg.name, &cfg.mode.private, qq)
        {
            allowed.push(qq);
        } else {
            denied.push(qq);
        }
    }

    let mut b = String::new();
    b.push_str(&format!("「 {} 」私聊\n", cfg.name));
    b.push_str(&format!("名单模式: {}\n", rule));
    b.push_str(&format!("开启{}私聊的用户有:\n", cfg.name));
    for qq in &allowed {
        b.push_str(&format!("{}\n", qq));
    }
    if allowed.is_empty() {
        b.push('\n');
    }
    b.push_str(&format!("\n关闭{}私聊的用户有:\n", cfg.name));
    for qq in &denied {
        b.push_str(&format!("{}\n", qq));
    }
    b.push('\n');

    let mc = &cfg.message.private;
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
    b.push_str(&format!("私聊消息 前缀: {}\n", prefix_str));
    b.push_str(&format!("私聊消息 模式: {}\n", mode_str));
    b.push_str(&format!("私聊消息 过滤: {}", filter_str));
    b
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

    let rule = if cfg.mode.group == WHITELIST {
        "群白名单"
    } else {
        "群黑名单"
    };

    let mut on: Vec<i64> = Vec::new();
    let mut off: Vec<i64> = Vec::new();
    for gid in &joined {
        if app
            .store
            .is_bot_group_allowed(&cfg.name, &cfg.mode.group, *gid)
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
            .is_bot_group_allowed(&bot.name, &bot.mode.group, group_id)
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
        .is_bot_group_allowed(&cfg.name, &cfg.mode.group, group_id);
    if local_on {
        b.push_str("本群状态: 启用\n");
    } else {
        b.push_str("本群状态: 禁用\n");
    }

    b.push_str(&format!(
        "名单模式: group={} / private={}\n",
        cfg.mode.group, cfg.mode.private
    ));

    let mc = &cfg.message.group;
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
pub fn reply_group_text(app: Arc<App>, group_id: i64, message_id: i64, text: &str) {
    if text.is_empty() {
        return;
    }
    let mut segs: Vec<Segment> = Vec::new();
    if message_id != 0 {
        segs.push(Segment::reply(message_id));
    }
    segs.push(Segment::text(text));
    let send = SendGroupMsg {
        group_id,
        message: OutboundMessage::Segments(segs),
    };
    if let Err(e) = onebot_api::enqueue_send(&app, &send, true, None) {
        log::error!("回复群消息失败: {}", e);
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

    #[test]
    fn parses_positive_qq() {
        assert_eq!(parse_positive_qq("123456789"), Some(123456789));
        assert_eq!(parse_positive_qq("0"), None);
        assert_eq!(parse_positive_qq("-1"), None);
        assert_eq!(parse_positive_qq("abc"), None);
    }
}
