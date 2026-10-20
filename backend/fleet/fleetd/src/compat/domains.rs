//! 新域名事件的“随机子域名”合并（生产，CompatCfg.domain_collapse）。
//!
//! 有些服务每次连接都用随机命名的子域名（fbcdn.net 的 `<uuid>-netseer-ipaddr-assoc.xz.fbcdn.net`、
//! auraex.cn 的 56 位随机串、ydycdn.com 的 8 位十六进制），每个都算“新域名”，一天几千条，把真正的行为变化挤出列表。
//! 这里把主域名（公共后缀表 PSL 的可注册域名）以下出现随机标签的主机合并成一条事件：
//! subject = `*.<主域名>`，同一资产同一天（window）一条；evidence 记数量与前 5 个示例。
//! 普通子域名（push-rtmp-vr-l26.douyincdn.com 之类）不合并，照旧逐个记。
use super::py::*;
use super::Ctx;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

const EXAMPLES: usize = 5;

/// 可注册域名（PSL）；IP、单标签、公共后缀本身 → None
pub fn registrable(host: &str) -> Option<String> {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    if h.parse::<std::net::IpAddr>().is_ok() || !h.contains('.') {
        return None;
    }
    psl::domain_str(&h).map(String::from)
}

fn is_uuid_at(b: &[u8], i: usize) -> bool {
    const SHAPE: [usize; 5] = [8, 4, 4, 4, 12];
    let mut p = i;
    for (k, n) in SHAPE.iter().enumerate() {
        if p + n > b.len() || !b[p..p + n].iter().all(|c| c.is_ascii_hexdigit()) {
            return false;
        }
        p += n;
        if k < 4 {
            if b.get(p) != Some(&b'-') {
                return false;
            }
            p += 1;
        }
    }
    true
}

/// 随机命名的标签：含 UUID；或 ≥8 位纯十六进制且至少 2 个数字；或 ≥20 位纯字母数字且至少 4 个数字
pub fn is_random_label(label: &str) -> bool {
    let l = label.to_ascii_lowercase();
    let b = l.as_bytes();
    if b.len() >= 36 && (0..=b.len() - 36).any(|i| is_uuid_at(b, i)) {
        return true;
    }
    let digits = b.iter().filter(|c| c.is_ascii_digit()).count();
    if b.len() >= 8 && digits >= 2 && b.iter().all(|c| c.is_ascii_hexdigit()) {
        return true;
    }
    b.len() >= 20 && digits >= 4 && b.iter().all(|c| c.is_ascii_alphanumeric())
}

/// 主域名以下有随机标签 → `*.<主域名>`
pub fn collapse_subject(host: &str) -> Option<String> {
    let reg = registrable(host)?;
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    let prefix = h.strip_suffix(reg.as_str())?.trim_end_matches('.');
    if prefix.is_empty() {
        return None;
    }
    prefix.split('.').any(is_random_label).then(|| format!("*.{reg}"))
}

/// behavior_events.evidence（JSON，只有合并事件有值）
pub fn ensure_column(c: &Connection) -> rusqlite::Result<()> {
    let has: bool = c.query_row("SELECT count(*) FROM pragma_table_info('behavior_events') WHERE name='evidence'", [], |r| r.get::<_, i64>(0))? > 0;
    let table: bool = c.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name='behavior_events'", [], |r| r.get::<_, i64>(0))? > 0;
    if table && !has {
        c.execute_batch("ALTER TABLE behavior_events ADD COLUMN evidence TEXT")?;
    }
    Ok(())
}

fn message(reg: &str, n: i64, examples: &[Value]) -> String {
    let ex = examples.first().and_then(|v| v.as_str()).unwrap_or("");
    format!("{reg} 下出现 {n} 个随机命名的新子域名（如 {ex}），已按主域名合并为一条；新域名不等于异常网站")
}

