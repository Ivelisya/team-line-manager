//! 7 天 / 14 天的 /behavior 与 /activity：按小时的（资产, 目标, 来源）汇总，写入时维护，请求时不扫原始记录。
//!
//! - 表 `beh_pair_h`（原生旧库）：每小时每个（asset, host, source_ip）一格：连接数、阻止数、最早 / 最晚时间，
//!   以及最晚那一行的 kind / team（旧 SQL 的裸列取 max(ts) 那一行）。喂数每拍按 rowid 游标把新访问记录并进来，
//!   首次按块回填最近 15 天（每拍最多 BACKFILL_CHUNK 行），保留 15 天。
//! - 内存镜像（Memo.rollup）：第一次长窗口请求时从表载入（一个读事务，带游标），之后喂数每拍把同一批格子并进去；
//!   游标对不上（载入与喂数交错）就作废，下次请求重载。
//! - 查询窗口 `ts >= now - seconds`：起点所在的那个不完整小时直接读原始记录（≤1 小时，走 ts 索引），
//!   其后的整小时用汇总格子，结果与旧 SQL 同值（去重计数用集合）。排序规则同 compat::window。
use super::py::*;
use super::window::ActFilter;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};

pub const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS beh_pair_h(hour INTEGER NOT NULL, asset TEXT NOT NULL, host TEXT NOT NULL, source_ip TEXT NOT NULL,
  kind TEXT, team TEXT, conn INTEGER NOT NULL, blocked INTEGER NOT NULL, first_ts REAL NOT NULL, last_ts REAL NOT NULL,
  PRIMARY KEY(hour, asset, host, source_ip)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS beh_pair_state(k TEXT PRIMARY KEY, v);";

pub const KEEP_HOURS: i64 = 15 * 24;
pub const BACKFILL_CHUNK: i64 = 150_000;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cell {
    pub conn: u32,
    pub blocked: u32,
    pub first: f64,
    pub last: f64,
    pub kind: u32,
    pub team: u32,
}

impl Cell {
    fn merge(&mut self, o: &Cell) {
        self.conn += o.conn;
        self.blocked += o.blocked;
        self.first = self.first.min(o.first);
        if o.last >= self.last {
            self.last = o.last;
            self.kind = o.kind;
            self.team = o.team;
        }
    }
}

#[derive(Default)]
pub struct Syms {
    names: Vec<String>,
    ids: HashMap<String, u32>,
}
impl Syms {
    pub fn sym(&mut self, s: &str) -> u32 {
        if let Some(i) = self.ids.get(s) {
            return *i;
        }
        let i = self.names.len() as u32;
        self.names.push(s.to_string());
        self.ids.insert(s.to_string(), i);
        i
    }
    pub fn name(&self, i: u32) -> &str {
        &self.names[i as usize]
    }
}

/// 一批新格子（字符串键），喂数与内存共用
pub type Batch = HashMap<(i64, String, String, String), (u32, u32, f64, f64, String, String)>;

#[derive(Default)]
pub struct Rollup {
    pub cursor: i64,
    pub syms: Syms,
    pub hours: BTreeMap<i64, HashMap<(u32, u32, u32), Cell>>,
    pub cells: usize,
}

fn ts_of(v: rusqlite::types::ValueRef<'_>) -> Option<f64> {
    match v {
        rusqlite::types::ValueRef::Real(x) => Some(x),
        rusqlite::types::ValueRef::Integer(x) => Some(x as f64),
        _ => None,
    }
}

fn state(c: &Connection, k: &str) -> Option<i64> {
    c.query_row("SELECT v FROM beh_pair_state WHERE k=?1", [k], |r| r.get::<_, i64>(0)).optional().ok().flatten()
}

fn set_state(c: &Connection, k: &str, v: i64) -> rusqlite::Result<()> {
    c.execute("INSERT INTO beh_pair_state(k,v) VALUES(?1,?2) ON CONFLICT(k) DO UPDATE SET v=excluded.v", params![k, v])?;
    Ok(())
}

/// 表是否已回填完（长窗口查询只在就绪后走汇总）
pub fn ready(c: &Connection) -> bool {
    c.query_row("SELECT count(*) FROM sqlite_master WHERE name='beh_pair_state'", [], |r| r.get::<_, i64>(0)).unwrap_or(0) > 0 && state(c, "ready") == Some(1)
}

/// 喂数（写连接、在喂数事务里）：把游标之后的访问记录并进表。返回（游标起点, 游标终点, 这批格子）。
pub fn feed(c: &Connection, now: f64) -> R<(i64, i64, Batch)> {
    c.execute_batch(SCHEMA)?;
    let start = match state(c, "rowid") {
        Some(x) => x,
        None => {
            let cutoff = now - (KEEP_HOURS * 3600) as f64;
            let r0: Option<i64> = c
                .query_row("SELECT rowid FROM activity INDEXED BY activity_time WHERE ts>=?1 ORDER BY ts LIMIT 1", [cutoff], |r| r.get(0))
                .optional()?;
            let s = r0.map(|x| x - 1).unwrap_or_else(|| c.query_row("SELECT coalesce(max(rowid),0) FROM activity", [], |r| r.get(0)).unwrap_or(0));
            set_state(c, "rowid", s)?;
            set_state(c, "ready", 0)?;
            s
        }
    };
    let mut batch: Batch = HashMap::new();
    let mut end = start;
    let mut n = 0i64;
    {
        let mut st = c.prepare_cached("SELECT rowid, ts, asset, team, host, kind, source_ip, action FROM activity WHERE rowid>?1 ORDER BY rowid LIMIT ?2")?;
        let mut q = st.query(params![start, BACKFILL_CHUNK])?;
        while let Some(r) = q.next()? {
            n += 1;
            end = r.get(0)?;
            let Some(ts) = ts_of(r.get_ref(1)?) else { continue };
            let s = |i: usize| -> String { r.get::<_, Option<String>>(i).ok().flatten().unwrap_or_default() };
            let e = batch.entry(((ts / 3600.0).floor() as i64, s(2), s(4), s(6))).or_insert((0, 0, ts, ts, s(5), s(3)));
            e.0 += 1;
            e.1 += (s(7) == "blocked") as u32;
            e.2 = e.2.min(ts);
            if ts >= e.3 {
                e.3 = ts;
                e.4 = s(5);
                e.5 = s(3);
            }
        }
    }
    {
        let mut up = c.prepare_cached(
            "INSERT INTO beh_pair_h(hour,asset,host,source_ip,kind,team,conn,blocked,first_ts,last_ts) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
             ON CONFLICT(hour,asset,host,source_ip) DO UPDATE SET conn=conn+excluded.conn, blocked=blocked+excluded.blocked,
               first_ts=min(first_ts,excluded.first_ts),
               kind=CASE WHEN excluded.last_ts>=last_ts THEN excluded.kind ELSE kind END,
               team=CASE WHEN excluded.last_ts>=last_ts THEN excluded.team ELSE team END,
               last_ts=max(last_ts,excluded.last_ts)",
        )?;
        for ((h, a, host, src), (conn, bl, fi, la, kind, team)) in &batch {
            up.execute(params![h, a, host, src, kind, team, conn, bl, fi, la])?;
        }
    }
    set_state(c, "rowid", end)?;
    if n < BACKFILL_CHUNK && state(c, "ready") != Some(1) {
        set_state(c, "ready", 1)?;
    }
    Ok((start, end, batch))
}

/// 每小时一次：删 15 天前的格子
pub fn retention(c: &Connection, now: f64) -> R<usize> {
    c.execute_batch(SCHEMA)?;
    let cut = (now / 3600.0).floor() as i64 - KEEP_HOURS;
    Ok(c.execute("DELETE FROM beh_pair_h WHERE hour<?1", [cut])?)
}

impl Rollup {
    pub fn mem_stats(&self) -> Value {
        json!({"cells": self.cells, "hours": self.hours.len(), "syms": self.syms.names.len(),
            "cap": self.hours.values().map(|m| m.capacity()).sum::<usize>()})
    }

    /// 从表载入（读连接；一个读事务里同时读游标与格子，保证一致）
    pub fn load(c: &Connection) -> R<Rollup> {
        let tx = c.unchecked_transaction()?;
        let mut r = Rollup { cursor: state(c, "rowid").unwrap_or(0), ..Default::default() };
        {
            let mut st = c.prepare("SELECT hour, asset, host, source_ip, kind, team, conn, blocked, first_ts, last_ts FROM beh_pair_h")?;
            let mut q = st.query([])?;
            while let Some(x) = q.next()? {
                let s = |i: usize| -> String { x.get::<_, Option<String>>(i).ok().flatten().unwrap_or_default() };
                let (h, a, host, src, kind, team) = (x.get::<_, i64>(0)?, s(1), s(2), s(3), s(4), s(5));
                let cell = Cell { conn: x.get::<_, i64>(6)? as u32, blocked: x.get::<_, i64>(7)? as u32, first: x.get(8)?, last: x.get(9)?, kind: r.syms.sym(&kind), team: r.syms.sym(&team) };
                let key = (r.syms.sym(&a), r.syms.sym(&host), r.syms.sym(&src));
                r.hours.entry(h).or_default().insert(key, cell);
                r.cells += 1;
            }
        }
        tx.finish()?;
        Ok(r)
    }

    /// 喂数的一批格子并进内存。游标不衔接 → false（调用方作废镜像）
    pub fn apply(&mut self, start: i64, end: i64, batch: &Batch, now: f64) -> bool {
        if start != self.cursor {
            return start < self.cursor && end <= self.cursor; // 已包含（载入时表里已有这批）
        }
        for ((h, a, host, src), (conn, bl, fi, la, kind, team)) in batch {
            let c = Cell { conn: *conn, blocked: *bl, first: *fi, last: *la, kind: self.syms.sym(kind), team: self.syms.sym(team) };
            let key = (self.syms.sym(a), self.syms.sym(host), self.syms.sym(src));
            match self.hours.entry(*h).or_default().entry(key) {
                std::collections::hash_map::Entry::Occupied(mut e) => e.get_mut().merge(&c),
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(c);
                    self.cells += 1;
                }
            }
        }
        self.cursor = end;
        let cut = (now / 3600.0).floor() as i64 - KEEP_HOURS;
        while let Some((&h, _)) = self.hours.iter().next() {
            if h >= cut {
                break;
            }
            if let Some(m) = self.hours.remove(&h) {
                self.cells -= m.len();
            }
        }
        true
    }

    /// 窗口内（按过滤）逐格回调：起点所在小时读原始记录，其后用格子
    fn scan(&mut self, db: &Connection, now: f64, seconds: i64, flt: &ActFilter, mut each: impl FnMut(&Syms, u32, u32, u32, &Cell)) -> R<()> {
        let start = now - seconds as f64;
        let h0 = (start / 3600.0).floor() as i64;
        // 起点那个不完整小时：原始记录（一行一格）
        let mut raw: Vec<(String, String, String, Cell, String, String, bool)> = vec![];
        {
            let mut st = db.prepare_cached(
                "SELECT ts, asset, team, host, kind, source_ip, action FROM activity INDEXED BY activity_time WHERE ts>=?1 AND ts<?2",
            )?;
            let mut q = st.query(params![start, ((h0 + 1) * 3600) as f64])?;
            while let Some(r) = q.next()? {
                let Some(ts) = ts_of(r.get_ref(0)?) else { continue };
                let s = |i: usize| -> String { r.get::<_, Option<String>>(i).ok().flatten().unwrap_or_default() };
                raw.push((s(1), s(3), s(5), Cell { conn: 1, blocked: (s(6) == "blocked") as u32, first: ts, last: ts, kind: 0, team: 0 }, s(4), s(2), s(6) == "blocked"));
            }
        }
        for (a, host, src, mut c, kind, team, _) in raw {
            c.kind = self.syms.sym(&kind);
            c.team = self.syms.sym(&team);
            let (a, host, src) = (self.syms.sym(&a), self.syms.sym(&host), self.syms.sym(&src));
            if pass(&self.syms, flt, a, host, src, &c) {
                each(&self.syms, a, host, src, &c);
            }
        }
        for (_, m) in self.hours.range(h0 + 1..) {
            for (&(a, host, src), c) in m {
                if pass(&self.syms, flt, a, host, src, c) {
                    each(&self.syms, a, host, src, c);
                }
            }
        }
        Ok(())
    }

    /// behavior.summary_live 的 nodes / targets / sources（targets 带 limit/offset）
    pub fn summary_parts(&mut self, db: &Connection, now: f64, seconds: i64, flt: &ActFilter, limit: usize, offset: usize) -> R<(Vec<Value>, Vec<Value>, Vec<Value>)> {
        #[derive(Default)]
        struct N {
            conn: i64,
            blocked: i64,
            ip_only: i64,
            last: f64,
            team: u32,
            domains: HashSet<u32>,
            srcs: HashSet<u32>,
        }
        #[derive(Default)]
        struct T {
            conn: i64,
            blocked: i64,
            first: f64,
            last: f64,
            kind: u32,
            srcs: Vec<u32>,
        }
        #[derive(Default)]
        struct S {
            conn: i64,
            last: f64,
            hosts: HashSet<u32>,
        }
        let mut nodes: HashMap<u32, N> = HashMap::new();
        let mut targets: HashMap<(u32, u32), T> = HashMap::new();
        let mut sources: HashMap<(u32, u32), S> = HashMap::new();
        let (dk, ik) = (Some(self.syms.sym("domain")), Some(self.syms.sym("ip")));
        self.scan(db, now, seconds, flt, |_, a, host, src, c| {
            let n = nodes.entry(a).or_insert_with(|| N { last: f64::MIN, ..Default::default() });
            n.conn += c.conn as i64;
            n.blocked += c.blocked as i64;
            if Some(c.kind) == ik {
                n.ip_only += c.conn as i64;
            }
            if Some(c.kind) == dk {
                n.domains.insert(host);
            }
            n.srcs.insert(src);
            if c.last >= n.last {
                n.last = c.last;
                n.team = c.team;
            }
            let t = targets.entry((a, host)).or_insert_with(|| T { first: f64::MAX, last: f64::MIN, ..Default::default() });
            t.conn += c.conn as i64;
            t.blocked += c.blocked as i64;
            t.first = t.first.min(c.first);
            if c.last >= t.last {
                t.last = c.last;
                t.kind = c.kind;
            }
            if !t.srcs.contains(&src) {
                t.srcs.push(src);
            }
            let s = sources.entry((a, src)).or_insert_with(|| S { last: f64::MIN, ..Default::default() });
            s.conn += c.conn as i64;
            s.last = s.last.max(c.last);
            s.hosts.insert(host);
        })?;
        let sy = &self.syms;
        let obj = |pairs: Vec<(&str, Value)>| -> Value { Value::Object(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect::<Map<String, Value>>()) };
        // nodes：无 LIMIT 的 ORDER BY last_seen DESC → 同值按分组（asset 升序）的逆序
        let mut nv: Vec<(&u32, &N)> = nodes.iter().collect();
        nv.sort_by(|a, b| sy.name(*b.0).cmp(sy.name(*a.0)));
        nv.sort_by(|a, b| b.1.last.partial_cmp(&a.1.last).unwrap_or(std::cmp::Ordering::Equal));
        let nodes_out = nv
            .into_iter()
            .map(|(k, n)| {
                obj(vec![("asset", json!(sy.name(*k))), ("team", json!(sy.name(n.team))), ("connections", i(n.conn)), ("domains", i(n.domains.len() as i64)),
                    ("sources", i(n.srcs.len() as i64)), ("blocked", i(n.blocked)), ("ip_only", i(n.ip_only)), ("last_seen", f(n.last))])
            })
            .collect();
        // targets：ORDER BY last_seen DESC, asset, host LIMIT ? OFFSET ?
        let mut tv: Vec<(&(u32, u32), &T)> = targets.iter().collect();
        tv.sort_by(|a, b| {
            b.1.last.partial_cmp(&a.1.last).unwrap_or(std::cmp::Ordering::Equal).then_with(|| sy.name(a.0 .0).cmp(sy.name(b.0 .0))).then_with(|| sy.name(a.0 .1).cmp(sy.name(b.0 .1)))
        });
        let mut targets_out = vec![];
        for ((asset, host), t) in tv.into_iter().skip(offset).take(limit) {
            let d: Option<(Value, Value)> = db
                .prepare_cached("SELECT first_seen, connections FROM destinations WHERE asset=?1 AND host=?2")?
                .query_row([sy.name(*asset), sy.name(*host)], |r| Ok((sql_to_json(r.get_ref(0)?), sql_to_json(r.get_ref(1)?))))
                .ok();
            let (fs, tr) = match d {
                Some((a, b)) => (if a.is_null() { f(t.first) } else { a }, if b.is_null() { i(t.conn) } else { b }),
                None => (f(t.first), i(t.conn)),
            };
            targets_out.push(obj(vec![("asset", json!(sy.name(*asset))), ("host", json!(sy.name(*host))), ("kind", json!(sy.name(t.kind))),
                ("connections", i(t.conn)), ("sources", i(t.srcs.len() as i64)), ("blocked", i(t.blocked)), ("first_in_window", f(t.first)),
                ("last_seen", f(t.last)), ("first_seen", fs), ("total_recorded", tr)]));
        }
        // sources：ORDER BY last_seen DESC LIMIT 100（同值保持分组顺序）
        let mut sv: Vec<(&(u32, u32), &S)> = sources.iter().collect();
        sv.sort_by(|a, b| sy.name(a.0 .0).cmp(sy.name(b.0 .0)).then_with(|| sy.name(a.0 .1).cmp(sy.name(b.0 .1))));
        sv.sort_by(|a, b| b.1.last.partial_cmp(&a.1.last).unwrap_or(std::cmp::Ordering::Equal));
        let sources_out = sv
            .into_iter()
            .take(100)
            .map(|((asset, src), s)| {
                obj(vec![("asset", json!(sy.name(*asset))), ("source_ip", json!(sy.name(*src))), ("connections", i(s.conn)), ("targets", i(s.hosts.len() as i64)), ("last_seen", f(s.last))])
            })
            .collect();
        Ok((nodes_out, targets_out, sources_out))
    }

    /// store.activity 的 domains（top 40）/ sources（top 30）/ total
    pub fn activity_parts(&mut self, db: &Connection, now: f64, seconds: i64, flt: &ActFilter) -> R<(Vec<Value>, Vec<Value>, i64)> {
        #[derive(Default)]
        struct D {
            conn: i64,
            blocked: i64,
            last: f64,
            srcs: Vec<u32>,
        }
        #[derive(Default)]
        struct S {
            conn: i64,
            last: f64,
            hosts: HashSet<u32>,
        }
        let mut dom: HashMap<(u32, u32), D> = HashMap::new();
        let mut src: HashMap<u32, S> = HashMap::new();
        let mut total = 0i64;
        self.scan(db, now, seconds, flt, |_, _a, host, s, c| {
            total += c.conn as i64;
            let d = dom.entry((host, c.kind)).or_insert_with(|| D { last: f64::MIN, ..Default::default() });
            d.conn += c.conn as i64;
            d.blocked += c.blocked as i64;
            d.last = d.last.max(c.last);
            if !d.srcs.contains(&s) {
                d.srcs.push(s);
            }
            let x = src.entry(s).or_insert_with(|| S { last: f64::MIN, ..Default::default() });
            x.conn += c.conn as i64;
            x.last = x.last.max(c.last);
            x.hosts.insert(host);
        })?;
        let sy = &self.syms;
        let obj = |pairs: Vec<(&str, Value)>| -> Value { Value::Object(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect::<Map<String, Value>>()) };
        let mut d: Vec<(&(u32, u32), &D)> = dom.iter().collect();
        d.sort_by(|a, b| sy.name(a.0 .0).cmp(sy.name(b.0 .0)).then_with(|| sy.name(a.0 .1).cmp(sy.name(b.0 .1))));
        d.sort_by(|a, b| b.1.conn.cmp(&a.1.conn));
        let domains = d
            .into_iter()
            .take(40)
            .map(|((host, kind), p)| {
                obj(vec![("host", json!(sy.name(*host))), ("kind", json!(sy.name(*kind))), ("connections", i(p.conn)), ("sources", i(p.srcs.len() as i64)),
                    ("last_seen", f(p.last)), ("blocked", i(p.blocked))])
            })
            .collect();
        let mut s: Vec<(&u32, &S)> = src.iter().collect();
        s.sort_by(|a, b| sy.name(*a.0).cmp(sy.name(*b.0)));
        s.sort_by(|a, b| b.1.conn.cmp(&a.1.conn));
        let sources = s
            .into_iter()
            .take(30)
            .map(|(k, p)| obj(vec![("source_ip", json!(sy.name(*k))), ("connections", i(p.conn)), ("domains", i(p.hosts.len() as i64)), ("last_seen", f(p.last))]))
            .collect();
        Ok((domains, sources, total))
    }
}

