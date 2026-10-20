//! 最近 24 小时访问记录的增量聚合（内存），替代默认 /behavior 与默认 /activity 每次对 26 万行重算。
//!
//! 与旧 SQL 同口径（compat::behavior::summary_live 默认参数、activity_aggregates）：
//! - 窗口 `ts >= now - 86400`；按 rowid 游标增量读新行（任何写入者都能看到），按 ts 从堆里淘汰；
//! - 每组的计数、去重计数（按成员引用计数）、min/max ts（有序多重集合）；
//! - 排序：先按分组键升序（SQLite GROUP BY 的输出顺序），再按排序键稳定排序（SQLite 带 LIMIT 的 ORDER BY 用带序号的临时 B 树，
//!   同值保持输入顺序；不带 LIMIT 的走外部排序器，同值是输入的逆序）。`fleetd legacy check-window` 在真实库上逐字节比对这两条路径。
use super::py::*;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, VecDeque};

#[derive(Clone, Copy, PartialEq, PartialOrd, Debug)]
struct Ts(f64);
impl Eq for Ts {}
impl Ord for Ts {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        self.0.partial_cmp(&o.0).unwrap_or(std::cmp::Ordering::Equal)
    }
}

#[derive(Clone, Copy)]
struct Row {
    ts: f64,
    asset: u32,
    team: u32,
    host: u32,
    kind: u32,
    src: u32,
    blocked: bool,
}

/// 计数型多重集合
#[derive(Default)]
struct Bag<K: std::hash::Hash + Eq>(HashMap<K, u32>);
impl<K: std::hash::Hash + Eq> Bag<K> {
    fn add(&mut self, k: K) {
        *self.0.entry(k).or_default() += 1;
    }
    fn del(&mut self, k: &K) {
        if let Some(n) = self.0.get_mut(k) {
            *n -= 1;
            if *n == 0 {
                self.0.remove(k);
            }
        }
    }
    fn len(&self) -> usize {
        self.0.len()
    }
}

/// 组里的最大时间，及那一刻最后加入的一行的附带值（旧 SQL 裸列取 max(ts) 那一行）。
/// 行只按全局时间先后淘汰（最早的先走）：被淘汰的行不晚于组里剩下的任何一行，所以组空之前最大时间和附带值都不会变——
/// 不需要按时间的多重集合（原来每组一棵 BTreeMap，24 h 约 86 万个时间点，是窗口内存的大头）。
/// 组里的最小时间只有 targets 的 first_in_window 要，渲染时对入选的那 100 组扫一遍行现算。
#[derive(Clone, Copy, Default)]
struct Last {
    ts: f64,
    tag: u32,
    set: bool,
}
impl Last {
    fn add(&mut self, t: f64, tag: u32) {
        match (self.set, Ts(t).cmp(&Ts(self.ts))) {
            (false, _) | (true, std::cmp::Ordering::Greater) => *self = Last { ts: t, tag, set: true },
            (true, std::cmp::Ordering::Equal) => self.tag = tag,
            _ => {}
        }
    }
    fn max(&self) -> Option<(f64, u32)> {
        self.set.then_some((self.ts, self.tag))
    }
}

#[derive(Default)]
struct NodeAgg {
    conn: i64,
    blocked: i64,
    ip_only: i64,
    domains: Bag<u32>,
    sources: Bag<u32>,
    times: Last, // tag = team
}

#[derive(Default)]
struct PairAgg {
    conn: i64,
    blocked: i64,
    distinct: Bag<u32>,
    times: Last, // tag = kind（targets）/ 0
}

/// store.activity 的过滤条件（窗口能算的那部分）
#[derive(Default, Debug, Clone)]
pub struct ActFilter {
    pub asset: Option<String>,
    pub team: Option<String>,
    pub kind: Option<String>,
    pub source_ip: Option<String>,
    pub host: Option<String>,
    /// action：blocked → Some(true)，routed → Some(false)
    pub blocked: Option<bool>,
}

