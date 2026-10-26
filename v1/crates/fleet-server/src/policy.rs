//! 访问策略（团队/节点的域名黑白名单）：规范化与校验、有效策略继承、编译成下发给 agent 的形状、
//! 预览用的匹配。纯函数在上半部分，查库的函数在下半部分；发布/回滚的事务在 `api/policy.rs`。
//!
//! 语义（旧系统规格 §5.5）：有效策略 = 节点策略（mode ≠ inherit）> 团队策略 > 默认 observe。

use crate::crypto::sha256;
use crate::names::display_name;
use fleet_proto::{EntryPolicy, PolicyMode};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;
use time::OffsetDateTime;
use uuid::Uuid;

/// 每份策略最多多少个域名
pub const MAX_DOMAINS: usize = 300;
pub const MAX_NOTE_CHARS: usize = 500;
/// 发布 / 回滚 / 丢弃 / 改草稿之间的互斥（事务级咨询锁）；新建入口拿共享锁，和发布互斥
pub const LOCK_KEY: i64 = 0x706f_6c69_6379;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Observe,
    Allowlist,
    Denylist,
    Blocked,
    /// 仅节点：跟随团队
    Inherit,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Observe => "observe",
            Mode::Allowlist => "allowlist",
            Mode::Denylist => "denylist",
            Mode::Blocked => "blocked",
            Mode::Inherit => "inherit",
        }
    }
    pub fn parse(s: &str) -> Option<Mode> {
        Some(match s {
            "observe" => Mode::Observe,
            "allowlist" => Mode::Allowlist,
            "denylist" => Mode::Denylist,
            "blocked" => Mode::Blocked,
            "inherit" => Mode::Inherit,
            _ => return None,
        })
    }
    pub fn needs_domains(self) -> bool { matches!(self, Mode::Allowlist | Mode::Denylist) }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Team,
    Node,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Team => "team",
            Scope::Node => "node",
        }
    }
    pub fn parse(s: &str) -> Option<Scope> {
        match s {
            "team" => Some(Scope::Team),
            "node" => Some(Scope::Node),
            _ => None,
        }
    }
    /// 该范围的默认值：团队 observe；节点 inherit（跟随团队）
    pub fn default_data(self) -> Data {
        Data { mode: if self == Scope::Node { Mode::Inherit } else { Mode::Observe }, domains: vec![], note: String::new() }
    }
}

/// 一份策略的内容（生效值与草稿同形）
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Data {
    pub mode: Mode,
    /// 规范化后的域名：`example.com`（含子域）、`=example.com`（仅精确）；已排序去重
    #[serde(default)]
    pub domains: Vec<String>,
    #[serde(default)]
    pub note: String,
}

impl Default for Data {
    fn default() -> Self { Data { mode: Mode::Observe, domains: vec![], note: String::new() } }
}

impl Data {
    /// 不用域名的模式（仅观察、暂停新连接、跟随团队）不留域名：编辑器在这些模式下隐藏域名框，
    /// 留着旧域名会让看起来一样的两份规则不相等（多出空草稿、预览「仅观察 → 仅观察」、删团队被拒）
    pub fn normalized(mut self) -> Data {
        if !self.mode.needs_domains() {
            self.domains.clear();
        }
        self
    }

    /// 给人看的一句话：「白名单 · 3 个域名」「暂停新连接」
    pub fn summary(&self) -> String {
        let m = crate::meta::label(crate::meta::POLICY_MODES, self.mode.as_str());
        if self.mode.needs_domains() { format!("{m} · {} 个域名", self.domains.len()) } else { m.to_string() }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Node,
    Team,
    Default,
}

// ── 域名规范化与校验 ─────────────────────────────────────────────────────────

fn is_sep(c: char) -> bool { c.is_whitespace() || matches!(c, ',' | '，' | '、' | ';' | '；') }

/// 把用户输入（逗号/中文逗号/空白分隔）规范化成排序去重的域名列表。错误信息中文并指出是哪一个域名。
pub fn normalize_domains(raw: &str) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = vec![];
    for tok in raw.split(is_sep).filter(|t| !t.is_empty()) {
        out.push(normalize_one(tok)?);
    }
    finish(out)
}

/// 前端传数组时：每个元素同样按分隔符再切一遍（容忍一行里写了多个）
pub fn normalize_list<S: AsRef<str>>(items: &[S]) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = vec![];
    for it in items {
        for tok in it.as_ref().split(is_sep).filter(|t| !t.is_empty()) {
            out.push(normalize_one(tok)?);
        }
    }
    finish(out)
}

