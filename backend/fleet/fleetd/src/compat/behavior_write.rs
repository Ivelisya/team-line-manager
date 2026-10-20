//! 旧 `teamops.behavior` 的写路径：Behavior.__init__ 的建表与首次播种（[`ensure`]）、ingest/change、
//! detect()、_refresh()（写 behavior_cache）、maintenance()，以及 store.ingest 里逐条写 activity 的那一段
//! （[`insert_activity`]，供原生喂数调用）。
//!
//! 逐行移植自 release 20260922T081813Z-providers 的 behavior.py / store.py；差分测试见
//! `compat::monitor_eval` 文件头。behavior_cache 的 value 是 `json.dumps(value)`（默认分隔符
//! `", "`/`": "`、ensure_ascii=True），由本文件的 [`dumps_default`] 逐字节复现。
use super::monitor_eval::{exec, in_tx, set_meta, to_sql};
use super::py::*;
use super::pyjson::{float_repr, py_round_int};
use super::Ctx;
use serde_json::Value;

// ─── json.dumps(v)（默认参数） ─────────────────────────────────────────────────────────────────────

fn push_ascii_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' => out.push(c),
            c => {
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{:04x}", u));
                }
            }
        }
    }
    out.push('"');
}

fn write_default(out: &mut String, v: &Value) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) if n.is_f64() => out.push_str(&float_repr(n.as_f64().unwrap_or(0.0))),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => push_ascii_str(out, s),
        Value::Array(a) => {
            out.push('[');
            for (k, x) in a.iter().enumerate() {
                if k > 0 {
                    out.push_str(", ");
                }
                write_default(out, x);
            }
            out.push(']');
        }
        Value::Object(m) => {
            out.push('{');
            for (k, (key, x)) in m.iter().enumerate() {
                if k > 0 {
                    out.push_str(", ");
                }
                push_ascii_str(out, key);
                out.push_str(": ");
                write_default(out, x);
            }
            out.push('}');
        }
    }
}

/// Python `json.dumps(v)`：分隔符 `", "` / `": "`，ensure_ascii=True（非 ASCII 与 DEL 转成 \uXXXX）
pub fn dumps_default(v: &Value) -> String {
    let mut s = String::new();
    write_default(&mut s, v);
    s
}

// ─── Python 数值小工具 ───────────────────────────────────────────────────────────────────────────

fn n_f64(v: &Value) -> R<f64> {
    match v {
        Value::Number(n) => Ok(n.as_f64().unwrap_or(0.0)),
        Value::Bool(b) => Ok(*b as i64 as f64),
        _ => Err(Err::Internal("TypeError: unsupported operand type(s)".into())),
    }
}

/// Python `a - b >= c`（int 与 int 精确，其余按 float）
fn sub_ge(a: &Value, b: &Value, c: f64) -> R<bool> {
    match (int_of(a), int_of(b)) {
        (Some(x), Some(y)) => Ok(((x - y) as f64) >= c),
        _ => Ok(n_f64(a)? - n_f64(b)? >= c),
    }
}

/// Python `int(x // n)`
fn floor_div(x: &Value, n: i64) -> R<i64> {
    n_f64(x)?;
    Ok(floor_div_int(x, n))
}

fn field<'a>(e: &'a Value, k: &str) -> R<&'a Value> {
    e.get(k).ok_or_else(|| Err::Internal(format!("KeyError: '{k}'")))
}

// ─── Behavior ────────────────────────────────────────────────────────────────────────────────────

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS destinations(asset TEXT,host TEXT,kind TEXT,first_seen REAL,last_seen REAL,connections INTEGER,blocked INTEGER,PRIMARY KEY(asset,host));
CREATE INDEX IF NOT EXISTS destination_seen ON destinations(asset,last_seen);
CREATE TABLE IF NOT EXISTS sources_seen(asset TEXT,source_ip TEXT,first_seen REAL,last_seen REAL,connections INTEGER,PRIMARY KEY(asset,source_ip));
CREATE TABLE IF NOT EXISTS behavior_buckets(asset TEXT,bucket INTEGER,connections INTEGER,blocked INTEGER,PRIMARY KEY(asset,bucket));
CREATE TABLE IF NOT EXISTS behavior_events(id INTEGER PRIMARY KEY,asset TEXT,kind TEXT,subject TEXT,window INTEGER,ts REAL,message TEXT,ack_at REAL,UNIQUE(asset,kind,subject,window));
CREATE INDEX IF NOT EXISTS behavior_events_asset ON behavior_events(asset,ts DESC);
CREATE INDEX IF NOT EXISTS behavior_events_time ON behavior_events(ts DESC,id DESC);
CREATE TABLE IF NOT EXISTS behavior_cache(key TEXT PRIMARY KEY,computed_at REAL,value TEXT NOT NULL);
";