#[derive(Default)]
pub struct Window {
    cursor: i64,
    loaded: bool,
    names: Vec<String>,
    ids: HashMap<String, u32>,
    /// 窗口里的行，按 ts 升序（同一时刻先后任意）：淘汰从头部弹出，新行多数追加在尾部
    rows: VecDeque<Row>,
    nodes: HashMap<u32, NodeAgg>,
    targets: HashMap<(u32, u32), PairAgg>,
    srcs: HashMap<(u32, u32), PairAgg>,
    domains: HashMap<(u32, u32), PairAgg>,
    all_src: HashMap<u32, PairAgg>,
    total: i64,
    pub last_sync_rows: usize,
}

impl Window {
    fn sym(&mut self, s: &str) -> u32 {
        if let Some(i) = self.ids.get(s) {
            return *i;
        }
        let i = self.names.len() as u32;
        self.names.push(s.to_string());
        self.ids.insert(s.to_string(), i);
        i
    }
    fn name(&self, i: u32) -> &str {
        &self.names[i as usize]
    }

    /// 符号表只增不减（主机名 / 来源 IP 每天都有新的）：远多于窗口里还在用的时，整个窗口作废、下次同步从库重建（约 0.5 s）
    pub fn bloated(&self) -> bool {
        let live = self.targets.len() + self.domains.len() + self.srcs.len() + self.all_src.len() + self.nodes.len() + 64;
        self.names.len() > 100_000 && self.names.len() > 4 * live
    }

    fn add(&mut self, r: Row) {
        let domain_kind = self.ids.get("domain").copied();
        let ip_kind = self.ids.get("ip").copied();
        let n = self.nodes.entry(r.asset).or_default();
        n.conn += 1;
        n.blocked += r.blocked as i64;
        n.ip_only += (Some(r.kind) == ip_kind) as i64;
        if Some(r.kind) == domain_kind {
            n.domains.add(r.host);
        }
        n.sources.add(r.src);
        n.times.add(r.ts, r.team);
        let t = self.targets.entry((r.asset, r.host)).or_default();
        t.conn += 1;
        t.blocked += r.blocked as i64;
        t.distinct.add(r.src);
        t.times.add(r.ts, r.kind);
        let s = self.srcs.entry((r.asset, r.src)).or_default();
        s.conn += 1;
        s.distinct.add(r.host);
        s.times.add(r.ts, 0);
        let d = self.domains.entry((r.host, r.kind)).or_default();
        d.conn += 1;
        d.blocked += r.blocked as i64;
        d.distinct.add(r.src);
        d.times.add(r.ts, 0);
        let a = self.all_src.entry(r.src).or_default();
        a.conn += 1;
        a.distinct.add(r.host);
        a.times.add(r.ts, 0);
        self.total += 1;
    }

    /// 淘汰一行（必须是窗口里最早的一行，见 Last）
    fn del(&mut self, r: &Row) {
        let domain_kind = self.ids.get("domain").copied();
        let ip_kind = self.ids.get("ip").copied();
        if let Some(n) = self.nodes.get_mut(&r.asset) {
            n.conn -= 1;
            n.blocked -= r.blocked as i64;
            n.ip_only -= (Some(r.kind) == ip_kind) as i64;
            if Some(r.kind) == domain_kind {
                n.domains.del(&r.host);
            }
            n.sources.del(&r.src);
            if n.conn == 0 {
                self.nodes.remove(&r.asset);
            }
        }
        fn dec<K: std::hash::Hash + Eq + Copy>(m: &mut HashMap<K, PairAgg>, k: K, r: &Row, member: u32) {
            if let Some(p) = m.get_mut(&k) {
                p.conn -= 1;
                p.blocked -= r.blocked as i64;
                p.distinct.del(&member);
                if p.conn == 0 {
                    m.remove(&k);
                }
            }
        }
        dec(&mut self.targets, (r.asset, r.host), r, r.src);
        dec(&mut self.srcs, (r.asset, r.src), r, r.host);
        dec(&mut self.domains, (r.host, r.kind), r, r.src);
        dec(&mut self.all_src, r.src, r, r.host);
        self.total -= 1;
    }