fn finish(mut out: Vec<String>) -> Result<Vec<String>, String> {
    out.sort();
    out.dedup();
    if out.len() > MAX_DOMAINS {
        return Err(format!("每份策略最多 {MAX_DOMAINS} 个域名，现在有 {} 个", out.len()));
    }
    Ok(out)
}

fn normalize_one(tok: &str) -> Result<String, String> {
    let shown = tok.chars().take(80).collect::<String>();
    let (exact, body) = match tok.strip_prefix('=') {
        Some(rest) => (true, rest),
        None => (false, tok),
    };
    let d = body.to_lowercase();
    let d = d.trim_end_matches('.');
    if d.is_empty() {
        return Err(format!("「{shown}」后面没有域名"));
    }
    if !d.is_ascii() {
        return Err(format!("「{shown}」含有非 ASCII 字符，国际化域名请填 xn-- 形式"));
    }
    if d.contains("://") || d.contains(['/', '?', '#', '@']) {
        return Err(format!("「{shown}」看起来是网址，请只填域名（不要 http://、路径、参数）"));
    }
    for p in ["domain:", "full:", "regexp:", "keyword:", "geosite:", "ext:"] {
        if d.starts_with(p) {
            return Err(format!("「{shown}」不要带 {p} 前缀：直接填域名，精确匹配在前面加 ="));
        }
    }
    let ip_like = d.parse::<IpAddr>().is_ok() || d.trim_matches(['[', ']']).parse::<IpAddr>().is_ok() || d.split('.').all(|l| !l.is_empty() && l.bytes().all(|b| b.is_ascii_digit()));
    if ip_like {
        return Err(format!("「{shown}」是 IP 地址：请填写域名，不支持 IP 地址白名单"));
    }
    if d.contains(':') {
        return Err(format!("「{shown}」带了端口：策略按域名匹配，不要写端口"));
    }
    if d.contains('*') {
        return Err(format!("「{shown}」不要写通配符 *：example.com 已经包含它的所有子域名"));
    }
    if d.chars().any(|c| "^$()[]{}\\|+!%&'\"`<>~=,".contains(c)) {
        return Err(format!("「{shown}」含有不支持的字符，不支持正则表达式"));
    }
    if d.len() > 253 {
        return Err(format!("「{shown}」太长（域名总长不超过 253 个字符）"));
    }
    if !d.contains('.') {
        return Err(format!("「{shown}」必须包含点（例如 example.com）"));
    }
    for label in d.split('.') {
        if label.is_empty() {
            return Err(format!("「{shown}」里有空的标签（连续的点或开头的点）"));
        }
        if label.len() > 63 {
            return Err(format!("「{shown}」的标签「{label}」超过 63 个字符"));
        }
        if !label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Err(format!("「{shown}」的标签「{label}」只能包含字母、数字和连字符"));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(format!("「{shown}」的标签「{label}」不能以连字符开头或结尾"));
        }
    }
    Ok(if exact { format!("={d}") } else { d.to_string() })
}

/// 校验一份草稿（模式 + 域名 + 备注），返回规范化后的内容
pub fn validate_draft(scope: Scope, mode: &str, domains: Vec<String>, note: &str) -> Result<Data, String> {
    let mode = Mode::parse(mode).ok_or_else(|| "模式只能是 observe、allowlist、denylist、blocked、inherit".to_string())?;
    if mode == Mode::Inherit && scope == Scope::Team {
        return Err("团队没有可继承的上一级，模式不能是 inherit".into());
    }
    if mode.needs_domains() && domains.is_empty() {
        return Err("启用白名单或黑名单前必须填写域名".into());
    }
    let note = note.trim();
    if note.chars().count() > MAX_NOTE_CHARS {
        return Err(format!("备注最多 {MAX_NOTE_CHARS} 个字符"));
    }
    Ok(Data { mode, domains, note: note.to_string() }.normalized())
}

// ── 匹配（预览用）与编译（下发用）────────────────────────────────────────────────

fn match_one(p: &str, host: &str) -> bool {
    let (exact, p) = match p.strip_prefix('=') {
        Some(r) => (true, r),
        None => (false, p),
    };
    if let Some(base) = host.strip_prefix("*.") {
        // 访问汇总里被合并的随机子域名：只有整棵子树都落在规则里的 domain 规则才算命中（近似）
        return !exact && (base == p || base.ends_with(&format!(".{p}")));
    }
    host == p || (!exact && host.ends_with(&format!(".{p}")))
}

/// 名单里有没有命中这个目标（`h == p` 或 `h` 以 `.p` 结尾；`=` 规则只等值）
pub fn list_matches(domains: &[String], host: &str) -> bool {
    domains.iter().any(|p| match_one(p, host))
}

