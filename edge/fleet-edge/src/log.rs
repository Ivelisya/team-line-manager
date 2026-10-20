//! 结构化日志：logfmt 一行一条，写 stderr。由 systemd 启动时（JOURNAL_STREAM 存在）
//! 加 `<N>` 优先级前缀，journald 据此设置 PRIORITY。
use std::io::Write;
use std::sync::OnceLock;

static JOURNAL: OnceLock<bool> = OnceLock::new();

#[derive(Clone, Copy)]
pub enum Level {
    Err = 3,
    Warn = 4,
    Info = 6,
    Debug = 7,
}

fn journal() -> bool {
    *JOURNAL.get_or_init(|| std::env::var_os("JOURNAL_STREAM").is_some())
}

/// 把值转成 logfmt 安全的形式（含空格、引号、= 时加引号并转义）。
pub fn q(v: &str) -> String {
    if !v.is_empty() && v.chars().all(|c| !c.is_whitespace() && c != '"' && c != '=' && c != '\\') {
        v.to_string()
    } else {
        format!("{:?}", v)
    }
}

pub fn emit(level: Level, event: &str, kv: &str) {
    let lvl = match level {
        Level::Err => "error",
        Level::Warn => "warn",
        Level::Info => "info",
        Level::Debug => "debug",
    };
    let line = if journal() {
        format!("<{}>level={lvl} event={event}{}{kv}\n", level as u8, if kv.is_empty() { "" } else { " " })
    } else {
        let ts = crate::util::now_f64();
        format!("ts={ts:.3} level={lvl} event={event}{}{kv}\n", if kv.is_empty() { "" } else { " " })
    };
    let _ = std::io::stderr().lock().write_all(line.as_bytes());
}

#[macro_export]
macro_rules! info { ($ev:expr) => { $crate::log::emit($crate::log::Level::Info, $ev, "") }; ($ev:expr, $($t:tt)*) => { $crate::log::emit($crate::log::Level::Info, $ev, &format!($($t)*)) }; }
#[macro_export]
macro_rules! warn { ($ev:expr) => { $crate::log::emit($crate::log::Level::Warn, $ev, "") }; ($ev:expr, $($t:tt)*) => { $crate::log::emit($crate::log::Level::Warn, $ev, &format!($($t)*)) }; }
#[macro_export]
macro_rules! error { ($ev:expr) => { $crate::log::emit($crate::log::Level::Err, $ev, "") }; ($ev:expr, $($t:tt)*) => { $crate::log::emit($crate::log::Level::Err, $ev, &format!($($t)*)) }; }