    /// 新行并入有序的行队列：按 ts 排序后与尾部归并（晚到的旧行只挪动比它新的那一段）
    fn merge_rows(&mut self, mut new: Vec<Row>) {
        if new.is_empty() {
            return;
        }
        new.sort_by(|a, b| Ts(a.ts).cmp(&Ts(b.ts)));
        let first = Ts(new[0].ts);
        let mut tail: Vec<Row> = vec![];
        while self.rows.back().is_some_and(|r| Ts(r.ts) > first) {
            tail.push(self.rows.pop_back().unwrap());
        }
        tail.reverse();
        // 按 25% 增长预留（默认翻倍会让 30 万行的队列多占一倍容量）
        let need = self.rows.len() + tail.len() + new.len();
        if need > self.rows.capacity() {
            let target = need.max(self.rows.len() + self.rows.len() / 4);
            self.rows.reserve_exact(target - self.rows.len());
        }
        let (mut i, mut j) = (0, 0);
        while i < tail.len() || j < new.len() {
            let take_tail = j >= new.len() || (i < tail.len() && Ts(tail[i].ts) <= Ts(new[j].ts));
            if take_tail {
                self.rows.push_back(tail[i]);
                i += 1;
            } else {
                self.rows.push_back(new[j]);
                j += 1;
            }
        }
    }

    /// 读入新行、淘汰窗口外的行
    pub fn sync(&mut self, db: &rusqlite::Connection, now: f64) -> R<()> {
        let cutoff = now - 86400.0;
        let (sql, arg) = if self.loaded {
            ("SELECT rowid, ts, asset, team, host, kind, source_ip, action FROM activity WHERE rowid>?1", rusqlite::types::Value::Integer(self.cursor))
        } else {
            ("SELECT rowid, ts, asset, team, host, kind, source_ip, action FROM activity INDEXED BY activity_time WHERE ts>=?1", rusqlite::types::Value::Real(cutoff))
        };
        let mut fresh: Vec<Row> = vec![];
        {
            let mut st = db.prepare_cached(sql)?;
            let mut q = st.query([arg])?;
            while let Some(r) = q.next()? {
                // 行号游标保证每行只读一次（初次加载按 ts 读，之后按 rowid > 游标）
                let rid: i64 = r.get(0)?;
                self.cursor = self.cursor.max(rid);
                let ts: f64 = match r.get_ref(1)? {
                    rusqlite::types::ValueRef::Real(x) => x,
                    rusqlite::types::ValueRef::Integer(x) => x as f64,
                    _ => continue,
                };
                if ts < cutoff {
                    continue;
                }
                let s = |i: usize| -> String { r.get::<_, Option<String>>(i).ok().flatten().unwrap_or_default() };
                let (asset, team, host, kind, src, action) = (s(2), s(3), s(4), s(5), s(6), s(7));
                let row = Row { ts, asset: self.sym(&asset), team: self.sym(&team), host: self.sym(&host), kind: self.sym(&kind), src: self.sym(&src), blocked: action == "blocked" };
                self.add(row);
                fresh.push(row);
            }
        }
        let n = fresh.len();
        self.merge_rows(fresh);
        if !self.loaded {
            // 初次加载按 ts 读的，游标取当前最大 rowid
            self.cursor = self.cursor.max(db.query_row("SELECT coalesce(max(rowid),0) FROM activity", [], |r| r.get(0))?);
            self.loaded = true;
        }
        while self.rows.front().is_some_and(|r| r.ts < cutoff) {
            let r = self.rows.pop_front().unwrap();
            self.del(&r);
        }
        if self.rows.capacity() > 4096 && self.rows.len() * 2 < self.rows.capacity() {
            self.rows.shrink_to(self.rows.len() + self.rows.len() / 4);
        }
        self.last_sync_rows = n;
        Ok(())
    }

    fn obj(pairs: Vec<(&str, Value)>) -> Value {
        let mut m = Map::new();
        for (k, v) in pairs {
            m.insert(k.into(), v);
        }
        Value::Object(m)
    }