/// 按这份策略，到 `host` 的连接会不会被阻止
pub fn would_block(d: &Data, host: &str) -> bool {
    match d.mode {
        Mode::Observe | Mode::Inherit => false,
        Mode::Blocked => true,
        Mode::Allowlist => !list_matches(&d.domains, host),
        Mode::Denylist => list_matches(&d.domains, host),
    }
}

/// 有效策略 → 下发给 agent 的形状（observe → None）
pub fn compile(d: &Data) -> Option<EntryPolicy> {
    let xray = |p: &String| match p.strip_prefix('=') {
        Some(r) => format!("full:{r}"),
        None => format!("domain:{p}"),
    };
    match d.mode {
        Mode::Observe | Mode::Inherit => None,
        Mode::Blocked => Some(EntryPolicy { mode: PolicyMode::Blocked, domains: vec![] }),
        Mode::Allowlist => Some(EntryPolicy { mode: PolicyMode::Allowlist, domains: d.domains.iter().map(xray).collect() }),
        Mode::Denylist => Some(EntryPolicy { mode: PolicyMode::Denylist, domains: d.domains.iter().map(xray).collect() }),
    }
}

// ── 有效策略 ────────────────────────────────────────────────────────────────

/// 一组策略（团队 + 节点各一张表）。可以是「生效值」视图，也可以是「草稿覆盖后」的视图。
#[derive(Clone, Debug, Default)]
pub struct PolicySet {
    pub team: HashMap<Uuid, Data>,
    pub node: HashMap<Uuid, Data>,
}

impl PolicySet {
    /// 节点的有效策略：节点策略（mode ≠ inherit）> 团队策略 > 默认 observe。返回的 mode 永远不是 inherit。
    pub fn effective(&self, node: Uuid, team: Option<Uuid>) -> (Data, Source) {
        if let Some(n) = self.node.get(&node).filter(|n| n.mode != Mode::Inherit) {
            return (n.clone(), Source::Node);
        }
        if let Some(t) = team.and_then(|t| self.team.get(&t)).filter(|t| t.mode != Mode::Inherit) {
            return (t.clone(), Source::Team);
        }
        (Data::default(), Source::Default)
    }
}

/// 表里的一行
#[derive(Clone, Debug)]
pub struct Row {
    pub scope: Scope,
    pub scope_id: Uuid,
    pub cur: Data,
    pub draft: Option<Data>,
    pub revision: i32,
    pub updated_by: Option<String>,
    pub updated_at: OffsetDateTime,
}

#[derive(sqlx::FromRow)]
struct RowRaw {
    scope: String,
    scope_id: Uuid,
    mode: String,
    domains: Vec<String>,
    note: String,
    draft: Option<serde_json::Value>,
    revision: i32,
    updated_by: Option<String>,
    updated_at: OffsetDateTime,
}

impl RowRaw {
    /// 生效值与草稿都规范化（旧数据里不用域名的模式可能还留着域名）；规范化后与生效值相同的草稿等于没有
    fn into_row(self) -> Option<Row> {
        let cur = Data { mode: Mode::parse(&self.mode)?, domains: self.domains, note: self.note }.normalized();
        Some(Row {
            scope: Scope::parse(&self.scope)?,
            scope_id: self.scope_id,
            draft: self.draft.and_then(|v| serde_json::from_value::<Data>(v).ok()).map(Data::normalized).filter(|d| *d != cur),
            cur,
            revision: self.revision,
            updated_by: self.updated_by,
            updated_at: self.updated_at,
        })
    }
}

impl Row {
    /// 没有生效的设置、也没有草稿：等于这个范围没有记录
    pub fn is_default(&self) -> bool { self.cur == self.scope.default_data() && self.draft.is_none() }

    /// 「白名单 · 3 个域名」，有未发布的草稿时后面加一句
    pub fn summary(&self) -> String {
        let cur = self.cur.summary();
        if self.draft.is_some() { format!("{cur}，另有未发布的草稿") } else { cur }
    }
}

const ROW_COLS: &str = "scope, scope_id, mode, domains, note, draft, revision, updated_by, updated_at";

pub async fn load_rows<'e, E: sqlx::PgExecutor<'e>>(ex: E, lock: bool) -> sqlx::Result<Vec<Row>> {
    let sql = format!("select {ROW_COLS} from policy order by scope, scope_id{}", if lock { " for update" } else { "" });
    let raw: Vec<RowRaw> = sqlx::query_as(&sql).fetch_all(ex).await?;
    Ok(raw.into_iter().filter_map(RowRaw::into_row).collect())
}

