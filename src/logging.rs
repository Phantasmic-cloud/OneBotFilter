//! 日志：对齐 Go 标准库 `log` 的输出格式 `2026/11/10 06:38:52 message`（写入 stderr）。

use std::io::Write;

use chrono::Local;
use log::{LevelFilter, Log, Metadata, Record};

struct GoLogger;

impl Log for GoLogger {
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        let ts = Local::now().format("%Y/%m/%d %H:%M:%S");
        let stderr = std::io::stderr();
        let mut lock = stderr.lock();
        let _ = writeln!(lock, "{} {}", ts, record.args());
    }

    fn flush(&self) {}
}

static LOGGER: GoLogger = GoLogger;

pub fn init() {
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(LevelFilter::Info);
}