/// Behavior.__init__：建表；behavior_started_at 缺失（或为假值）时从 activity 播种三张表并写入 cx.now。
/// （旧代码随后把 behavior_cache 读进内存；这里没有进程内缓存，读路径直接读表。）
pub fn ensure(cx: &Ctx) -> R<()> {
    cx.db.execute_batch(SCHEMA)?;
    in_tx(cx, || {
        if !truthy(&meta(cx.db, "behavior_started_at", Value::Null)?) {
            // 已有记录只播种、不产生提示
            exec(cx.db, "INSERT OR IGNORE INTO destinations SELECT asset,host,kind,min(ts),max(ts),count(*),sum(action='blocked') FROM activity GROUP BY asset,host", &[])?;
            exec(cx.db, "INSERT OR IGNORE INTO sources_seen SELECT asset,source_ip,min(ts),max(ts),count(*) FROM activity GROUP BY asset,source_ip", &[])?;
            exec(cx.db, "INSERT OR IGNORE INTO behavior_buckets SELECT asset,cast(ts/300 as integer)*300,count(*),sum(action='blocked') FROM activity GROUP BY asset,cast(ts/300 as integer)", &[])?;
            set_meta(cx.db, "behavior_started_at", &f(cx.now))?;
        }
        Ok(())
    })
}

/// Behavior.change(asset, kind, subject, window, stamp, message)
pub fn change(cx: &Ctx, asset: &str, kind: &str, subject: &Value, window: i64, stamp: &Value, message: &str) -> R<()> {
    exec(cx.db, "INSERT OR IGNORE INTO behavior_events(asset,kind,subject,window,ts,message) VALUES(?,?,?,?,?,?)",
        &[st(asset), st(kind), to_sql(subject)?, si(window), to_sql(stamp)?, st(message)])?;
    Ok(())
}

/// Behavior.ingest(asset, event)：在写入 activity 的同一事务里调用（只对新插入的行）
pub fn ingest(cx: &Ctx, asset: &str, event: &Value) -> R<()> {
    let db = cx.db;
    let stamp = field(event, "ts")?;
    let host = field(event, "host")?;
    let source_ip = field(event, "source_ip")?;
    let blocked = (field(event, "action")? == "blocked") as i64;
    let domain = scalar(db, "SELECT count(*) FROM destinations WHERE asset=? AND host=?", &[st(asset), to_sql(host)?])?;
    let source = scalar(db, "SELECT count(*) FROM sources_seen WHERE asset=? AND source_ip=?", &[st(asset), to_sql(source_ip)?])?;
    let (domain, source) = (int_of(&domain).unwrap_or(0) > 0, int_of(&source).unwrap_or(0) > 0);
    let key = format!("behavior_baseline:{asset}");
    let mut baseline = meta(db, &key, Value::Null)?;
    if !truthy(&baseline) {
        set_meta(db, &key, stamp)?;
        baseline = stamp.clone();
    }
    let ready = sub_ge(stamp, &baseline, 1800.0)?;
    if ready && !domain && field(event, "kind")? == "domain"
        && !(cx.cfg.domain_collapse && host.as_str().is_some_and(|h| super::domains::record(cx, asset, h, floor_div(stamp, 86400).unwrap_or(0), stamp).unwrap_or(false)))
    {
        change(cx, asset, "new_domain", host, floor_div(stamp, 86400)?, stamp, "首次记录到此域名；新域名不等于异常网站")?;
    }
    if ready && !source {
        change(cx, asset, "new_source", source_ip, floor_div(stamp, 86400)?, stamp, "出现新的入口来源 IP；也可能是正常网络切换或共享出口")?;
    }
    let ts = to_sql(stamp)?;
    exec(db, "INSERT INTO destinations VALUES(?,?,?,?,?,1,?) ON CONFLICT(asset,host) DO UPDATE SET last_seen=max(last_seen,excluded.last_seen),first_seen=min(first_seen,excluded.first_seen),connections=connections+1,blocked=blocked+excluded.blocked",
        &[st(asset), to_sql(host)?, to_sql(field(event, "kind")?)?, ts.clone(), ts.clone(), si(blocked)])?;
    exec(db, "INSERT INTO sources_seen VALUES(?,?,?,?,1) ON CONFLICT(asset,source_ip) DO UPDATE SET last_seen=max(last_seen,excluded.last_seen),first_seen=min(first_seen,excluded.first_seen),connections=connections+1",
        &[st(asset), to_sql(source_ip)?, ts.clone(), ts])?;
    exec(db, "INSERT INTO behavior_buckets VALUES(?,?,1,?) ON CONFLICT(asset,bucket) DO UPDATE SET connections=connections+1,blocked=blocked+excluded.blocked",
        &[st(asset), si(floor_div(stamp, 300)? * 300), si(blocked)])?;
    Ok(())
}

