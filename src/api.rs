//! 网关侧用到的高层 OneBot API：群名解析、群列表缓存、延迟测量。
//!
//! 底层 action/echo/回包统一走 `onebot_api`。

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::global::App;
use crate::onebot_api::{self, GetGroupInfo, GetGroupList, GetLoginInfo};
use crate::store::GroupInfoItem;

/// 若是本程序发起的 API 回包则投递并返回 true。
pub fn try_deliver_api_response(app: &App, raw: &[u8]) -> bool {
    onebot_api::try_deliver_response(app, raw)
}

/// 优先 API，成功则与 RAM 比对更新；失败退回缓存。
pub async fn resolve_group_name(app: &Arc<App>, group_id: i64) -> String {
    let info = onebot_api::call(
        app,
        GetGroupInfo {
            group_id,
            no_cache: false,
        },
        Duration::from_secs(3),
    )
    .await;
    if let Ok(info) = info {
        if !info.group_name.is_empty() {
            app.store
                .update_group_name_if_changed(group_id, &info.group_name);
            return info.group_name;
        }
    }
    let cached = app.store.get_group_name(group_id);
    if !cached.is_empty() {
        return cached;
    }
    group_id.to_string()
}

/// 拉 get_group_list 写缓存，并顺带清理不在列表里的群状态。
pub async fn refresh_joined_groups(app: &Arc<App>) {
    let list = match onebot_api::call(app, GetGroupList, Duration::from_secs(5)).await {
        Ok(list) => list,
        Err(_) => return,
    };
    let items: Vec<GroupInfoItem> = list
        .into_iter()
        .map(|g| GroupInfoItem {
            group_id: g.group_id,
            group_name: g.group_name,
        })
        .collect();
    app.store.set_joined_groups(&items);

    let cfg = app.config();
    if !cfg.server.prune_runtime_state {
        return;
    }
    if items.is_empty() {
        // 实现方可能因为权限/临时异常返回空列表，此时清理会误删全部状态。
        log::warn!("跳过无用状态清理：get_group_list 返回空列表");
        return;
    }
    let ids: Vec<i64> = items.iter().map(|g| g.group_id).collect();
    let report = app.store.prune_groups(&ids);
    if !report.is_empty() {
        log::info!(
            "已清理无用状态: enabled_groups={} bot_groups={} group_names={}",
            report.enabled_groups,
            report.bot_groups,
            report.group_names
        );
    }
}

/// 轻量 API 测上游 RTT。
pub async fn measure_onebot_latency(app: &Arc<App>) -> (Duration, Result<(), String>) {
    let start = Instant::now();
    let res = onebot_api::call(app, GetLoginInfo, Duration::from_secs(3)).await;
    let d = start.elapsed();
    match res {
        Ok(_) => {
            app.upstream.write().last_latency = d;
            (d, Ok(()))
        }
        Err(e) => (d, Err(e.to_string())),
    }
}