    /// summary_live({}) 的 nodes / targets(limit 100, offset 0) / sources
    pub fn summary_parts(&self, db: &rusqlite::Connection, limit: usize) -> R<(Vec<Value>, Vec<Value>, Vec<Value>)> {
        // nodes：GROUP BY asset ORDER BY last_seen DESC
        // 没有 LIMIT 的 ORDER BY 走 SQLite 的外部排序器：记录插在链表头，同值按分组顺序的逆序输出
        let mut nodes: Vec<(&u32, &NodeAgg)> = self.nodes.iter().collect();
        nodes.sort_by(|a, b| self.name(*b.0).cmp(self.name(*a.0)));
        nodes.sort_by(|a, b| b.1.times.max().map(|x| Ts(x.0)).cmp(&a.1.times.max().map(|x| Ts(x.0))));
        let nodes: Vec<Value> = nodes
            .into_iter()
            .map(|(k, n)| {
                let (last, team) = n.times.max().unwrap_or((0.0, 0));
                Self::obj(vec![("asset", json!(self.name(*k))), ("team", json!(self.name(team))), ("connections", i(n.conn)), ("domains", i(n.domains.len() as i64)),
                    ("sources", i(n.sources.len() as i64)), ("blocked", i(n.blocked)), ("ip_only", i(n.ip_only)), ("last_seen", f(last))])
            })
            .collect();
        // targets：GROUP BY asset,host ORDER BY last_seen DESC, asset, host LIMIT
        let mut t: Vec<(&(u32, u32), &PairAgg)> = self.targets.iter().collect();
        t.sort_by(|a, b| {
            b.1.times.max().map(|x| Ts(x.0)).cmp(&a.1.times.max().map(|x| Ts(x.0)))
                .then_with(|| self.name(a.0 .0).cmp(self.name(b.0 .0)))
                .then_with(|| self.name(a.0 .1).cmp(self.name(b.0 .1)))
        });
        let top: Vec<(&(u32, u32), &PairAgg)> = t.into_iter().take(limit).collect();
        // 入选各组的最早时间（first_in_window）：扫一遍窗口行（行按时间升序，每组第一次出现就是最早）
        let mut first_of: HashMap<(u32, u32), f64> = top.iter().map(|(k, _)| (**k, f64::NAN)).collect();
        let mut left = first_of.len();
        for r in self.rows.iter() {
            if left == 0 {
                break;
            }
            if let Some(x) = first_of.get_mut(&(r.asset, r.host)) {
                if x.is_nan() {
                    *x = r.ts;
                    left -= 1;
                }
            }
        }
        let mut targets = vec![];
        for ((asset, host), p) in top {
            let (last, kind) = p.times.max().unwrap_or((0.0, 0));
            let first = first_of.get(&(*asset, *host)).copied().filter(|x| !x.is_nan()).unwrap_or(0.0);
            let d: Option<(Value, Value)> = db
                .prepare_cached("SELECT first_seen, connections FROM destinations WHERE asset=?1 AND host=?2")?
                .query_row([self.name(*asset), self.name(*host)], |r| Ok((sql_to_json(r.get_ref(0)?), sql_to_json(r.get_ref(1)?))))
                .ok();
            let (fs, tr) = match d {
                Some((a, b)) => (if a.is_null() { f(first) } else { a }, if b.is_null() { i(p.conn) } else { b }),
                None => (f(first), i(p.conn)),
            };
            targets.push(Self::obj(vec![("asset", json!(self.name(*asset))), ("host", json!(self.name(*host))), ("kind", json!(self.name(kind))),
                ("connections", i(p.conn)), ("sources", i(p.distinct.len() as i64)), ("blocked", i(p.blocked)), ("first_in_window", f(first)),
                ("last_seen", f(last)), ("first_seen", fs), ("total_recorded", tr)]));
        }
        // sources：GROUP BY asset,source_ip ORDER BY last_seen DESC LIMIT 100
        let mut s: Vec<(&(u32, u32), &PairAgg)> = self.srcs.iter().collect();
        s.sort_by(|a, b| self.name(a.0 .0).cmp(self.name(b.0 .0)).then_with(|| self.name(a.0 .1).cmp(self.name(b.0 .1))));
        s.sort_by(|a, b| b.1.times.max().map(|x| Ts(x.0)).cmp(&a.1.times.max().map(|x| Ts(x.0))));
        let sources = s
            .into_iter()
            .take(100)
            .map(|((asset, src), p)| {
                Self::obj(vec![("asset", json!(self.name(*asset))), ("source_ip", json!(self.name(*src))), ("connections", i(p.conn)),
                    ("targets", i(p.distinct.len() as i64)), ("last_seen", f(p.times.max().map(|x| x.0).unwrap_or(0.0)))])
            })
            .collect();
        Ok((nodes, targets, sources))
    }