/// store.ingest 里事件归属的那一段：按入口端口找资产（线路机主入口端口归到线路机自身）。
/// `master_ports` = 本次快照里没有 outbound_tag 的入站端口。返回 (asset uuid, team)。
pub fn event_owner(cx: &Ctx, relay: &str, master_ports: &[i64], event: &Value) -> R<(String, String)> {
    let assets = super::store::raw_assets(cx)?;
    let inbound = field(event, "inbound")?.as_str().ok_or_else(|| Err::Internal("AttributeError: 'split'".into()))?;
    let tail = inbound.rsplit('-').next().unwrap_or(inbound);
    let port = py_int(tail).map_err(|e| match e {
        Err::Bad(m) => Err::Internal(format!("ValueError: {m}")),
        e => e,
    })?;
    // by_port = {a["relay_port"]: a for a in assets if a.get("relay") == relay}（同端口后者覆盖）
    let mut owner = assets
        .iter()
        .filter(|a| get(a, "relay").as_str() == Some(relay))
        .filter(|a| super::monitor_eval::py_eq(get(a, "relay_port"), &i(port)))
        .last()
        .cloned();
    if !master_ports.is_empty() && master_ports.contains(&port) {
        if let Some(m) = assets.iter().find(|a| get(a, "team") == super::store::RELAY_TEAM && get(a, "name").as_str() == Some(relay)) {
            owner = Some(m.clone());
        }
    }
    Ok(match owner {
        Some(a) => (get(&a, "uuid").as_str().unwrap_or("").to_string(), get(&a, "team").as_str().unwrap_or("").to_string()),
        None => (
            format!("direct:{relay}:{tail}"),
            if field(event, "outbound")? == "direct" { "未分配直连" } else { "未归属入口" }.to_string(),
        ),
    })
}

/// store.ingest 的单条事件写入：`INSERT OR IGNORE INTO activity …`，新插入时调用 [`ingest`]。
/// `asset`/`team` 可由 [`event_owner`] 得出；event 是线路机快照里 events 的一项
/// （event_id、ts、source_ip、host、port、protocol、action、client、kind）。返回是否新插入。
/// 调用方已在事务里时并入该事务，否则自成一个事务。
pub fn insert_activity(cx: &Ctx, relay: &str, asset: &str, team: &str, event: &Value) -> R<bool> {
    in_tx(cx, || {
        let id = format!("{relay}:{}", field(event, "event_id")?.as_str().ok_or_else(|| Err::Internal("TypeError: event_id".into()))?);
        let mut args = vec![st(&id), to_sql(field(event, "ts")?)?, st(asset), st(team), st(relay)];
        for k in ["source_ip", "host", "port", "protocol", "action", "client", "kind"] {
            args.push(to_sql(field(event, k)?)?);
        }
        let inserted = exec(cx.db, "INSERT OR IGNORE INTO activity VALUES(?,?,?,?,?,?,?,?,?,?,?,?)", &args)?;
        if inserted > 0 {
            ingest(cx, asset, event)?;
        }
        Ok(inserted > 0)
    })
}

/// statistics.median（偶数个取中间两数的平均，结果为 float）
fn median(mut xs: Vec<Value>) -> R<Value> {
    xs.sort_by(|a, b| cmp(a, b));
    let n = xs.len();
    if n % 2 == 1 {
        return Ok(xs[n / 2].clone());
    }
    Ok(f((n_f64(&xs[n / 2 - 1])? + n_f64(&xs[n / 2])?) / 2.0))
}

