//! 主机流量计（本期累计出站 / 入站），替代 Komari 的 net_total_up / net_total_down 口径。
//!
//! nodeagent / relayd 的网卡计数（sample.net.tx_bytes / rx_bytes）是开机以来的累计值，不能直接当“本期用量”
//!（线路机1 开机 16 天、跨了一个账单周期，直接用就成了 2462 GB）。这里按资产维护一个流量计：
//! - 每 30 s 读一次计数：同一次开机且计数没有倒退 → 加差值；重启 / 计数清零 → 加新计数本身；
//! - 线路机按供应商账单周期清零：周期从到期日的“日”开始（到期 10-01 → 每月 1 日；10-13 → 13 日），按资产的账单时区；
//! - 基线：`fleetd vendor import`（供应商面板读数 + 读数时刻之后网卡出站积分）；落地节点首次从 Komari 最后同步值接上。
//! 数据在原生旧库 legacy.db 的 fleetd_meter 表；hosts 的 net_total_up / down 取自这里。
use crate::compat::py;
use chrono::{Datelike, NaiveDate, TimeZone};
use chrono_tz::Tz;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

pub const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS fleetd_meter(asset TEXT PRIMARY KEY, period_start TEXT, up REAL NOT NULL, down REAL NOT NULL,
  last_tx INTEGER, last_rx INTEGER, last_boot TEXT, updated_at REAL NOT NULL, baseline TEXT)";

#[derive(Debug, Clone)]
pub struct Meter {
    pub period_start: Option<String>,
    pub up: f64,
    pub down: f64,
    pub last_tx: Option<i64>,
    pub last_rx: Option<i64>,
    pub last_boot: Option<String>,
}

pub fn get(c: &Connection, asset: &str) -> Option<Meter> {
    c.query_row("SELECT period_start, up, down, last_tx, last_rx, last_boot FROM fleetd_meter WHERE asset=?1", [asset], |r| {
        Ok(Meter { period_start: r.get(0)?, up: r.get(1)?, down: r.get(2)?, last_tx: r.get(3)?, last_rx: r.get(4)?, last_boot: r.get(5)? })
    })
    .optional()
    .ok()
    .flatten()
}

pub fn put(c: &Connection, asset: &str, m: &Meter, now: f64, baseline: Option<&Value>) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO fleetd_meter(asset, period_start, up, down, last_tx, last_rx, last_boot, updated_at, baseline) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)
         ON CONFLICT(asset) DO UPDATE SET period_start=excluded.period_start, up=excluded.up, down=excluded.down, last_tx=excluded.last_tx,
           last_rx=excluded.last_rx, last_boot=excluded.last_boot, updated_at=excluded.updated_at, baseline=COALESCE(excluded.baseline, fleetd_meter.baseline)",
        params![asset, m.period_start, m.up, m.down, m.last_tx, m.last_rx, m.last_boot, now, baseline.map(|b| b.to_string())],
    )?;
    Ok(())
}

/// 当前账单周期的开始日（按到期日的“日”，每月；该月没有这一天就取月末），None = 没有到期日
pub fn period_start(asset: &Value, now: f64) -> Option<NaiveDate> {
    let due = asset["due_date"].as_str().and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())?;
    let tz: Tz = asset["billing_timezone"].as_str().unwrap_or("Asia/Shanghai").parse().unwrap_or(chrono_tz::Asia::Shanghai);
    let today = tz.timestamp_opt(now as i64, 0).single()?.date_naive();
    let day = due.day();
    let clamp = |y: i32, m: u32| -> Option<NaiveDate> {
        let mut d = day;
        loop {
            if let Some(x) = NaiveDate::from_ymd_opt(y, m, d) {
                return Some(x);
            }
            d -= 1;
            if d == 0 {
                return None;
            }
        }
    };
    let this = clamp(today.year(), today.month())?;
    if this <= today {
        Some(this)
    } else {
        let (y, m) = if today.month() == 1 { (today.year() - 1, 12) } else { (today.year(), today.month() - 1) };
        clamp(y, m)
    }
}

/// 周期开始时刻（账单时区 0 点）
pub fn period_start_ts(asset: &Value, d: NaiveDate) -> f64 {
    let tz: Tz = asset["billing_timezone"].as_str().unwrap_or("Asia/Shanghai").parse().unwrap_or(chrono_tz::Asia::Shanghai);
    tz.from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap()).earliest().map(|t| t.timestamp() as f64).unwrap_or(0.0)
}