/// 一个范围的那一行（还没有记录为 None），规范化同 `load_rows`
pub async fn load_row<'e, E: sqlx::PgExecutor<'e>>(ex: E, scope: Scope, id: Uuid) -> sqlx::Result<Option<Row>> {
    let sql = format!("select {ROW_COLS} from policy where scope = $1 and scope_id = $2");
    let raw: Option<RowRaw> = sqlx::query_as(&sql).bind(scope.as_str()).bind(id).fetch_optional(ex).await?;
    Ok(raw.and_then(RowRaw::into_row))
}

/// 节点单独设的访问策略（已发布的与草稿）改挂到另一个节点：替换节点交换编号时，「跟着客户走」的一部分（和来源画像一样）。
/// 要换下来的节点当前没有（或只是默认的跟随团队）节点级策略时，当前的策略行不动；历史版本的快照照样改写（旧节点曾经设过又清掉的，
/// 回滚到那些版本也不能把策略放回进了备用的旧节点；重复执行结果不变）。新节点自己有节点级策略的以旧节点的为准，返回里带上被覆盖的那份。
/// 调用方须已拿访问规则的锁（`api::policy::lock`，在事务开头、任何行锁之前）。线路机的期望状态由调用方同事务里刷新代数
pub async fn move_node(conn: &mut sqlx::PgConnection, from: Uuid, to: Uuid, actor: &str) -> sqlx::Result<NodeMoved> {
    let mut moved = NodeMoved::default();
    if let Some(old) = load_row(&mut *conn, Scope::Node, from).await?.filter(|r| !r.is_default()) {
        moved.replaced = load_row(&mut *conn, Scope::Node, to).await?.filter(|r| !r.is_default()).map(|r| r.summary());
        moved.summary = Some(old.summary());
        sqlx::query("delete from policy where scope = 'node' and scope_id = $1").bind(to).execute(&mut *conn).await?;
        sqlx::query("update policy set scope_id = $2, revision = revision + 1, updated_by = $3, updated_at = now() where scope = 'node' and scope_id = $1")
            .bind(from)
            .bind(to)
            .bind(actor)
            .execute(&mut *conn)
            .await?;
    }
    // 历史版本的快照里这个节点也改挂到新节点（新节点原有的那项丢掉，同上以旧节点的为准）：不然回滚到替换前的任一版，
    // 会把策略放回已进备用的旧节点、把新节点写回默认。快照按 (团队在前, id) 排序，改挂后按同一顺序重排
    // 只改写含旧节点那项的版本（所以再来一遍是空操作）；同一版里新节点也有一项的，丢掉新节点的（以旧节点的为准）。
    // 被改写的版本号，以及其中丢掉了新节点原有项的，审计里记下，改写可追溯
    let hit: Vec<(i64, bool)> = sqlx::query_as(
        "with hit as materialized (
             select v.seq, exists (select 1 from jsonb_array_elements(v.snapshot) e where e->>'scope' = 'node' and e->>'scope_id' = $2::text) as dropped
             from policy_version v
             where exists (select 1 from jsonb_array_elements(v.snapshot) e where e->>'scope' = 'node' and e->>'scope_id' = $1::text))
         update policy_version v set snapshot = (
             select coalesce(jsonb_agg(case when e->>'scope' = 'node' and e->>'scope_id' = $1::text then jsonb_set(e, '{scope_id}', to_jsonb($2::text)) else e end
                                       order by e->>'scope' = 'node', case when e->>'scope' = 'node' and e->>'scope_id' = $1::text then $2::text else e->>'scope_id' end),
                             '[]'::jsonb)
             from jsonb_array_elements(v.snapshot) e
             where not (e->>'scope' = 'node' and e->>'scope_id' = $2::text))
         from hit where v.seq = hit.seq
         returning v.seq, hit.dropped",
    )
    .bind(from)
    .bind(to)
    .fetch_all(&mut *conn)
    .await?;
    moved.rewritten_seqs = hit.iter().map(|h| h.0).collect();
    moved.dropped_seqs = hit.iter().filter(|h| h.1).map(|h| h.0).collect();
    moved.rewritten_seqs.sort_unstable();
    moved.dropped_seqs.sort_unstable();
    Ok(moved)
}

/// `move_node` 做了什么（给作业日志、审计用）
#[derive(Default)]
pub struct NodeMoved {
    /// 搬走的当前节点级策略；旧节点当前没有的为 None（此时没有策略行被搬，不记 replace 版本）
    pub summary: Option<String>,
    /// 新节点原来自己有的节点级策略（被覆盖）
    pub replaced: Option<String>,
    /// 快照被改写的历史版本号
    pub rewritten_seqs: Vec<i64>,
    /// 其中丢掉了新节点原有项的版本号
    pub dropped_seqs: Vec<i64>,
}