fn pass(sy: &Syms, f: &ActFilter, a: u32, host: u32, src: u32, c: &Cell) -> bool {
    let eq = |want: &Option<String>, v: u32| want.as_deref().is_none_or(|w| sy.name(v) == w);
    eq(&f.asset, a) && eq(&f.host, host) && eq(&f.source_ip, src) && eq(&f.kind, c.kind) && eq(&f.team, c.team)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE activity(id TEXT, ts REAL, asset TEXT, team TEXT, host TEXT, kind TEXT, source_ip TEXT, action TEXT);
             CREATE INDEX activity_time ON activity(ts);
             CREATE TABLE destinations(asset TEXT, host TEXT, kind TEXT, first_seen REAL, last_seen REAL, connections INTEGER, blocked INTEGER, PRIMARY KEY(asset,host));",
        )
        .unwrap();
        c
    }
    #[test]
    fn feed_load_apply_query() {
        let c = db();
        let now = 1_000_000.0 * 3600.0 + 1800.0;
        let ins = |t: f64, a: &str, h: &str, s: &str, act: &str| {
            c.execute("INSERT INTO activity VALUES('x',?1,?2,'团队1',?3,'domain',?4,?5)", params![t, a, h, s, act]).unwrap();
        };
        ins(now - 7200.0 - 100.0, "a1", "x.com", "1.1.1.1", "routed");
        ins(now - 7200.0 + 100.0, "a1", "x.com", "1.1.1.2", "blocked");
        ins(now - 60.0, "a2", "y.com", "1.1.1.1", "routed");
        let (s, e, b) = feed(&c, now).unwrap();
        assert!(ready(&c));
        assert_eq!(e - s, 3);
        assert_eq!(b.len(), 3);
        let mut r = Rollup::load(&c).unwrap();
        assert_eq!(r.cells, 3);
        ins(now - 30.0, "a2", "y.com", "1.1.1.1", "routed");
        let (s2, e2, b2) = feed(&c, now).unwrap();
        assert!(r.apply(s2, e2, &b2, now));
        // 窗口 2 小时：起点落在 now-7200 所在小时的中间 → 那一小时读原始记录（只含 +100 那条）
        let (nodes, targets, sources) = r.summary_parts(&c, now, 7200, &ActFilter::default(), 100, 0).unwrap();
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0]["asset"], "a2");
        assert_eq!(nodes[0]["connections"], 2);
        assert_eq!(nodes[1]["connections"], 1);
        assert_eq!(targets.len(), 2);
        assert_eq!(sources.len(), 2);
        let (d, s, t) = r.activity_parts(&c, now, 3 * 3600, &ActFilter { asset: Some("a1".into()), ..Default::default() }).unwrap();
        assert_eq!(t, 2);
        assert_eq!(d[0]["sources"], 2);
        assert_eq!(d[0]["blocked"], 1);
        assert_eq!(s.len(), 2);
        // 游标不衔接 → false
        assert!(!r.apply(e2 + 5, e2 + 6, &HashMap::new(), now));
    }
}