/// 一次读数：返回更新后的 (up, down)。`sample` = nodeagent / relayd 最新样本，`boot` = 该批次的 boot_id
pub fn step(c: &Connection, asset: &Value, sample: &Value, boot: &str, now: f64) -> rusqlite::Result<Option<(f64, f64)>> {
    let Some(uuid) = asset["uuid"].as_str() else { return Ok(None) };
    let (Some(tx), Some(rx)) = (sample["net"]["tx_bytes"].as_i64(), sample["net"]["rx_bytes"].as_i64()) else { return Ok(None) };
    let is_relay = asset["team"] == "线路机";
    let ps = if is_relay { period_start(asset, now).map(|d| d.to_string()) } else { None };
    let mut m = match get(c, uuid) {
        Some(m) => m,
        // 没有基线：从这一刻起计（线路机请用 fleetd vendor import 补基线）
        None => Meter { period_start: ps.clone(), up: 0.0, down: 0.0, last_tx: Some(tx), last_rx: Some(rx), last_boot: Some(boot.to_string()) },
    };
    let same_boot = m.last_boot.as_deref() == Some(boot);
    let d = |cur: i64, last: Option<i64>| -> f64 {
        match last {
            Some(l) if same_boot && cur >= l => (cur - l) as f64,
            Some(_) => cur.max(0) as f64, // 重启或计数清零：新计数就是重启后的量
            None => 0.0,
        }
    };
    let (dt, dr) = (d(tx, m.last_tx), d(rx, m.last_rx));
    if is_relay && ps.is_some() && m.period_start != ps {
        // 新账单周期：清零（这一步的增量记入新周期）
        m.period_start = ps;
        m.up = 0.0;
        m.down = 0.0;
    }
    m.up += dt;
    m.down += dr;
    m.last_tx = Some(tx);
    m.last_rx = Some(rx);
    m.last_boot = Some(boot.to_string());
    put(c, uuid, &m, now, None)?;
    Ok(Some((m.up, m.down)))
}

/// host_1m 的网卡速率积分（字节）：[since, until) 内每分钟 平均速率 × 60
pub fn integrate(ic: &Connection, host: &str, since: f64, until: f64) -> (f64, f64) {
    ic.query_row(
        "SELECT coalesce(sum(CASE WHEN tx_n>0 THEN tx_s*60.0/tx_n END),0), coalesce(sum(CASE WHEN rx_n>0 THEN rx_s*60.0/rx_n END),0)
         FROM host_1m WHERE host_id=?1 AND minute>=?2 AND minute<?3",
        params![host, since as i64, until as i64],
        |r| Ok((r.get::<_, f64>(0)?, r.get::<_, f64>(1)?)),
    )
    .unwrap_or((0.0, 0.0))
}

pub fn report(c: &Connection) -> Value {
    let mut st = match c.prepare("SELECT asset, period_start, up, down, updated_at, baseline FROM fleetd_meter ORDER BY asset") {
        Ok(s) => s,
        Err(_) => return json!([]),
    };
    let rows: Vec<Value> = st
        .query_map([], |r| {
            Ok(json!({"asset": r.get::<_, String>(0)?, "period_start": r.get::<_, Option<String>>(1)?, "up": py::f(r.get(2)?), "down": py::f(r.get(3)?),
                "updated_at": r.get::<_, f64>(4)?, "baseline": r.get::<_, Option<String>>(5)?.and_then(|s| serde_json::from_str::<Value>(&s).ok())}))
        })
        .map(|it| it.flatten().collect())
        .unwrap_or_default();
    Value::Array(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn periods() {
        // 2026-09-24 12:00 北京
        let now = 1790222400.0;
        let a = |due: &str| json!({"due_date": due, "billing_timezone": "Asia/Shanghai"});
        assert_eq!(period_start(&a("2026-10-01"), now).unwrap().to_string(), "2026-09-01");
        assert_eq!(period_start(&a("2026-10-13"), now).unwrap().to_string(), "2026-09-13");
        assert_eq!(period_start(&a("2026-10-18"), now).unwrap().to_string(), "2026-09-18");
        assert_eq!(period_start(&a("2026-10-24"), now).unwrap().to_string(), "2026-09-24");
        assert_eq!(period_start(&a("2026-10-25"), now).unwrap().to_string(), "2026-08-25");
        assert_eq!(period_start(&a("2026-10-31"), 1788782400.0).unwrap().to_string(), "2026-08-31"); // 09-07 → 上期 08-31
    }
    #[test]
    fn stepping() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(SCHEMA).unwrap();
        let a = json!({"uuid": "r1", "team": "线路机", "due_date": "2026-10-01"});
        let s = |tx: i64, rx: i64| json!({"net": {"tx_bytes": tx, "rx_bytes": rx}});
        let now = 1790222400.0;
        put(&c, "r1", &Meter { period_start: Some("2026-09-01".into()), up: 1000.0, down: 0.0, last_tx: Some(500), last_rx: Some(0), last_boot: Some("b1".into()) }, now, None).unwrap();
        assert_eq!(step(&c, &a, &s(700, 10), "b1", now).unwrap(), Some((1200.0, 10.0)));
        // 重启：计数从 50 开始
        assert_eq!(step(&c, &a, &s(50, 5), "b2", now).unwrap(), Some((1250.0, 15.0)));
        // 跨周期：10-01 之后清零
        assert_eq!(step(&c, &a, &s(80, 5), "b2", 1790784000.0 + 3600.0).unwrap(), Some((30.0, 0.0)));
    }
}
