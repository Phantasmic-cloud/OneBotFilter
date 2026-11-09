//! `db.json` 运行时状态（对应 Go 版 `store.go`）。schema 与 Go 版完全一致。

use std::collections::HashMap;
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
    pub black_users: Vec<i64>,
    pub bots: HashMap<String, BotStoreEntry>,
    pub group_names: HashMap<String, String>,
    pub joined_groups: Vec<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BotStoreEntry {
    pub ids: Vec<i64>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct GroupInfoItem {
    pub group_id: i64,
    pub group_name: String,
}

pub struct Store {
    path: PathBuf,
    data: RwLock<StoreData>,
}

fn normalize_null_fields(mut v: Value) -> Value {
    if let Some(obj) = v.as_object_mut() {
        let defaults: [(&str, Value); 5] = [
            ("enabled_groups", json!([])),
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
    }
    v
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

    pub fn set_bot_group(
        &self,
        bot_name: &str,
        mode: &str,
        group_id: i64,
        enable: bool,
    ) -> Result<bool, String> {
        let mut data = self.data.write();
        let entry = data.bots.entry(bot_name.to_string()).or_default();
        let in_list = contains_int64(&entry.ids, group_id);
        let should_in_list = match mode {
            WHITELIST => enable,
            BLACKLIST => !enable,
            _ => return Err(format!("未知 mode: {}", mode)),
        };
        if in_list == should_in_list {
            return Ok(false);
        }
        if should_in_list {
            add_int64(&mut entry.ids, group_id);
        } else {
            remove_int64(&mut entry.ids, group_id);
        }
        drop(data);
        self.persist()?;
        Ok(true)
    }

    pub fn is_bot_allowed(&self, bot_name: &str, mode: &str, group_id: i64) -> bool {
        let data = self.data.read();
        let in_list = data
            .bots
            .get(bot_name)
            .map(|e| contains_int64(&e.ids, group_id))
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
        let mut new_set = std::collections::HashSet::new();
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

    pub fn snapshot_black_users(&self) -> Vec<i64> {
        self.data.read().black_users.clone()
    }

    pub fn snapshot_enabled_groups(&self) -> Vec<i64> {
        self.data.read().enabled_groups.clone()
    }

    pub fn snapshot_joined_groups(&self) -> Vec<i64> {
        self.data.read().joined_groups.clone()
    }

    pub fn is_joined_group(&self, group_id: i64) -> bool {
        contains_int64(&self.data.read().joined_groups, group_id)
    }

    pub fn snapshot_bot_ids(&self, bot_name: &str) -> Vec<i64> {
        self.data
            .read()
            .bots
            .get(bot_name)
            .map(|e| e.ids.clone())
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

    fn new_store() -> Store {
        let dir = std::env::temp_dir().join(format!(
            "obf-store-test-{}-{}",
            std::process::id(),
            uuid_like()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("db.json");
        Store::load(path.to_str().unwrap(), &Config::default()).unwrap()
    }

    fn uuid_like() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
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
    fn black_users() {
        let s = new_store();
        assert!(!s.is_black_user(123456789));
        assert!(s.add_black_user(123456789).unwrap());
        assert!(s.is_black_user(123456789));
        assert!(s.del_black_user(123456789).unwrap());
        assert!(!s.is_black_user(123456789));
    }

    #[test]
    fn bot_group_whitelist() {
        let s = new_store();
        assert!(!s.is_bot_allowed("alpha", WHITELIST, 100));
        assert!(s.set_bot_group("alpha", WHITELIST, 100, true).unwrap());
        assert!(s.is_bot_allowed("alpha", WHITELIST, 100));
        assert!(s.set_bot_group("alpha", WHITELIST, 100, false).unwrap());
        assert!(!s.is_bot_allowed("alpha", WHITELIST, 100));
    }

    #[test]
    fn bot_group_blacklist() {
        let s = new_store();
        assert!(s.is_bot_allowed("alpha", BLACKLIST, 100));
        assert!(s.set_bot_group("alpha", BLACKLIST, 100, false).unwrap());
        assert!(!s.is_bot_allowed("alpha", BLACKLIST, 100));
        assert!(s.set_bot_group("alpha", BLACKLIST, 100, true).unwrap());
        assert!(s.is_bot_allowed("alpha", BLACKLIST, 100));
    }

    #[test]
    fn persists_and_format() {
        let dir = std::env::temp_dir().join(format!(
            "obf-store-test-persist-{}-{}",
            std::process::id(),
            uuid_like()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("db.json");
        let p = path.to_str().unwrap();
        {
            let s = Store::load(p, &Config::default()).unwrap();
            s.enable_group(200).unwrap();
            s.update_group_name_if_changed(200, "测试群");
        }
        let s = Store::load(p, &Config::default()).unwrap();
        assert!(s.is_group_enabled(200));
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
        s.set_joined_groups(&[GroupInfoItem {
            group_id: 1,
            group_name: "一".into(),
        }]);
        assert!(!s.is_joined_group(2));
    }
}