fn merge(c: &Connection, id: i64, add: i64, hosts: &[String], last_ts: f64) -> rusqlite::Result<()> {
    let (subject, ev): (String, Option<String>) = c.query_row("SELECT subject, evidence FROM behavior_events WHERE id=?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))?;
    let reg = subject.trim_start_matches("*.").to_string();
    let mut ev: Value = ev.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_else(|| json!({"collapsed_by": "registrable_domain", "domain": reg, "count": 0, "examples": []}));
    let n = ev["count"].as_i64().unwrap_or(0) + add;
    let mut ex: Vec<Value> = ev["examples"].as_array().cloned().unwrap_or_default();
    for h in hosts {
        if ex.len() >= EXAMPLES {
            break;
        }
        if !ex.iter().any(|x| x == h) {
            ex.push(json!(h));
        }
    }
    ev["count"] = json!(n);
    ev["examples"] = json!(ex);
    ev["last_ts"] = json!(last_ts.max(ev["last_ts"].as_f64().unwrap_or(0.0)));
    c.execute("UPDATE behavior_events SET evidence=?2, message=?3 WHERE id=?1", params![id, ev.to_string(), message(&reg, n, &ex)])?;
    Ok(())
}

/// 写入一条新域名事件：随机子域名合并，其余照旧。返回 true = 已处理（合并）
pub fn record(cx: &Ctx, asset: &str, host: &str, window: i64, stamp: &Value) -> R<bool> {
    let Some(subject) = collapse_subject(host) else { return Ok(false) };
    let ts = stamp.as_f64().unwrap_or(cx.now);
    let reg = subject.trim_start_matches("*.");
    let id: Option<i64> = cx
        .db
        .query_row("SELECT id FROM behavior_events WHERE asset=?1 AND kind='new_domain' AND subject=?2 AND window=?3", params![asset, subject, window], |r| r.get(0))
        .optional()?;
    let id = match id {
        Some(id) => id,
        None => {
            cx.db.execute(
                "INSERT INTO behavior_events(asset,kind,subject,window,ts,message) VALUES(?1,'new_domain',?2,?3,?4,?5)",
                params![asset, subject, window, ts, message(reg, 0, &[])],
            )?;
            cx.db.last_insert_rowid()
        }
    };
    merge(cx.db, id, 1, &[host.to_string()], ts)?;
    Ok(true)
}

/// 回填：ts ≥ since 的逐条随机子域名事件 → 每（资产, 天, 主域名）一条。保留最早一条（改 subject），其余删除；
/// 组内有未查看的就保持未查看。
pub fn backfill(c: &Connection, since: f64, dry_run: bool) -> Result<Value, String> {
    ensure_column(c).map_err(|e| e.to_string())?;
    let tx = c.unchecked_transaction().map_err(|e| e.to_string())?;
    let rows: Vec<(i64, String, String, i64, f64, Option<f64>)> = {
        let mut st = c
            .prepare("SELECT id, asset, subject, window, ts, ack_at FROM behavior_events WHERE kind='new_domain' AND ts>=?1 AND subject NOT LIKE '*.%' ORDER BY ts, id")
            .map_err(|e| e.to_string())?;
        let it = st
            .query_map([since], |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, Option<String>>(2)?.unwrap_or_default(), r.get(3)?, r.get(4)?, r.get(5)?)))
            .map_err(|e| e.to_string())?;
        it.flatten().collect()
    };
    let mut groups: std::collections::BTreeMap<(String, i64, String), Vec<(i64, String, f64, Option<f64>)>> = Default::default();
    for (id, asset, subject, window, ts, ack) in rows {
        if let Some(s) = collapse_subject(&subject) {
            groups.entry((asset, window, s)).or_default().push((id, subject, ts, ack));
        }
    }
    let (mut removed, mut kept) = (0usize, 0usize);
    let mut top: Vec<(String, usize)> = vec![];
    for ((asset, window, subject), items) in &groups {
        top.push((subject.clone(), items.len()));
        if dry_run {
            removed += items.len() - 1;
            kept += 1;
            continue;
        }
        let existing: Option<i64> = c
            .query_row("SELECT id FROM behavior_events WHERE asset=?1 AND kind='new_domain' AND subject=?2 AND window=?3", params![asset, subject, window], |r| r.get(0))
            .optional()
            .map_err(|e| e.to_string())?;
        let unacked = items.iter().any(|x| x.3.is_none());
        let (keep, rest): (i64, &[(i64, String, f64, Option<f64>)]) = match existing {
            Some(id) => (id, &items[..]),
            None => {
                let first = &items[0];
                c.execute("UPDATE behavior_events SET subject=?2 WHERE id=?1", params![first.0, subject]).map_err(|e| e.to_string())?;
                (first.0, &items[1..])
            }
        };
        for x in rest {
            c.execute("DELETE FROM behavior_events WHERE id=?1", [x.0]).map_err(|e| e.to_string())?;
        }
        if unacked {
            c.execute("UPDATE behavior_events SET ack_at=NULL WHERE id=?1", [keep]).map_err(|e| e.to_string())?;
        }
        let hosts: Vec<String> = items.iter().map(|x| x.1.clone()).collect();
        let last = items.iter().map(|x| x.2).fold(0.0, f64::max);
        merge(c, keep, items.len() as i64, &hosts, last).map_err(|e| e.to_string())?;
        removed += rest.len();
        kept += 1;
    }
    if !dry_run {
        tx.commit().map_err(|e| e.to_string())?;
    }
    top.sort_by(|a, b| b.1.cmp(&a.1));
    let rows_in: usize = groups.values().map(|v| v.len()).sum();
    Ok(json!({"dry_run": dry_run, "since": since, "random_rows": rows_in, "merged_events": kept, "removed_rows": removed,
        "top": top.into_iter().take(10).map(|(s, n)| json!({"subject": s, "rows": n})).collect::<Vec<_>>()}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn labels() {
        assert!(is_random_label("21f7dc08-4a70-4856-8526-19957f479d90-netseer-ipaddr-assoc"));
        assert!(is_random_label("7020256e"));
        assert!(is_random_label("g84gbtiuoro0kbeq7o5i5cim623i8t0knms2pji0asics822rub2aru"));
        for l in ["push-rtmp-vr-l26", "array506", "v16m", "cafebabe", "api16-normal-useast8", "25-courier", "oecim16-normal-useast5", "lf16-geckocdn"] {
            assert!(!is_random_label(l), "{l}");
        }
        assert_eq!(collapse_subject("21f7dc08-4a70-4856-8526-19957f479d90-netseer-ipaddr-assoc.xz.fbcdn.net").as_deref(), Some("*.fbcdn.net"));
        assert_eq!(collapse_subject("248b6232.ydycdn.com").as_deref(), Some("*.ydycdn.com"));
        assert_eq!(collapse_subject("tips.vivo.com.cn"), None);
        assert_eq!(collapse_subject("7020256e.com.cn"), None); // 公共后缀 + 一段：随机串就是主域名本身
        assert_eq!(collapse_subject("1.2.3.4"), None);
    }
    #[test]
    fn record_and_backfill() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE behavior_events(id INTEGER PRIMARY KEY,asset TEXT,kind TEXT,subject TEXT,window INTEGER,ts REAL,message TEXT,ack_at REAL,UNIQUE(asset,kind,subject,window));").unwrap();
        ensure_column(&c).unwrap();
        for (i, h) in ["aaaa1111.x.com", "bbbb2222.x.com", "www.x.com", "cccc3333.x.com"].iter().enumerate() {
            c.execute("INSERT INTO behavior_events(asset,kind,subject,window,ts,message,ack_at) VALUES('a','new_domain',?1,1,?2,'m',?3)",
                params![h, 100.0 + i as f64, if i == 0 { Some(1.0) } else { None }]).unwrap();
        }
        let v = backfill(&c, 0.0, false).unwrap();
        assert_eq!(v["removed_rows"], 2);
        let (s, ev, ack): (String, String, Option<f64>) = c.query_row("SELECT subject, evidence, ack_at FROM behavior_events WHERE subject LIKE '*.%'", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
        assert_eq!(s, "*.x.com");
        assert_eq!(serde_json::from_str::<Value>(&ev).unwrap()["count"], 3);
        assert_eq!(ack, None);
        let cfg = super::super::CompatCfg::default();
        let cx = Ctx { db: &c, now: 200.0, cfg: &cfg };
        assert!(record(&cx, "a", "dddd4444.x.com", 1, &json!(150.0)).unwrap());
        assert!(!record(&cx, "a", "api.x.com", 1, &json!(150.0)).unwrap());
        let ev: String = c.query_row("SELECT evidence FROM behavior_events WHERE subject='*.x.com'", [], |r| r.get(0)).unwrap();
        let ev: Value = serde_json::from_str(&ev).unwrap();
        assert_eq!(ev["count"], 4);
        assert_eq!(ev["examples"].as_array().unwrap().len(), 4);
        assert_eq!(c.query_row("SELECT count(*) FROM behavior_events", [], |r| r.get::<_, i64>(0)).unwrap(), 2);
    }
}
