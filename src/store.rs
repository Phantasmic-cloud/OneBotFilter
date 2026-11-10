//! `db.json` 运行时状态。
//!
//! 第二代 schema 增加了全局私聊白名单，并把每个 bot 的名单拆成群名单 / 私聊用户名单：
//!
//! ```json
//! {
//!   "enabled_groups": [],
//!   "enabled_private_users": [],
//!   "black_users": [],
//!   "bots": { "alpha": { "groups": [], "users": [] } },
//!   "group_names": {},
//!   "joined_groups": []
//! }
//! ```
//!
//! 旧版 `bots[name].ids` 会通过 serde alias 自动读成 `groups`。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::Config;
use crate::global::{BLACKLIST, WHITELIST};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct StoreData {
    pub enabled_groups: Vec<i64>,
    /// 全局私聊白名单：只有在这个列表里的用户，私聊才会继续往下走。
    pub enabled_private_users: Vec<i64>,
    pub black_users: Vec<i64>,
    pub bots: HashMap<String, BotStoreEntry>,
    pub group_names: HashMap<String, String>,
    pub joined_groups: Vec<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BotStoreEntry {
    /// 群名单：配合 bot 的 `mode.group` 使用。
    #[serde(alias = "ids")]
    pub groups: Vec<i64>,
    /// 私聊用户名单：配合 bot 的 `mode.private` 使用。
    pub users: Vec<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct GroupInfoItem {
    pub group_id: i64,
    pub group_name: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneReport {
    pub bots: usize,
    pub enabled_groups: usize,
    pub bot_groups: usize,
    pub group_names: usize,
}

impl PruneReport {
    pub fn is_empty(&self) -> bool {
        self.bots == 0 && self.enabled_groups == 0 && self.bot_groups == 0 && self.group_names == 0
    }
}

pub struct Store {
    path: PathBuf,
    data: RwLock<StoreData>,
}

fn contains_int64(list: &[i64], id: i64) -> bool {
    list.contains(&id)
}

fn add_int64(list: &mut Vec<i64>, id: i64) {
    if !contains_int64(list, id) {
        list.push(id);
    }
}

fn remove_int64(list: &mut Vec<i64>, id: i64) {
    list.retain(|v| *v != id);
}

fn group_key(id: i64) -> String {
    id.to_string()
}

fn should_be_in_list(mode: &str, enable: bool) -> Result<bool, String> {
    match mode {
        WHITELIST => Ok(enable),
        BLACKLIST => Ok(!enable),
        _ => Err(format!("未知 mode: {}", mode)),
    }
}

fn normalize_null_fields(mut v: Value) -> Value {
    if let Some(obj) = v.as_object_mut() {
        let defaults: [(&str, Value); 6] = [
            ("enabled_groups", json!([])),
            ("enabled_private_users", json!([])),
            ("black_users", json!([])),
            ("bots", json!({})),
            ("group_names", json!({})),
            ("joined_groups", json!([])),
        ];
        for (key, def) in defaults {
            if matches!(obj.get(key), None | Some(Value::Null)) {
                obj.insert(key.to_string(), def);
            }
        }
        // 旧版 bots[name].ids 迁到 groups，user 名单补空
        if let Some(bots) = obj.get_mut("bots").and_then(|b| b.as_object_mut()) {
            for (_, entry) in bots.iter_mut() {
                let Some(map) = entry.as_object_mut() else {
                    continue;
                };
                if matches!(map.get("groups"), None | Some(Value::Null)) {
                    let groups = match map.remove("ids") {
                        Some(v) => v,
                        None => json!([]),
                    };
                    map.insert("groups".to_string(), groups);
                }
                if matches!(map.get("users"), None | Some(Value::Null)) {
                    map.insert("users".to_string(), json!([]));
                }
            }
        }
    }
    v
}

impl Store {
    pub fn load(path: &str, cfg: &Config) -> Result<Store, String> {
        let path = if path.is_empty() { "db.json" } else { path };
        let pathbuf = PathBuf::from(path);
        let data = match std::fs::read(&pathbuf) {
            Ok(raw) => {
                if raw.is_empty() {
                    StoreData::default()
                } else {
                    let value: Value = serde_json::from_slice(&raw)
                        .map_err(|e| format!("解析 {} 失败: {}", path, e))?;
                    serde_json::from_value(normalize_null_fields(value))
                        .map_err(|e| format!("解析 {} 失败: {}", path, e))?
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => StoreData::default(),
            Err(e) => return Err(e.to_string()),
        };
        let store = Store {
            path: pathbuf,
            data: RwLock::new(data),
        };
        if cfg.server.prune_runtime_state {
            let removed = store.prune_config_bots(cfg);
            if removed > 0 {
                log::info!("已清理 {} 个配置里不存在的 bot 状态", removed);
            }
        }
        store.sync_bots_from_config(cfg);
        store.persist()?;
        Ok(store)
    }

    /// 配置里的 bot 名补进状态的 bots 表（只补不删）。
    pub fn sync_bots_from_config(&self, cfg: &Config) {
        let mut data = self.data.write();
        for app in &cfg.bot_apps {
            data.bots.entry(app.name.clone()).or_default();
        }
    }

    /// 删除配置里已经不存在的 bot 条目，返回删除数量。
    pub fn prune_config_bots(&self, cfg: &Config) -> usize {
        let names: HashSet<&str> = cfg.bot_apps.iter().map(|b| b.name.as_str()).collect();
        let mut data = self.data.write();
        let before = data.bots.len();
        data.bots.retain(|name, _| names.contains(name.as_str()));
        let removed = before - data.bots.len();
        drop(data);
        if removed > 0 {
            if let Err(e) = self.persist() {
                log::error!("保存 db.json 失败: {}", e);
            }
        }
        removed
    }

    pub fn persist(&self) -> Result<(), String> {
        let data = self.data.read();
        let raw = serde_json::to_vec_pretty(&*data).map_err(|e| e.to_string())?;
        drop(data);
        let tmp = PathBuf::from(format!("{}.tmp", self.path.display()));
        std::fs::write(&tmp, &raw).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &self.path).map_err(|e| e.to_string())
    }

    pub fn is_group_enabled(&self, group_id: i64) -> bool {
        contains_int64(&self.data.read().enabled_groups, group_id)
    }

    pub fn is_private_user_enabled(&self, user_id: i64) -> bool {
        contains_int64(&self.data.read().enabled_private_users, user_id)
    }

    pub fn is_black_user(&self, user_id: i64) -> bool {
        contains_int64(&self.data.read().black_users, user_id)
    }

    pub fn enable_group(&self, group_id: i64) -> Result<bool, String> {
        let mut data = self.data.write();
        if contains_int64(&data.enabled_groups, group_id) {
            return Ok(false);
        }
        data.enabled_groups.push(group_id);
        drop(data);
        self.persist()?;
        Ok(true)
    }

    pub fn disable_group(&self, group_id: i64) -> Result<bool, String> {
        let mut data = self.data.write();
        if !contains_int64(&data.enabled_groups, group_id) {
            return Ok(false);
        }
        remove_int64(&mut data.enabled_groups, group_id);
        drop(data);
        self.persist()?;
        Ok(true)
    }

    pub fn enable_private_user(&self, user_id: i64) -> Result<bool, String> {
        let mut data = self.data.write();
        if contains_int64(&data.enabled_private_users, user_id) {
            return Ok(false);
        }
        data.enabled_private_users.push(user_id);
        drop(data);
        self.persist()?;
        Ok(true)
    }

    pub fn disable_private_user(&self, user_id: i64) -> Result<bool, String> {
        let mut data = self.data.write();
        if !contains_int64(&data.enabled_private_users, user_id) {
            return Ok(false);
        }
        remove_int64(&mut data.enabled_private_users, user_id);
        drop(data);
        self.persist()?;
        Ok(true)
    }

    pub fn add_black_user(&self, user_id: i64) -> Result<bool, String> {
        let mut data = self.data.write();
        if contains_int64(&data.black_users, user_id) {
            return Ok(false);
        }
        data.black_users.push(user_id);
        drop(data);
        self.persist()?;
        Ok(true)
    }

    pub fn del_black_user(&self, user_id: i64) -> Result<bool, String> {
        let mut data = self.data.write();
        if !contains_int64(&data.black_users, user_id) {
            return Ok(false);
        }
        remove_int64(&mut data.black_users, user_id);
        drop(data);
        self.persist()?;
        Ok(true)
    }

    fn set_bot_list(
        &self,
        bot_name: &str,
        mode: &str,
        id: i64,
        enable: bool,
        private: bool,
    ) -> Result<bool, String> {
        let should_in_list = should_be_in_list(mode, enable)?;
        let mut data = self.data.write();
        let entry = data.bots.entry(bot_name.to_string()).or_default();
        let list = if private {
            &mut entry.users
        } else {
            &mut entry.groups
        };
        let in_list = contains_int64(list, id);
        if in_list == should_in_list {
            return Ok(false);
        }
        if should_in_list {
            add_int64(list, id);
        } else {
            remove_int64(list, id);
        }
        drop(data);
        self.persist()?;
        Ok(true)
    }

    /// 群名单开关（`/<bot> on|off`）。
    pub fn set_bot_group(
        &self,
        bot_name: &str,
        mode: &str,
        group_id: i64,
        enable: bool,
    ) -> Result<bool, String> {
        self.set_bot_list(bot_name, mode, group_id, enable, false)
    }

    /// 私聊用户名单开关（`/<bot> on|off <qq> private`）。
    pub fn set_bot_private_user(
        &self,
        bot_name: &str,
        mode: &str,
        user_id: i64,
        enable: bool,
    ) -> Result<bool, String> {
        self.set_bot_list(bot_name, mode, user_id, enable, true)
    }

    pub fn is_bot_group_allowed(&self, bot_name: &str, mode: &str, group_id: i64) -> bool {
        let data = self.data.read();
        let in_list = data
            .bots
            .get(bot_name)
            .map(|e| contains_int64(&e.groups, group_id))
            .unwrap_or(false);
        match mode {
            WHITELIST => in_list,
            BLACKLIST => !in_list,
            _ => false,
        }
    }

    pub fn is_bot_private_allowed(&self, bot_name: &str, mode: &str, user_id: i64) -> bool {
        let data = self.data.read();
        let in_list = data
            .bots
            .get(bot_name)
            .map(|e| contains_int64(&e.users, user_id))
            .unwrap_or(false);
        match mode {
            WHITELIST => in_list,
            BLACKLIST => !in_list,
            _ => false,
        }
    }

    pub fn get_group_name(&self, group_id: i64) -> String {
        self.data
            .read()
            .group_names
            .get(&group_key(group_id))
            .cloned()
            .unwrap_or_default()
    }

    /// 与 RAM 不同才更新并落盘。
    pub fn update_group_name_if_changed(&self, group_id: i64, name: &str) {
        if name.is_empty() {
            return;
        }
        let changed = {
            let mut data = self.data.write();
            let key = group_key(group_id);
            if data.group_names.get(&key).map(|s| s.as_str()) == Some(name) {
                false
            } else {
                data.group_names.insert(key, name.to_string());
                true
            }
        };
        if changed {
            if let Err(e) = self.persist() {
                log::error!("保存 db.json 失败: {}", e);
            }
        }
    }

    pub fn set_joined_groups(&self, groups: &[GroupInfoItem]) {
        let mut data = self.data.write();
        let mut ids = Vec::with_capacity(groups.len());
        let mut new_set = HashSet::new();
        let mut changed = false;
        for g in groups {
            ids.push(g.group_id);
            new_set.insert(g.group_id);
            if !g.group_name.is_empty() {
                let key = group_key(g.group_id);
                if data.group_names.get(&key).map(|s| s.as_str()) != Some(g.group_name.as_str()) {
                    data.group_names.insert(key, g.group_name.clone());
                    changed = true;
                }
            }
        }
        if ids.len() != data.joined_groups.len()
            || data.joined_groups.iter().any(|id| !new_set.contains(id))
        {
            changed = true;
        }
        if !changed {
            return;
        }
        data.joined_groups = ids;
        drop(data);
        if let Err(e) = self.persist() {
            log::error!("保存 db.json 失败: {}", e);
        }
    }

    /// 清理不在 joined 列表里的群相关状态；调用方需保证 joined 非空。
    pub fn prune_groups(&self, joined: &[i64]) -> PruneReport {
        let joined_set: HashSet<i64> = joined.iter().copied().collect();
        let mut report = PruneReport::default();
        let mut data = self.data.write();

        let before = data.enabled_groups.len();
        data.enabled_groups.retain(|id| joined_set.contains(id));
        report.enabled_groups = before - data.enabled_groups.len();

        for entry in data.bots.values_mut() {
            let before = entry.groups.len();
            entry.groups.retain(|id| joined_set.contains(id));
            report.bot_groups += before - entry.groups.len();
        }

        let before = data.group_names.len();
        data.group_names.retain(|k, _| {
            k.parse::<i64>()
                .map(|id| joined_set.contains(&id))
                .unwrap_or(true)
        });
        report.group_names = before - data.group_names.len();

        drop(data);
        if !report.is_empty() {
            if let Err(e) = self.persist() {
                log::error!("保存 db.json 失败: {}", e);
            }
        }
        report
    }

    pub fn snapshot_black_users(&self) -> Vec<i64> {
        self.data.read().black_users.clone()
    }

    pub fn snapshot_enabled_groups(&self) -> Vec<i64> {
        self.data.read().enabled_groups.clone()
    }

    pub fn snapshot_private_users(&self) -> Vec<i64> {
        self.data.read().enabled_private_users.clone()
    }

    pub fn snapshot_joined_groups(&self) -> Vec<i64> {
        self.data.read().joined_groups.clone()
    }

    pub fn is_joined_group(&self, group_id: i64) -> bool {
        contains_int64(&self.data.read().joined_groups, group_id)
    }

    pub fn snapshot_bot_groups(&self, bot_name: &str) -> Vec<i64> {
        self.data
            .read()
            .bots
            .get(bot_name)
            .map(|e| e.groups.clone())
            .unwrap_or_default()
    }

    pub fn snapshot_bot_users(&self, bot_name: &str) -> Vec<i64> {
        self.data
            .read()
            .bots
            .get(bot_name)
            .map(|e| e.users.clone())
            .unwrap_or_default()
    }

    /// 优先缓存名。
    pub fn format_group_line(&self, group_id: i64) -> String {
        let name = self.get_group_name(group_id);
        if name.is_empty() {
            format!("{}({})", group_id, group_id)
        } else {
            format!("{}({})", name, group_id)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn temp_path(tag: &str) -> String {
        let dir = std::env::temp_dir().join(format!(
            "obf-store-test-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("db.json").to_str().unwrap().to_string()
    }

    fn new_store() -> Store {
        Store::load(&temp_path("basic"), &Config::default()).unwrap()
    }

    #[test]
    fn group_enable_disable() {
        let s = new_store();
        assert!(!s.is_group_enabled(100));
        assert!(s.enable_group(100).unwrap());
        assert!(s.is_group_enabled(100));
        assert!(!s.enable_group(100).unwrap());
        assert!(s.disable_group(100).unwrap());
        assert!(!s.is_group_enabled(100));
    }

    #[test]
    fn private_user_enable_disable() {
        let s = new_store();
        assert!(!s.is_private_user_enabled(123456789));
        assert!(s.enable_private_user(123456789).unwrap());
        assert!(s.is_private_user_enabled(123456789));
        assert!(!s.enable_private_user(123456789).unwrap());
        assert!(s.disable_private_user(123456789).unwrap());
        assert!(!s.is_private_user_enabled(123456789));
    }

    #[test]
    fn black_users() {
        let s = new_store();
        assert!(!s.is_black_user(123456789));
        assert!(s.add_black_user(123456789).unwrap());
        assert!(s.is_black_user(123456789));
        assert!(s.del_black_user(123456789).unwrap());
        assert!(!s.is_black_user(123456789));
    }

    #[test]
    fn bot_group_whitelist_and_blacklist() {
        let s = new_store();
        assert!(!s.is_bot_group_allowed("alpha", WHITELIST, 100));
        assert!(s.set_bot_group("alpha", WHITELIST, 100, true).unwrap());
        assert!(s.is_bot_group_allowed("alpha", WHITELIST, 100));
        assert!(s.set_bot_group("alpha", WHITELIST, 100, false).unwrap());
        assert!(!s.is_bot_group_allowed("alpha", WHITELIST, 100));

        assert!(s.is_bot_group_allowed("alpha", BLACKLIST, 100));
        assert!(s.set_bot_group("alpha", BLACKLIST, 100, false).unwrap());
        assert!(!s.is_bot_group_allowed("alpha", BLACKLIST, 100));
        assert!(s.set_bot_group("alpha", BLACKLIST, 100, true).unwrap());
        assert!(s.is_bot_group_allowed("alpha", BLACKLIST, 100));
    }

    #[test]
    fn bot_private_whitelist_and_blacklist() {
        let s = new_store();
        assert!(!s.is_bot_private_allowed("alpha", WHITELIST, 123456789));
        assert!(s
            .set_bot_private_user("alpha", WHITELIST, 123456789, true)
            .unwrap());
        assert!(s.is_bot_private_allowed("alpha", WHITELIST, 123456789));
        assert!(s
            .set_bot_private_user("alpha", WHITELIST, 123456789, false)
            .unwrap());
        assert!(!s.is_bot_private_allowed("alpha", WHITELIST, 123456789));

        assert!(s.is_bot_private_allowed("alpha", BLACKLIST, 123456789));
        assert!(s
            .set_bot_private_user("alpha", BLACKLIST, 123456789, false)
            .unwrap());
        assert!(!s.is_bot_private_allowed("alpha", BLACKLIST, 123456789));
    }

    #[test]
    fn persists_and_formats() {
        let path = temp_path("persist");
        {
            let s = Store::load(&path, &Config::default()).unwrap();
            s.enable_group(200).unwrap();
            s.enable_private_user(123456789).unwrap();
            s.update_group_name_if_changed(200, "测试群");
        }
        let s = Store::load(&path, &Config::default()).unwrap();
        assert!(s.is_group_enabled(200));
        assert!(s.is_private_user_enabled(123456789));
        assert_eq!(s.get_group_name(200), "测试群");
        assert_eq!(s.format_group_line(200), "测试群(200)");
        assert_eq!(s.format_group_line(300), "300(300)");
    }

    #[test]
    fn joined_groups() {
        let s = new_store();
        s.set_joined_groups(&[
            GroupInfoItem {
                group_id: 1,
                group_name: "一".into(),
            },
            GroupInfoItem {
                group_id: 2,
                group_name: String::new(),
            },
        ]);
        assert!(s.is_joined_group(1));
        assert!(s.is_joined_group(2));
        assert_eq!(s.get_group_name(1), "一");
    }

    fn config_with_bot(name: &str) -> Config {
        let mut cfg = Config::default();
        cfg.server.prune_runtime_state = false;
        cfg.bot_apps.push(crate::config::BotAppsConfig {
            name: name.into(),
            uri: "ws://127.0.0.1:1/ws".into(),
            mode: crate::config::ModeConfig {
                group: WHITELIST.into(),
                private: WHITELIST.into(),
            },
            ..Default::default()
        });
        cfg
    }

    #[test]
    fn migrates_legacy_ids() {
        let path = temp_path("legacy");
        std::fs::write(
            &path,
            r#"{"enabled_groups":[100],"black_users":[],"bots":{"alpha":{"ids":[100]}},"group_names":{},"joined_groups":[100]}"#,
        )
        .unwrap();
        let s = Store::load(&path, &config_with_bot("alpha")).unwrap();
        assert!(s.is_bot_group_allowed("alpha", WHITELIST, 100));
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"groups\""));
    }

    #[test]
    fn prune_groups_removes_stale() {
        let s = new_store();
        s.enable_group(100).unwrap();
        s.enable_group(999).unwrap();
        s.set_bot_group("alpha", BLACKLIST, 999, false).unwrap();
        s.update_group_name_if_changed(999, "旧群");
        let report = s.prune_groups(&[100]);
        assert_eq!(report.enabled_groups, 1);
        assert_eq!(report.bot_groups, 1);
        assert_eq!(report.group_names, 1);
        assert!(s.is_group_enabled(100));
        assert!(!s.is_group_enabled(999));
        assert_eq!(s.get_group_name(999), "");
        assert!(s.snapshot_bot_groups("alpha").is_empty());
    }

    #[test]
    fn prune_bots_removes_stale() {
        let path = temp_path("prune-bots");
        let cfg = config_with_bot("alpha");
        let s = Store::load(&path, &cfg).unwrap();
        assert_eq!(s.prune_config_bots(&cfg), 0);
        let mut empty = Config::default();
        empty.server.prune_runtime_state = false;
        assert_eq!(s.prune_config_bots(&empty), 1);
        assert!(s.snapshot_bot_groups("alpha").is_empty());
    }
}