fn py_str_num(v: &Value) -> String {
    match v {
        Value::Number(n) if n.is_f64() => float_repr(n.as_f64().unwrap_or(0.0)),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// Behavior.detect()：上一个完整 5 分钟桶的连接突增 / 规则阻止突增
pub fn detect(cx: &Ctx) -> R<()> {
    let now = cx.now;
    let end = (now / 300.0).floor() as i64 * 300;
    in_tx(cx, || {
        for a in super::store::raw_assets(cx)? {
            if !truthy(get(&a, "relay_port")) {
                continue;
            }
            let uuid = get(&a, "uuid").as_str().unwrap_or("").to_string();
            let rs = rows(cx.db, "SELECT bucket,connections,blocked FROM behavior_buckets WHERE asset=? AND bucket>=? AND bucket<? ORDER BY bucket",
                &[st(&uuid), si(end - 7200), si(end)])?;
            let Some(current) = rs.last() else { continue };
            if int_of(&current["bucket"]) != Some(end - 300) && num(&current["bucket"]) != Some((end - 300) as f64) {
                continue;
            }
            let previous = &rs[..rs.len() - 1];
            if previous.len() >= 12 {
                let m = median(previous.iter().map(|r| r["connections"].clone()).collect())?;
                let threshold = (n_f64(&m)? * 5.0).max(300.0);
                let threshold = if int_of(&m).is_some() { (int_of(&m).unwrap() * 5).max(300) as f64 } else { threshold };
                if n_f64(&current["connections"])? >= threshold {
                    let rounded = match int_of(&m) {
                        Some(x) => x,
                        None => py_round_int(n_f64(&m)?),
                    };
                    change(cx, &uuid, "connection_spike", &Value::String("connections".into()), int_of(&current["bucket"]).unwrap_or(end - 300), &f(now),
                        &format!("5 分钟内 {} 次入口连接，此前有流量时段中位数 {}；请查看来源和目标，连接数不等于用户数或页面浏览量", py_str_num(&current["connections"]), rounded))?;
                }
            }
            if n_f64(&current["blocked"])? >= 20.0 {
                change(cx, &uuid, "blocked_burst", &Value::String("blocked".into()), int_of(&current["bucket"]).unwrap_or(end - 300), &f(now),
                    &format!("5 分钟内 {} 次连接被规则阻止；核对访问规则是否符合预期", py_str_num(&current["blocked"])))?;
            }
        }
        Ok(())
    })
}

/// Behavior._refresh()：重算默认查询的 summary 与 activity 聚合并写入 behavior_cache。
/// （旧 refresh() 的「10 秒内不重算」依赖进程内缓存，由调用方的节拍保证。）
pub fn refresh(cx: &Ctx) -> R<()> {
    refresh_parts(cx, true, true)
}

/// 只刷新有人要的那一份（summary = 默认 /behavior；activity = 默认 /activity 的聚合）
pub fn refresh_parts(cx: &Ctx, want_summary: bool, want_activity: bool) -> R<()> {
    let computed_at = cx.now;
    let mut parts: Vec<(&str, Value)> = vec![];
    if want_summary {
        let mut summary = super::behavior::summary_live(cx, &Params::new())?;
        summary.as_object_mut().unwrap().insert("computed_at".into(), f(computed_at));
        parts.push(("summary", summary));
    }
    if want_activity {
        parts.push(("activity", super::behavior::activity_aggregates(cx)?));
    }
    in_tx(cx, || {
        for (key, value) in &parts {
            exec(cx.db, "INSERT INTO behavior_cache VALUES(?,?,?) ON CONFLICT(key) DO UPDATE SET computed_at=excluded.computed_at,value=excluded.value",
                &[st(key), sf(computed_at), st(&dumps_default(value))])?;
        }
        Ok(())
    })
}

/// Behavior.maintenance()：目标/来源/提示保留 90 天，5 分钟桶保留 14 天
pub fn maintenance(cx: &Ctx) -> R<()> {
    let now = cx.now;
    in_tx(cx, || {
        for (table, column, days) in [("destinations", "last_seen", 90), ("sources_seen", "last_seen", 90), ("behavior_buckets", "bucket", 14), ("behavior_events", "ts", 90)] {
            exec(cx.db, &format!("DELETE FROM {table} WHERE {column}<?"), &[sf(now - (days * 86400) as f64)])?;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compat::CompatCfg;
    use rusqlite::Connection;
    use serde_json::json;

    #[test]
    fn dumps_default_matches_python() {
        // Python: json.dumps({"a": [1, 2.0, None, True], "中": "x\"\n\u007f😀", "e": {}, "l": []})
        let v = json!({"a": [1, 2.0, null, true], "中": "x\"\n\u{7f}😀", "e": {}, "l": []});
        assert_eq!(dumps_default(&v), r#"{"a": [1, 2.0, null, true], "\u4e2d": "x\"\n\u007f\ud83d\ude00", "e": {}, "l": []}"#);
        assert_eq!(dumps_default(&json!(1e-05)), "1e-05");
    }

    #[test]
    fn medians() {
        assert_eq!(median(vec![json!(3), json!(1), json!(2)]).unwrap(), json!(2));
        let m = median(vec![json!(6), json!(7), json!(1), json!(9)]).unwrap();
        assert!(m.is_f64() && m.as_f64() == Some(6.5));
        assert_eq!(py_round_int(6.5), 6);
    }

    fn mem() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
            CREATE TABLE assets(uuid TEXT PRIMARY KEY,data TEXT NOT NULL);
            CREATE TABLE activity(id TEXT PRIMARY KEY,ts REAL,asset TEXT,team TEXT,relay TEXT,source_ip TEXT,host TEXT,port INTEGER,protocol TEXT,action TEXT,client TEXT,kind TEXT);
            CREATE INDEX activity_time ON activity(ts);")
            .unwrap();
        db
    }

    #[test]
    fn ingest_and_detect() {
        let db = mem();
        let cfg = CompatCfg::default();
        db.execute("INSERT INTO assets VALUES('u1', ?)", [r#"{"uuid":"u1","team":"团队1","name":"n1","relay":"线路机2","relay_port":20002}"#]).unwrap();
        db.execute("INSERT INTO activity VALUES('线路机2:old',1000.0,'u1','团队1','线路机2','1.1.1.1','a.com',443,'tcp','routed','c','domain')", []).unwrap();
        let cx = Ctx { db: &db, now: 5000.0, cfg: &cfg };
        ensure(&cx).unwrap();
        assert_eq!(scalar(&db, "SELECT connections FROM destinations WHERE host='a.com'", &[]).unwrap(), json!(1));
        assert_eq!(meta(&db, "behavior_started_at", Value::Null).unwrap(), json!(5000.0));
        // 基线 = 首个事件时刻（int 保型），30 分钟后才提示新目标
        let ev = |id: &str, ts: Value, host: &str, ip: &str| json!({"event_id": id, "ts": ts, "inbound": "inbound-20002", "outbound": "ss", "source_ip": ip, "host": host,
            "port": 443, "protocol": "tcp", "action": "blocked", "client": "c", "kind": "domain"});
        let e1 = ev("1", json!(2000), "b.com", "2.2.2.2");
        let (a, t) = event_owner(&cx, "线路机2", &[], &e1).unwrap();
        assert_eq!((a.as_str(), t.as_str()), ("u1", "团队1"));
        assert!(insert_activity(&cx, "线路机2", &a, &t, &e1).unwrap());
        assert!(!insert_activity(&cx, "线路机2", &a, &t, &e1).unwrap());
        assert_eq!(db.query_row("SELECT value FROM meta WHERE key='behavior_baseline:u1'", [], |r| r.get::<_, String>(0)).unwrap(), "2000");
        assert!(insert_activity(&cx, "线路机2", &a, &t, &ev("2", json!(3900.5), "c.com", "3.3.3.3")).unwrap());
        let kinds: Vec<String> = db.prepare("SELECT kind||'/'||subject||'/'||window FROM behavior_events ORDER BY id").unwrap()
            .query_map([], |r| r.get(0)).unwrap().flatten().collect();
        assert_eq!(kinds, ["new_domain/c.com/0", "new_source/3.3.3.3/0"]);
        let (d, t2) = event_owner(&cx, "线路机2", &[], &json!({"inbound": "inbound-9", "outbound": "direct"})).unwrap();
        assert_eq!((d.as_str(), t2.as_str()), ("direct:线路机2:9", "未分配直连"));
        // 20 次阻止 → blocked_burst
        for k in 0..20 {
            insert_activity(&cx, "线路机2", "u1", "团队1", &ev(&format!("b{k}"), json!(9000 + k), "c.com", "3.3.3.3")).unwrap();
        }
        let cx = Ctx { db: &db, now: 9310.0, cfg: &cfg };
        detect(&cx).unwrap();
        let msg: String = db.query_row("SELECT message FROM behavior_events WHERE kind='blocked_burst'", [], |r| r.get(0)).unwrap();
        assert!(msg.starts_with("5 分钟内 20 次连接被规则阻止"));
        maintenance(&Ctx { db: &db, now: 9000.0 + 15.0 * 86400.0, cfg: &cfg }).unwrap();
        assert_eq!(scalar(&db, "SELECT count(*) FROM behavior_buckets", &[]).unwrap(), json!(0));
    }
}