    /// 过滤后的 activity 聚合（store.activity 带 asset/team/action/kind/source_ip/host 过滤、默认 24 h 时）：扫内存里的窗口行
    pub fn activity_filtered(&self, f: &ActFilter) -> (Vec<Value>, Vec<Value>, i64) {
        let want = |id: Option<u32>, v: u32| id.is_none_or(|x| x == v);
        let (fa, ft, fk, fs, fh) = (f.asset.as_deref().map(|x| self.ids.get(x).copied()), f.team.as_deref().map(|x| self.ids.get(x).copied()),
            f.kind.as_deref().map(|x| self.ids.get(x).copied()), f.source_ip.as_deref().map(|x| self.ids.get(x).copied()), f.host.as_deref().map(|x| self.ids.get(x).copied()));
        // 过滤值在窗口里从没出现过 → 空结果
        for x in [fa, ft, fk, fs, fh].into_iter().flatten() {
            if x.is_none() {
                return (vec![], vec![], 0);
            }
        }
        let (fa, ft, fk, fs, fh) = (fa.flatten(), ft.flatten(), fk.flatten(), fs.flatten(), fh.flatten());
        // 一次性聚合：只要最大时间，不需要可淘汰的多重集合（PairAgg 的 Bag / Times 是给增量窗口用的）
        #[derive(Default)]
        struct Lite {
            conn: i64,
            blocked: i64,
            last: f64,
            distinct: std::collections::HashSet<u32>,
        }
        let mut dom: HashMap<(u32, u32), Lite> = HashMap::new();
        let mut src: HashMap<u32, Lite> = HashMap::new();
        let mut total = 0i64;
        for r in self.rows.iter() {
            if !(want(fa, r.asset) && want(ft, r.team) && want(fk, r.kind) && want(fs, r.src) && want(fh, r.host) && f.blocked.is_none_or(|b| b == r.blocked)) {
                continue;
            }
            total += 1;
            let d = dom.entry((r.host, r.kind)).or_default();
            d.conn += 1;
            d.blocked += r.blocked as i64;
            d.distinct.insert(r.src);
            d.last = d.last.max(r.ts);
            let a = src.entry(r.src).or_default();
            a.conn += 1;
            a.distinct.insert(r.host);
            a.last = a.last.max(r.ts);
        }
        let dv: Vec<((u32, u32), i64, i64, f64, i64)> = dom.into_iter().map(|(k, p)| (k, p.conn, p.distinct.len() as i64, p.last, p.blocked)).collect();
        let sv: Vec<(u32, i64, i64, f64)> = src.into_iter().map(|(k, p)| (k, p.conn, p.distinct.len() as i64, p.last)).collect();
        let (domains, sources) = self.render_rows(dv, sv);
        (domains, sources, total)
    }

    /// render_activity 的通用形态：(键, 连接, 去重数, 最后时间[, 阻止])
    fn render_rows(&self, mut d: Vec<((u32, u32), i64, i64, f64, i64)>, mut s: Vec<(u32, i64, i64, f64)>) -> (Vec<Value>, Vec<Value>) {
        d.sort_by(|a, b| self.name(a.0 .0).cmp(self.name(b.0 .0)).then_with(|| self.name(a.0 .1).cmp(self.name(b.0 .1))));
        d.sort_by(|a, b| b.1.cmp(&a.1));
        let domains = d
            .into_iter()
            .take(40)
            .map(|((host, kind), conn, n, last, blocked)| {
                Self::obj(vec![("host", json!(self.name(host))), ("kind", json!(self.name(kind))), ("connections", i(conn)),
                    ("sources", i(n)), ("last_seen", f(last)), ("blocked", i(blocked))])
            })
            .collect();
        s.sort_by(|a, b| self.name(a.0).cmp(self.name(b.0)));
        s.sort_by(|a, b| b.1.cmp(&a.1));
        let sources = s
            .into_iter()
            .take(30)
            .map(|(src, conn, n, last)| Self::obj(vec![("source_ip", json!(self.name(src))), ("connections", i(conn)), ("domains", i(n)), ("last_seen", f(last))]))
            .collect();
        (domains, sources)
    }