/// 生效值视图
pub fn set_current(rows: &[Row]) -> PolicySet { build_set(rows, false) }
/// 草稿覆盖后的视图（没有草稿的范围用生效值）
pub fn set_with_drafts(rows: &[Row]) -> PolicySet { build_set(rows, true) }

fn build_set(rows: &[Row], drafts: bool) -> PolicySet {
    let mut s = PolicySet::default();
    for r in rows {
        let d = if drafts { r.draft.as_ref().unwrap_or(&r.cur) } else { &r.cur };
        match r.scope {
            Scope::Team => s.team.insert(r.scope_id, d.clone()),
            Scope::Node => s.node.insert(r.scope_id, d.clone()),
        };
    }
    s
}

pub async fn load_current(db: &PgPool) -> sqlx::Result<PolicySet> { Ok(set_current(&load_rows(db, false).await?)) }

/// 只有各范围生效的 mode（域名、备注为空，不读草稿）：实时视图每 3 秒重建一次，只用它判断节点是否「暂停新连接」
pub async fn load_modes(db: &PgPool) -> sqlx::Result<PolicySet> {
    let rows: Vec<(String, Uuid, String)> = sqlx::query_as("select scope, scope_id, mode from policy").fetch_all(db).await?;
    let mut s = PolicySet::default();
    for (scope, id, mode) in rows {
        let (Some(scope), Some(mode)) = (Scope::parse(&scope), Mode::parse(&mode)) else { continue };
        let d = Data { mode, ..Data::default() };
        match scope {
            Scope::Team => s.team.insert(id, d),
            Scope::Node => s.node.insert(id, d),
        };
    }
    Ok(s)
}

/// 当前生效版本号（最新 policy_version.seq；还没有版本为 0）
pub async fn current_version<'e, E: sqlx::PgExecutor<'e>>(ex: E) -> sqlx::Result<i64> {
    sqlx::query_scalar("select coalesce(max(seq), 0)::bigint from policy_version").fetch_one(ex).await
}

/// 所有草稿的规范化 JSON 加当前生效版本号的 sha256（十六进制）。没有草稿 → None。
/// 预览返回它，发布/丢弃必须带上且一致：防止预览之后草稿又被改，或期间有人发布 / 回滚。
pub fn draft_hash(rows: &[Row], version: i64) -> Option<String> {
    let mut items: Vec<(&'static str, Uuid, &Data)> = rows.iter().filter_map(|r| r.draft.as_ref().map(|d| (r.scope.as_str(), r.scope_id, d))).collect();
    if items.is_empty() {
        return None;
    }
    items.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    let v: Vec<serde_json::Value> = items
        .into_iter()
        .map(|(s, id, d)| serde_json::json!({"scope": s, "scope_id": id, "mode": d.mode.as_str(), "domains": d.domains, "note": d.note}))
        .collect();
    let v = serde_json::json!({"version": version, "drafts": v});
    Some(hex::encode(sha256(serde_json::to_string(&v).unwrap_or_default().as_bytes())))
}

/// 版本快照：所有范围的生效值，按 (scope, id) 排序。与默认值相同的范围不入快照。
pub fn snapshot_of(rows: &[Row]) -> Vec<serde_json::Value> {
    let mut m: BTreeMap<(Scope, Uuid), &Data> = BTreeMap::new();
    for r in rows {
        if r.cur != r.scope.default_data() {
            m.insert((r.scope, r.scope_id), &r.cur);
        }
    }
    m.into_iter()
        .map(|((s, id), d)| serde_json::json!({"scope": s.as_str(), "scope_id": id, "mode": d.mode.as_str(), "domains": d.domains, "note": d.note}))
        .collect()
}

/// 快照 JSON → (scope, id) → 内容（规范化，见 `Data::normalized`）。无法解析的项、规范化后是默认值的项跳过。
pub fn parse_snapshot(v: &serde_json::Value) -> BTreeMap<(Scope, Uuid), Data> {
    let mut m = BTreeMap::new();
    for it in v.as_array().into_iter().flatten() {
        let (Some(scope), Some(id), Some(mode)) = (
            it["scope"].as_str().and_then(Scope::parse),
            it["scope_id"].as_str().and_then(|s| s.parse::<Uuid>().ok()),
            it["mode"].as_str().and_then(Mode::parse),
        ) else {
            continue;
        };
        let domains = it["domains"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
        let note = it["note"].as_str().unwrap_or("").to_string();
        let d = Data { mode, domains, note }.normalized();
        if d != scope.default_data() {
            m.insert((scope, id), d);
        }
    }
    m
}

// ── 节点与名字 ──────────────────────────────────────────────────────────────

/// 带入口的在用节点：策略的作用对象
#[derive(Clone, Debug)]
pub struct NodeRef {
    pub id: Uuid,
    pub team_id: Option<Uuid>,
    pub team_name: Option<String>,
    pub name: String,
    /// 该节点入口所在的线路机（在用）
    pub relays: Vec<Uuid>,
}

pub async fn live_nodes<'e, E: sqlx::PgExecutor<'e>>(ex: E) -> sqlx::Result<Vec<NodeRef>> {
    #[derive(sqlx::FromRow)]
    struct NodeRow {
        id: Uuid,
        kind: String,
        team_id: Option<Uuid>,
        team: Option<String>,
        number: Option<i32>,
        relays: Vec<Uuid>,
    }
    let rows: Vec<NodeRow> = sqlx::query_as(
        "select m.id, m.kind::text kind, m.team_id, t.name team, m.number, array_agg(distinct e.relay_id) relays
         from entry e join machine m on m.id = e.node_id left join team t on t.id = m.team_id
         join machine r on r.id = e.relay_id and r.lifecycle = 'active'
         where e.lifecycle in ('pending', 'active', 'disabled') and m.lifecycle <> 'retired'
         group by m.id, m.kind, m.team_id, t.name, m.number
         order by t.name nulls last, m.number",
    )
    .fetch_all(ex)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| NodeRef {
            name: display_name(&r.kind, None, r.team.as_deref(), r.number),
            id: r.id,
            team_id: r.team_id,
            team_name: r.team,
            relays: r.relays,
        })
        .collect())
}

