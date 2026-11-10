//! 全局常量与共享状态。
//!
//! Go 版用包级变量保存配置、连接和计数器；Rust 版把同样的状态收进 [`App`]，
//! 由 `main` / 集成测试创建一份，用 `Arc` 共享给各个任务。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use arc_swap::ArcSwap;
use parking_lot::{Mutex, RwLock};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::config::{BotAppsConfig, Config};
use crate::filters::Filter;
use crate::rewrite::RewriteTracker;
use crate::store::Store;

pub const DEFAULT: &str = "default";
pub const ON: &str = "on";
pub const OFF: &str = "off";
pub const WHITELIST: &str = "whitelist";
pub const BLACKLIST: &str = "blacklist";

pub const PRIVATE: &str = "private";
pub const GROUP: &str = "group";

pub const MESSAGE_FORMAT_ARRAY: &str = "array";
pub const MESSAGE_FORMAT_STRING: &str = "string";
pub const MESSAGE_TYPE_TEXT: &str = "text";

/// 上游写出一帧，写成功后按标记计数。
pub struct UpstreamWrite {
    pub msg: Message,
    pub inc_onebot_send: bool,
    pub inc_bot_send: Option<String>,
}

/// 下游写出一帧，写成功后按标记计 bot 收。
pub struct ClientWrite {
    pub msg: Message,
    pub inc_bot_recv: bool,
}

#[derive(Debug, Clone, Default)]
pub struct BotConnStat {
    pub ever_connected: bool,
    /// 当前连接建立时间；None 表示断开。
    pub connected_at: Option<SystemTime>,
    pub disconnected_at: Option<SystemTime>,
    pub last_latency: Duration,
}

pub struct UpstreamConn {
    pub tx: Option<mpsc::UnboundedSender<UpstreamWrite>>,
    /// 当前连接建立时刻，用于 uptime。
    pub connected_since: Option<Instant>,
    /// 当前连接建立时刻（墙上时间），用于「建连时间」。
    pub connected_at: Option<SystemTime>,
    pub last_latency: Duration,
    /// 本程序发起的 API 调用：echo -> 回包投递。
    pub pending: HashMap<String, tokio::sync::oneshot::Sender<Vec<u8>>>,
}

impl Default for UpstreamConn {
    fn default() -> Self {
        Self {
            tx: None,
            connected_since: None,
            connected_at: None,
            last_latency: Duration::ZERO,
            pending: HashMap::new(),
        }
    }
}

/// 向 ping 任务要一次延迟测量，返回 Ok(延迟) / Err(原因)。
pub type LatencyRequest = tokio::sync::oneshot::Sender<Result<Duration, String>>;

pub struct Client {
    pub name: String,
    pub filter: Arc<RwLock<Filter>>,
    pub tx: mpsc::UnboundedSender<ClientWrite>,
    pub latency: mpsc::UnboundedSender<LatencyRequest>,
}

/// 进程级共享状态。
pub struct App {
    config: ArcSwap<Config>,
    pub store: Arc<Store>,
    pub filters: RwLock<Vec<Arc<RwLock<Filter>>>>,
    pub clients: RwLock<Vec<Arc<Client>>>,
    pub upstream: RwLock<UpstreamConn>,
    pub conn_stats: RwLock<HashMap<String, BotConnStat>>,
    pub bot_traffic: RwLock<HashMap<String, Arc<BotTraffic>>>,
    pub rewrite: Mutex<RewriteTracker>,
    pub onebot_recv: AtomicU64,
    pub onebot_send: AtomicU64,
    /// 上游只有一条连接：握手阶段用 CAS 占坑，避免两个连接同时通过检查。
    upstream_slot: AtomicBool,
}

#[derive(Default)]
pub struct BotTraffic {
    pub recv: AtomicU64,
    pub send: AtomicU64,
}

impl App {
    pub fn new(cfg: Config, store: Store) -> Self {
        let bots = cfg
            .bot_apps
            .iter()
            .map(|b| b.name.clone())
            .collect::<Vec<_>>();
        let mut bot_traffic = HashMap::new();
        let mut conn_stats = HashMap::new();
        for name in bots {
            bot_traffic.insert(name.clone(), Arc::new(BotTraffic::default()));
            conn_stats.insert(name, BotConnStat::default());
        }
        Self {
            config: ArcSwap::from_pointee(cfg),
            store: Arc::new(store),
            filters: RwLock::new(Vec::new()),
            clients: RwLock::new(Vec::new()),
            upstream: RwLock::new(UpstreamConn::default()),
            conn_stats: RwLock::new(conn_stats),
            bot_traffic: RwLock::new(bot_traffic),
            rewrite: Mutex::new(RewriteTracker::default()),
            onebot_recv: AtomicU64::new(0),
            onebot_send: AtomicU64::new(0),
            upstream_slot: AtomicBool::new(false),
        }
    }

    pub fn config(&self) -> Arc<Config> {
        self.config.load_full()
    }

    pub fn set_config(&self, cfg: Config) {
        self.config.store(Arc::new(cfg));
    }

    /// 找配置里的 bot-app，先精确 name，再大小写不敏感 name，最后 alias。
    pub fn find_bot_app(&self, name: &str) -> Option<BotAppsConfig> {
        if name.is_empty() {
            return None;
        }
        let cfg = self.config();
        if let Some(b) = cfg.bot_apps.iter().find(|b| b.name == name) {
            return Some(b.clone());
        }
        if let Some(b) = cfg
            .bot_apps
            .iter()
            .find(|b| b.name.eq_ignore_ascii_case(name))
        {
            return Some(b.clone());
        }
        cfg.bot_apps
            .iter()
            .find(|b| {
                b.alias
                    .iter()
                    .any(|a| a == name || a.eq_ignore_ascii_case(name))
            })
            .cloned()
    }