    fn render_activity(&self, dom: &HashMap<(u32, u32), PairAgg>, src: &HashMap<u32, PairAgg>) -> (Vec<Value>, Vec<Value>) {
        let mut d: Vec<(&(u32, u32), &PairAgg)> = dom.iter().collect();
        d.sort_by(|a, b| self.name(a.0 .0).cmp(self.name(b.0 .0)).then_with(|| self.name(a.0 .1).cmp(self.name(b.0 .1))));
        d.sort_by(|a, b| b.1.conn.cmp(&a.1.conn));
        let domains = d
            .into_iter()
            .take(40)
            .map(|((host, kind), p)| {
                Self::obj(vec![("host", json!(self.name(*host))), ("kind", json!(self.name(*kind))), ("connections", i(p.conn)),
                    ("sources", i(p.distinct.len() as i64)), ("last_seen", f(p.times.max().map(|x| x.0).unwrap_or(0.0))), ("blocked", i(p.blocked))])
            })
            .collect();
        let mut s: Vec<(&u32, &PairAgg)> = src.iter().collect();
        s.sort_by(|a, b| self.name(*a.0).cmp(self.name(*b.0)));
        s.sort_by(|a, b| b.1.conn.cmp(&a.1.conn));
        let sources = s
            .into_iter()
            .take(30)
            .map(|(src, p)| {
                Self::obj(vec![("source_ip", json!(self.name(*src))), ("connections", i(p.conn)), ("domains", i(p.distinct.len() as i64)),
                    ("last_seen", f(p.times.max().map(|x| x.0).unwrap_or(0.0)))])
            })
            .collect();
        (domains, sources)
    }

    /// 内存读数（/v2/api/v1/debug/mem）
    pub fn mem_stats(&self) -> Value {
        let bag = |m: &HashMap<(u32, u32), PairAgg>| m.values().map(|p| p.distinct.len()).sum::<usize>();
        json!({"rows": self.rows.len(), "rows_cap": self.rows.capacity(), "names": self.names.len(),
            "names_bytes": self.names.iter().map(|x| x.capacity() * 2 + 48).sum::<usize>(), "nodes": self.nodes.len(),
            "targets": self.targets.len(), "target_bag": bag(&self.targets), "srcs": self.srcs.len(), "src_bag": bag(&self.srcs),
            "domains": self.domains.len(), "domain_bag": bag(&self.domains), "all_src": self.all_src.len(),
            "all_src_bag": self.all_src.values().map(|p| p.distinct.len()).sum::<usize>()})
    }

    /// activity_aggregates 的 domains / sources / total
    pub fn activity_parts(&self) -> (Vec<Value>, Vec<Value>, i64) {
        let (domains, sources) = self.render_activity(&self.domains, &self.all_src);
        (domains, sources, self.total)
    }