/// 两套策略视图之间，哪些线路机的期望状态会变（有效策略编译结果不同的节点所在的线路机）。已排序去重。
pub fn affected_relays(nodes: &[NodeRef], before: &PolicySet, after: &PolicySet) -> Vec<Uuid> {
    let mut out: Vec<Uuid> = vec![];
    for n in nodes {
        let a = compile(&before.effective(n.id, n.team_id).0);
        let b = compile(&after.effective(n.id, n.team_id).0);
        if a != b {
            out.extend(n.relays.iter().copied());
        }
    }
    out.sort();
    out.dedup();
    out
}

/// 团队与节点的显示名（变更摘要用）
pub async fn scope_names(db: &PgPool) -> sqlx::Result<HashMap<Uuid, String>> { scope_names_in(&mut *db.acquire().await?).await }

/// 同 `scope_names`，在调用方的连接（事务）里读
pub async fn scope_names_in(db: &mut sqlx::PgConnection) -> sqlx::Result<HashMap<Uuid, String>> {
    let mut m: HashMap<Uuid, String> = sqlx::query_as::<_, (Uuid, String)>("select id, name from team").fetch_all(&mut *db).await?.into_iter().collect();
    let nodes: Vec<(Uuid, String, Option<String>, Option<i32>)> = sqlx::query_as(
        "select m.id, m.kind::text, t.name, m.number from machine m left join team t on t.id = m.team_id where m.kind <> 'relay'",
    )
    .fetch_all(&mut *db)
    .await?;
    for (id, kind, team, number) in nodes {
        m.insert(id, display_name(&kind, None, team.as_deref(), number));
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(mode: Mode, domains: &[&str]) -> Data { Data { mode, domains: domains.iter().map(|s| s.to_string()).collect(), note: String::new() } }

    #[test]
    fn normalizes_separators_case_dot_exact_and_dedupes() {
        let got = normalize_domains("  Example.COM.,b.example.org，=Exact.net\n\tc.io  example.com ").unwrap();
        assert_eq!(got, vec!["=exact.net", "b.example.org", "c.io", "example.com"]);
        assert_eq!(normalize_domains("").unwrap(), Vec::<String>::new());
        assert_eq!(normalize_list(&["a.com, b.com", "A.com."]).unwrap(), vec!["a.com", "b.com"]);
        // xn-- 国际化域名可以
        assert_eq!(normalize_domains("xn--fiqs8s.cn").unwrap(), vec!["xn--fiqs8s.cn"]);
    }

    #[test]
    fn rejects_bad_domains_with_chinese_messages_naming_the_domain() {
        let cases: &[(&str, &str)] = &[
            ("中国.cn", "非 ASCII"),
            ("http://a.com", "网址"),
            ("a.com/path", "网址"),
            ("a.com?x=1", "网址"),
            ("user@a.com", "网址"),
            ("1.2.3.4", "IP 地址"),
            ("2001:db8::1", "IP 地址"),
            ("=8.8.8.8", "IP 地址"),
            ("a.com:443", "端口"),
            ("*.a.com", "通配符"),
            ("regexp:.*", "前缀"),
            ("domain:a.com", "前缀"),
            ("a(b).com", "正则"),
            ("localhost", "必须包含点"),
            ("a..com", "空的标签"),
            (".a.com", "空的标签"),
            ("-a.com", "连字符"),
            ("a-.com", "连字符"),
            ("a_b.com", "字母、数字和连字符"),
            ("=", "后面没有域名"),
        ];
        for (input, want) in cases {
            let e = normalize_domains(input).unwrap_err();
            assert!(e.contains(want), "{input} -> {e}");
            assert!(e.contains(&format!("「{input}")) || e.contains("「"), "{e}");
        }
        // 出错信息要点名具体是哪一个
        let e = normalize_domains("good.com, bad_one.com, other.com").unwrap_err();
        assert!(e.contains("bad_one.com"), "{e}");
        // 标签 / 总长 / 数量上限
        let long_label = format!("{}.com", "a".repeat(64));
        assert!(normalize_domains(&long_label).unwrap_err().contains("63"));
        assert!(normalize_domains(&format!("{}.com", "a".repeat(63))).is_ok());
        let long_total = format!("{}.com", vec!["a".repeat(60); 5].join("."));
        assert!(normalize_domains(&long_total).unwrap_err().contains("253"));
        let many: Vec<String> = (0..301).map(|i| format!("h{i}.example.com")).collect();
        assert!(normalize_list(&many).unwrap_err().contains("300"));
        assert!(normalize_list(&many[..300]).is_ok());
    }

    #[test]
    fn draft_validation() {
        assert!(validate_draft(Scope::Team, "allowlist", vec![], "").unwrap_err().contains("必须填写域名"));
        assert!(validate_draft(Scope::Node, "denylist", vec![], "").is_err());
        assert!(validate_draft(Scope::Team, "inherit", vec![], "").unwrap_err().contains("inherit"));
        assert!(validate_draft(Scope::Node, "inherit", vec![], "").is_ok());
        assert!(validate_draft(Scope::Team, "blocked", vec![], "").is_ok());
        assert!(validate_draft(Scope::Team, "observe", vec![], &"x".repeat(501)).unwrap_err().contains("500"));
        assert!(validate_draft(Scope::Team, "pause", vec![], "").is_err());
        let ok = validate_draft(Scope::Team, "allowlist", vec!["a.com".into()], "  备注 ").unwrap();
        assert_eq!(ok.note, "备注");
    }

    #[test]
    fn effective_inheritance() {
        let (team, other, node, plain) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3), Uuid::from_u128(4));
        let mut s = PolicySet::default();
        s.team.insert(team, d(Mode::Denylist, &["bad.com"]));
        // 没有节点策略：用团队
        let (e, src) = s.effective(plain, Some(team));
        assert_eq!((e.mode, src), (Mode::Denylist, Source::Team));
        // 节点 inherit：仍用团队
        s.node.insert(node, d(Mode::Inherit, &[]));
        assert_eq!(s.effective(node, Some(team)).1, Source::Team);
        // 节点自己的模式优先（哪怕是 observe：显式不受团队约束）
        s.node.insert(node, d(Mode::Observe, &[]));
        let (e, src) = s.effective(node, Some(team));
        assert_eq!((e.mode, src), (Mode::Observe, Source::Node));
        s.node.insert(node, d(Mode::Allowlist, &["ok.com"]));
        assert_eq!(s.effective(node, Some(team)).0.domains, vec!["ok.com"]);
        // 团队没有策略：默认 observe
        let (e, src) = s.effective(plain, Some(other));
        assert_eq!((e.mode, src), (Mode::Observe, Source::Default));
        assert_eq!(s.effective(plain, None).1, Source::Default);
    }

    #[test]
    fn matching_and_would_block() {
        let allow = d(Mode::Allowlist, &["example.com", "=exact.org"]);
        assert!(!would_block(&allow, "example.com"));
        assert!(!would_block(&allow, "a.b.example.com"));
        assert!(would_block(&allow, "notexample.com"));
        assert!(would_block(&allow, "example.com.evil.io"));
        assert!(!would_block(&allow, "exact.org"));
        assert!(would_block(&allow, "www.exact.org"));
        assert!(would_block(&allow, "1.2.3.4"));
        let deny = d(Mode::Denylist, &["bad.com"]);
        assert!(would_block(&deny, "x.bad.com"));
        assert!(!would_block(&deny, "good.com"));
        assert!(!would_block(&deny, "1.2.3.4"));
        assert!(would_block(&d(Mode::Blocked, &[]), "anything.com"));
        assert!(!would_block(&d(Mode::Observe, &["a.com"]), "a.com"));
        // 合并的随机子域名 *.reg.com：整棵子树在 domain 规则里才算命中
        assert!(would_block(&deny, "*.bad.com"));
        assert!(!would_block(&allow, "*.example.com"));
        assert!(would_block(&allow, "*.exact.org"));
    }

    #[test]
    fn compiles_to_xray_matchers() {
        assert_eq!(compile(&d(Mode::Observe, &["a.com"])), None);
        assert_eq!(compile(&d(Mode::Inherit, &[])), None);
        let c = compile(&d(Mode::Allowlist, &["a.com", "=b.com"])).unwrap();
        assert_eq!((c.mode, c.domains), (PolicyMode::Allowlist, vec!["domain:a.com".to_string(), "full:b.com".to_string()]));
        let c = compile(&d(Mode::Blocked, &["ignored.com"])).unwrap();
        assert_eq!((c.mode, c.domains.len()), (PolicyMode::Blocked, 0));
    }

    fn row(scope: Scope, id: u128, cur: Data, draft: Option<Data>) -> Row {
        Row { scope, scope_id: Uuid::from_u128(id), cur, draft, revision: 1, updated_by: None, updated_at: OffsetDateTime::UNIX_EPOCH }
    }

    #[test]
    fn draft_hash_is_stable_and_sensitive() {
        let a = row(Scope::Team, 1, Data::default(), Some(d(Mode::Blocked, &[])));
        let b = row(Scope::Node, 2, Scope::Node.default_data(), Some(d(Mode::Denylist, &["x.com"])));
        assert_eq!(draft_hash(&[], 1), None);
        assert_eq!(draft_hash(&[row(Scope::Team, 1, Data::default(), None)], 1), None);
        let h1 = draft_hash(&[a.clone(), b.clone()], 1).unwrap();
        assert_eq!(h1.len(), 64);
        assert_eq!(draft_hash(&[b.clone(), a.clone()], 1).unwrap(), h1, "与行的顺序无关");
        assert_ne!(draft_hash(&[a.clone(), b.clone()], 2).unwrap(), h1, "生效版本变了哈希也变");
        let mut b2 = b.clone();
        b2.draft = Some(d(Mode::Denylist, &["y.com"]));
        assert_ne!(draft_hash(&[a, b2], 1).unwrap(), h1);
    }

    #[test]
    fn snapshot_roundtrip_skips_defaults() {
        let rows = vec![
            row(Scope::Team, 1, d(Mode::Allowlist, &["a.com"]), None),
            row(Scope::Team, 2, Data::default(), None),
            row(Scope::Node, 3, Scope::Node.default_data(), None),
            row(Scope::Node, 4, d(Mode::Blocked, &[]), None),
        ];
        let snap = serde_json::Value::Array(snapshot_of(&rows));
        let m = parse_snapshot(&snap);
        assert_eq!(m.len(), 2);
        assert_eq!(m[&(Scope::Team, Uuid::from_u128(1))].domains, vec!["a.com"]);
        assert_eq!(m[&(Scope::Node, Uuid::from_u128(4))].mode, Mode::Blocked);
    }

    #[test]
    fn affected_relays_only_where_compiled_policy_changes() {
        let (team, n1, n2) = (Uuid::from_u128(1), Uuid::from_u128(11), Uuid::from_u128(12));
        let (r1, r2) = (Uuid::from_u128(101), Uuid::from_u128(102));
        let nodes = vec![
            NodeRef { id: n1, team_id: Some(team), team_name: None, name: "n1".into(), relays: vec![r1] },
            NodeRef { id: n2, team_id: Some(team), team_name: None, name: "n2".into(), relays: vec![r2] },
        ];
        let before = PolicySet::default();
        let mut after = PolicySet::default();
        after.team.insert(team, d(Mode::Denylist, &["a.com"]));
        assert_eq!(affected_relays(&nodes, &before, &after), vec![r1, r2]);
        // n2 自己选了 observe：团队策略对它没有影响，r2 不受影响
        after.node.insert(n2, d(Mode::Observe, &[]));
        assert_eq!(affected_relays(&nodes, &before, &after), vec![r1]);
        // 只改备注：不影响线路机
        let mut after2 = after.clone();
        after2.team.insert(team, Data { note: "x".into(), ..d(Mode::Denylist, &["a.com"]) });
        assert!(affected_relays(&nodes, &after, &after2).is_empty());
    }
}