    pub fn add_filter(&self, filter: Arc<RwLock<Filter>>) {
        let name = filter.read().name.clone();
        let mut filters = self.filters.write();
        if filters.iter().any(|f| f.read().name == name) {
            return;
        }
        filters.push(filter);
    }

    pub fn remove_filter(&self, name: &str) {
        self.filters.write().retain(|f| f.read().name != name);
    }

    /// 配置热更后重编译已存在的过滤器（与 Go 版 ReLoadFilters 一致：只处理已存在的）。
    pub fn reload_filters(&self) {
        let cfg = self.config();
        for bot in &cfg.bot_apps {
            let mut bot = bot.clone();
            if let Err(e) = bot.check() {
                log::error!("bot {} 的配置文件校验失败：{}", bot.name, e);
                continue;
            }
            for filter in self.filters.read().iter() {
                if filter.read().name == bot.name {
                    filter.write().compile(&bot);
                    let filter_text = filter.read().to_string();
                    log::info!("已重新加载过滤器：{}", filter_text);
                    break;
                }
            }
        }
        log::info!("重新加载过滤器，共有{}个", self.filters.read().len());
    }

    pub fn snapshot_clients(&self) -> Vec<Arc<Client>> {
        self.clients.read().clone()
    }

    /// 加入连接列表；同名已存在时返回 false（对应 Go 版 AddWsClient 报错）。
    pub fn add_client(&self, client: Arc<Client>) -> bool {
        let mut clients = self.clients.write();
        if clients.iter().any(|c| c.name == client.name) {
            return false;
        }
        clients.push(client);
        true
    }

    pub fn remove_client(&self, name: &str) {
        self.clients.write().retain(|c| c.name != name);
    }

    pub fn find_client(&self, name: &str) -> Option<Arc<Client>> {
        self.clients.read().iter().find(|c| c.name == name).cloned()
    }

    pub fn is_onebot_connected(&self) -> bool {
        self.upstream.read().tx.is_some()
    }

    /// 尝试占用唯一的上游连接位；成功返回 true。
    pub fn try_acquire_upstream(&self) -> bool {
        self.upstream_slot
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    pub fn release_upstream(&self) {
        self.upstream_slot.store(false, Ordering::SeqCst);
    }

    pub fn inc_onebot_recv(&self) {
        self.onebot_recv.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_onebot_send(&self) {
        self.onebot_send.fetch_add(1, Ordering::Relaxed);
    }

    pub fn onebot_traffic(&self) -> (u64, u64) {
        (
            self.onebot_recv.load(Ordering::Relaxed),
            self.onebot_send.load(Ordering::Relaxed),
        )
    }

    pub fn reset_onebot_traffic(&self) {
        self.onebot_recv.store(0, Ordering::Relaxed);
        self.onebot_send.store(0, Ordering::Relaxed);
    }

    pub fn bot_traffic(&self, name: &str) -> (u64, u64) {
        let map = self.bot_traffic.read();
        match map.get(name) {
            Some(t) => (
                t.recv.load(Ordering::Relaxed),
                t.send.load(Ordering::Relaxed),
            ),
            None => (0, 0),
        }
    }

    pub fn reset_bot_traffic(&self, name: &str) {
        let mut map = self.bot_traffic.write();
        let entry = map
            .entry(name.to_string())
            .or_insert_with(|| Arc::new(BotTraffic::default()));
        entry.recv.store(0, Ordering::Relaxed);
        entry.send.store(0, Ordering::Relaxed);
    }

    pub fn inc_bot_recv(&self, name: &str) {
        if name.is_empty() {
            return;
        }
        let mut map = self.bot_traffic.write();
        let entry = map
            .entry(name.to_string())
            .or_insert_with(|| Arc::new(BotTraffic::default()));
        entry.recv.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_bot_send(&self, name: &str) {
        if name.is_empty() {
            return;
        }
        let mut map = self.bot_traffic.write();
        let entry = map
            .entry(name.to_string())
            .or_insert_with(|| Arc::new(BotTraffic::default()));
        entry.send.fetch_add(1, Ordering::Relaxed);
    }

    pub fn get_or_create_stat(&self, name: &str) -> BotConnStat {
        let mut stats = self.conn_stats.write();
        stats.entry(name.to_string()).or_default().clone()
    }

    pub fn update_stat<F: FnOnce(&mut BotConnStat)>(&self, name: &str, f: F) {
        let mut stats = self.conn_stats.write();
        f(stats.entry(name.to_string()).or_default());
    }

    pub fn mark_bot_connected(&self, name: &str) {
        self.update_stat(name, |s| {
            s.ever_connected = true;
            s.connected_at = Some(SystemTime::now());
            s.disconnected_at = None;
        });
    }

    pub fn mark_bot_disconnected(&self, name: &str) {
        self.update_stat(name, |s| {
            s.disconnected_at = Some(SystemTime::now());
            s.connected_at = None;
        });
    }
}

static APP: std::sync::OnceLock<Arc<App>> = std::sync::OnceLock::new();

pub fn set_global(app: Arc<App>) {
    let _ = APP.set(app);
}

pub fn global() -> Arc<App> {
    APP.get().expect("App 未初始化").clone()
}

pub fn try_global() -> Option<Arc<App>> {
    APP.get().cloned()
}