    #[allow(dead_code)]
    fn activity_parts_old(&self) -> (Vec<Value>, Vec<Value>, i64) {
        let mut d: Vec<(&(u32, u32), &PairAgg)> = self.domains.iter().collect();
        d.sort_by(|a, b| self.name(a.0 .0).cmp(self.name(b.0 .0)).then_with(|| self.name(a.0 .1).cmp(self.name(b.0 .1))));
        d.sort_by(|a, b| b.1.conn.cmp(&a.1.conn));
        let domains = d
            .into_iter()
            .take(40)
            .map(|((host, kind), p)| {
                Self::obj(vec![("host", json!(self.name(*host))), ("kind", json!(self.name(*kind))), ("connections", i(p.conn)),
                    ("sources", i(p.distinct.len() as i64)), ("last_seen", f(p.times.max().map(|x| x.0).unwrap_or(0.0))), ("blocked", i(p.blocked))])
            })
            .collect();
        let mut s: Vec<(&u32, &PairAgg)> = self.all_src.iter().collect();
        s.sort_by(|a, b| self.name(*a.0).cmp(self.name(*b.0)));
        s.sort_by(|a, b| b.1.conn.cmp(&a.1.conn));
        let sources = s
            .into_iter()
            .take(30)
            .map(|(src, p)| {
                Self::obj(vec![("source_ip", json!(self.name(*src))), ("connections", i(p.conn)), ("domains", i(p.distinct.len() as i64)),
                    ("last_seen", f(p.times.max().map(|x| x.0).unwrap_or(0.0)))])
            })
            .collect();
        (domains, sources, self.total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{params, Connection};

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE activity(id TEXT, ts REAL, asset TEXT, team TEXT, relay TEXT, source_ip TEXT, host TEXT, port INTEGER, protocol TEXT, action TEXT, client TEXT, kind TEXT);
             CREATE INDEX activity_time ON activity(ts);
             CREATE TABLE destinations(asset TEXT, host TEXT, first_seen REAL, connections INTEGER, PRIMARY KEY(asset, host));",
        )
        .unwrap();
        c
    }

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self, n: u64) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (self.0 >> 33) % n
        }
    }

    fn insert(c: &Connection, rng: &mut Lcg, ts: f64) {
        let asset = format!("a{}", rng.next(6));
        let team = format!("团队{}", rng.next(2) + 1);
        let host = if rng.next(4) == 0 { format!("10.0.0.{}", rng.next(5)) } else { format!("h{}.example", rng.next(40)) };
        let kind = if host.starts_with("10.") { "ip" } else { "domain" };
        let src = format!("1.1.1.{}", rng.next(4));
        let action = if rng.next(5) == 0 { "blocked" } else { "routed" };
        c.execute("INSERT INTO activity(ts,asset,team,source_ip,host,action,kind) VALUES(?1,?2,?3,?4,?5,?6,?7)", params![ts, asset, team, src, host, action, kind]).unwrap();
    }

    fn outputs(w: &Window, c: &Connection) -> String {
        let (n, t, s) = w.summary_parts(c, 100).unwrap();
        let (d, a, total) = w.activity_parts();
        let mut out = vec![json!([n, t, s, d, a, total])];
        for f in [ActFilter { asset: Some("a1".into()), ..Default::default() }, ActFilter { team: Some("团队2".into()), blocked: Some(true), ..Default::default() },
            ActFilter { kind: Some("ip".into()), ..Default::default() }, ActFilter { source_ip: Some("1.1.1.3".into()), ..Default::default() }] {
            let (d, s, n) = w.activity_filtered(&f);
            out.push(json!([d, s, n]));
        }
        Value::Array(out).to_string()
    }

    /// 增量同步（乱序晚到的行、逐步淘汰、同一时刻多行）与在最终时刻整窗重载的结果逐字节相同
    #[test]
    fn incremental_matches_fresh_load() {
        let c = db();
        let mut rng = Lcg(7);
        let t0 = 1_790_000_000.0;
        // 初始：过去 30 小时的行（最早 6 小时已在窗口外）
        for i in 0..3000 {
            insert(&c, &mut rng, (t0 - 30.0 * 3600.0 + i as f64 * 36.0).floor());
        }
        let mut w = Window::default();
        w.sync(&c, t0).unwrap();
        let mut now = t0;
        for step in 0..60 {
            now += 600.0;
            // 新行：多数是刚发生的，少数晚到（最多 2 小时前）、同一秒多行
            for _ in 0..40 {
                let late = if rng.next(6) == 0 { rng.next(7200) as f64 } else { rng.next(600) as f64 };
                insert(&c, &mut rng, (now - late).floor());
            }
            if step % 7 == 0 {
                for _ in 0..5 {
                    insert(&c, &mut rng, now - 1.0);
                }
            }
            w.sync(&c, now).unwrap();
            if step % 10 == 9 {
                let mut fresh = Window::default();
                fresh.sync(&c, now).unwrap();
                assert_eq!(w.rows.len(), fresh.rows.len());
                assert_eq!(outputs(&w, &c), outputs(&fresh, &c), "step {step}");
            }
        }
        assert!(w.rows.iter().zip(w.rows.iter().skip(1)).all(|(a, b)| a.ts <= b.ts), "行队列保持按时间有序");
    }
}
