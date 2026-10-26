//! 访问策略接口：草稿（乐观锁）、预览、发布、丢弃、版本与回滚。领域逻辑在 `crate::policy`。
//!
//! 编辑只改草稿；发布把所有草稿一次性生效成新版本（存完整快照），并对受影响的线路机代数 +1，
//! 由 agent 渲染 Xray 路由规则并热更新。版本状态由各线路机的 applied_generation 推导。

use super::ok;
use crate::auth::Actor;
use crate::db::{audit, bump_relay, new_id, Tx};
use crate::error::{ApiError, ApiJson, ApiPath, ApiResult};
use crate::policy::{self, Data, Mode, PolicySet, Row, Scope, Source};
use crate::state::{AppState, Event};
use axum::extract::State;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use time::OffsetDateTime;
use uuid::Uuid;

/// 节点的基本信息：(类型, 生命周期, 线路机名, 团队名, 编号)
type NodeRow = (String, String, Option<String>, Option<String>, Option<i32>);

/// 预览看最近多少小时的访问汇总
const PREVIEW_HOURS: i64 = 24;
const PREVIEW_DOMAINS: usize = 20;
const VERSIONS_SHOWN: i64 = 30;

fn data_json(d: &Data) -> Value { json!({"mode": d.mode.as_str(), "domains": d.domains, "note": d.note}) }

/// 一个范围的当前值 + 草稿 + 版本号；还没有记录的范围给默认值、revision 0
fn policy_json(scope: Scope, id: Uuid, row: Option<&Row>) -> Value {
    match row {
        Some(r) => json!({
            "scope": scope.as_str(), "id": id,
            "mode": r.cur.mode.as_str(), "domains": r.cur.domains, "note": r.cur.note,
            "draft": r.draft.as_ref().map(data_json),
            "revision": r.revision, "updated_by": r.updated_by, "updated_at": crate::meta::fmt_ts(r.updated_at),
        }),
        None => {
            let d = scope.default_data();
            json!({
                "scope": scope.as_str(), "id": id,
                "mode": d.mode.as_str(), "domains": d.domains, "note": d.note,
                "draft": null, "revision": 0, "updated_by": null, "updated_at": null,
            })
        }
    }
}

fn effective_json(set: &PolicySet, node: Uuid, team: Option<Uuid>) -> Value {
    let (d, src) = set.effective(node, team);
    let src = match src {
        Source::Node => "node",
        Source::Team => "team",
        Source::Default => "default",
    };
    json!({"mode": d.mode.as_str(), "domains": d.domains, "note": d.note, "source": src})
}

fn index(rows: &[Row]) -> HashMap<(Scope, Uuid), &Row> { rows.iter().map(|r| ((r.scope, r.scope_id), r)).collect() }

/// 所有团队 / 节点的生效值 + 草稿 + revision
pub async fn list(_a: Actor, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let rows = policy::load_rows(&st.db, false).await?;
    let idx = index(&rows);
    let cur = policy::set_current(&rows);
    let with_drafts = policy::set_with_drafts(&rows);
    let teams: Vec<(Uuid, String)> = sqlx::query_as("select id, name from team order by sort_order, name").fetch_all(&st.db).await?;
    #[derive(sqlx::FromRow)]
    struct N {
        id: Uuid,
        team_id: Option<Uuid>,
        kind: String,
        number: Option<i32>,
        has_entry: bool,
    }
    let nodes: Vec<N> = sqlx::query_as(
        "select m.id, m.team_id, m.kind::text kind, m.number,
                exists (select 1 from entry e where e.node_id = m.id and e.lifecycle in ('pending', 'active', 'disabled')) has_entry
         from machine m where m.kind <> 'relay' and m.lifecycle <> 'retired' order by m.number",
    )
    .fetch_all(&st.db)
    .await?;
    let out: Vec<Value> = teams
        .iter()
        .map(|(tid, tname)| {
            let ns: Vec<Value> = nodes
                .iter()
                .filter(|n| n.team_id == Some(*tid))
                .map(|n| {
                    json!({
                        "id": n.id,
                        "name": crate::names::display_name(&n.kind, None, Some(tname), n.number),
                        "number": n.number,
                        "has_entry": n.has_entry,
                        "policy": policy_json(Scope::Node, n.id, idx.get(&(Scope::Node, n.id)).copied()),
                        "effective": effective_json(&cur, n.id, n.team_id),
                        "draft_effective": effective_json(&with_drafts, n.id, n.team_id),
                    })
                })
                .collect();
            json!({"id": tid, "name": tname, "policy": policy_json(Scope::Team, *tid, idx.get(&(Scope::Team, *tid)).copied()), "nodes": ns})
        })
        .collect();
    let draft_count = rows.iter().filter(|r| r.draft.is_some()).count();
    let ver = policy::current_version(&st.db).await?;
    Ok(ok(json!({"draft_hash": policy::draft_hash(&rows, ver), "draft_count": draft_count, "teams": out})))
}

// ── 改草稿 ─────────────────────────────────────────────────────────────────

/// `domains` 既可以是数组也可以是一整段文本
#[derive(Deserialize)]
#[serde(untagged)]
enum DomainsIn {
    List(Vec<String>),
    Text(String),
}

impl Default for DomainsIn {
    fn default() -> Self { DomainsIn::List(vec![]) }
}

#[derive(Deserialize)]
pub struct PutIn {
    mode: String,
    #[serde(default)]
    domains: DomainsIn,
    #[serde(default)]
    note: String,
    expected_revision: i32,
}

pub async fn put_one(a: Actor, State(st): State<AppState>, ApiPath((scope, id)): ApiPath<(String, Uuid)>, ApiJson(b): ApiJson<PutIn>) -> ApiResult<Json<Value>> {
    let scope = Scope::parse(&scope).ok_or_else(|| ApiError::bad_request("范围只能是 team 或 node"))?;
    // 范围的显示名（团队名 / 节点名）写进审计：「团队1 · 黑名单 · 2 个域名」
    let name = match scope {
        Scope::Team => sqlx::query_scalar::<_, String>("select name from team where id = $1")
            .bind(id)
            .fetch_optional(&st.db)
            .await?
            .ok_or_else(|| ApiError::not_found("没有这个团队"))?,
        Scope::Node => {
            let m: Option<NodeRow> =
                sqlx::query_as("select m.kind::text, m.lifecycle::text, m.label, t.name, m.number from machine m left join team t on t.id = m.team_id where m.id = $1")
                    .bind(id)
                    .fetch_optional(&st.db)
                    .await?;
            match m {
                None => return Err(ApiError::not_found("没有这台节点")),
                Some((k, ..)) if k == "relay" => return Err(ApiError::bad_request("线路机没有访问策略，请设置它上面节点的策略")),
                Some((_, lc, ..)) if lc == "retired" => return Err(ApiError::conflict("节点已退役")),
                Some((k, _, label, team, number)) => crate::names::display_name(&k, label.as_deref(), team.as_deref(), number),
            }
        }
    };
    let domains = match &b.domains {
        DomainsIn::List(l) => policy::normalize_list(l),
        DomainsIn::Text(t) => policy::normalize_domains(t),
    }
    .map_err(ApiError::bad_request)?;
    let data = policy::validate_draft(scope, &b.mode, domains, &b.note).map_err(ApiError::bad_request)?;

    let mut tx = st.db.begin().await?;
    lock(&mut tx).await?;
    // 等锁的时候团队可能刚被删掉（删团队拿同一把锁）：不能给它留下一行草稿
    if scope == Scope::Team && sqlx::query_scalar::<_, i32>("select 1 from team where id = $1").bind(id).fetch_optional(&mut *tx).await?.is_none() {
        return Err(ApiError::not_found("没有这个团队"));
    }
    let cur: Option<(i32, String, Vec<String>, String)> =
        sqlx::query_as("select revision, mode, domains, note from policy where scope = $1 and scope_id = $2 for update")
            .bind(scope.as_str())
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let stale = || ApiError::conflict("草稿已被修改，请重新加载");
    let draft = match cur {
        None => {
            if b.expected_revision != 0 {
                return Err(stale());
            }
            let d = scope.default_data();
            // 草稿与生效值相同 = 没有改动
            let draft = (data != d).then(|| data_json(&data));
            sqlx::query("insert into policy (scope, scope_id, mode, domains, note, draft, updated_by) values ($1, $2, $3, $4, $5, $6, $7)")
                .bind(scope.as_str())
                .bind(id)
                .bind(d.mode.as_str())
                .bind(&d.domains)
                .bind(&d.note)
                .bind(&draft)
                .bind(&a.name)
                .execute(&mut *tx)
                .await?;
            draft
        }
        Some((rev, mode, domains, note)) => {
            if rev != b.expected_revision {
                return Err(stale());
            }
            let cur = Data { mode: Mode::parse(&mode).unwrap_or_default(), domains, note }.normalized();
            let draft = (data != cur).then(|| data_json(&data));
            sqlx::query("update policy set draft = $3, revision = revision + 1, updated_by = $4, updated_at = now() where scope = $1 and scope_id = $2")
                .bind(scope.as_str())
                .bind(id)
                .bind(&draft)
                .bind(&a.name)
                .execute(&mut *tx)
                .await?;
            draft
        }
    };
    audit(
        &mut tx,
        &a.name,
        "policy.draft",
        scope.as_str(),
        Some(id),
        json!({"name": name, "mode": data.mode.as_str(), "domains": data.domains.len(), "note": data.note, "cleared": draft.is_none()}),
    )
    .await?;
    tx.commit().await?;
    st.emit(Event::Policies);
    let rows = policy::load_rows(&st.db, false).await?;
    let one = rows.iter().find(|r| r.scope == scope && r.scope_id == id);
    let ver = policy::current_version(&st.db).await?;
    Ok(ok(json!({"policy": policy_json(scope, id, one), "draft_hash": policy::draft_hash(&rows, ver), "draft_count": rows.iter().filter(|r| r.draft.is_some()).count()})))
}

/// 访问规则的锁（见 policy::LOCK_KEY）；删团队也拿它
pub(crate) async fn lock(tx: &mut Tx<'_>) -> ApiResult<()> {
    sqlx::query("select pg_advisory_xact_lock($1)").bind(policy::LOCK_KEY).execute(&mut **tx).await?;
    Ok(())
}

// ── 预览 ───────────────────────────────────────────────────────────────────

fn mode_domains(d: &Data) -> Value { json!({"mode": d.mode.as_str(), "domains": d.domains.len()}) }

/// 草稿相对生效值的变更摘要（给人看）：`from` / `to` 只有模式与域名个数，另写域名、备注各改没改（同模式同个数时分得清是换了域名还是只改了备注）
fn diff_json(before: &BTreeMap<(Scope, Uuid), Data>, after: &BTreeMap<(Scope, Uuid), Data>, names: &HashMap<Uuid, String>) -> Vec<Value> {
    let mut keys: Vec<(Scope, Uuid)> = before.keys().chain(after.keys()).copied().collect();
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .filter_map(|(s, id)| {
            let a = before.get(&(s, id)).cloned().unwrap_or_else(|| s.default_data());
            let b = after.get(&(s, id)).cloned().unwrap_or_else(|| s.default_data());
            (a != b).then(|| {
                json!({
                    "scope": s.as_str(), "id": id, "name": names.get(&id).cloned().unwrap_or_default(),
                    "from": mode_domains(&a), "to": mode_domains(&b),
                    "domains_changed": a.domains != b.domains, "note_changed": a.note != b.note,
                })
            })
        })
        .collect()
}

fn map_of(rows: &[Row], drafts: bool) -> BTreeMap<(Scope, Uuid), Data> {
    rows.iter().map(|r| ((r.scope, r.scope_id), if drafts { r.draft.clone().unwrap_or_else(|| r.cur.clone()) } else { r.cur.clone() })).collect()
}

/// 模拟：把所有草稿生效后，最近 24 小时每个节点会被阻止多少连接、哪些域名
pub async fn preview(_a: Actor, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let rows = policy::load_rows(&st.db, false).await?;
    let hash = policy::draft_hash(&rows, policy::current_version(&st.db).await?);
    let cur = policy::set_current(&rows);
    let new = policy::set_with_drafts(&rows);
    let nodes = policy::live_nodes(&st.db).await?;
    let names = policy::scope_names(&st.db).await?;
    let relays = policy::affected_relays(&nodes, &cur, &new);

    // 只模拟「草稿生效后不是 observe」或「有效策略有变化」的节点
    let picked: Vec<(&policy::NodeRef, Data, Source, bool)> = nodes
        .iter()
        .filter_map(|n| {
            let (d, src) = new.effective(n.id, n.team_id);
            let changed = policy::compile(&cur.effective(n.id, n.team_id).0) != policy::compile(&d);
            (d.mode != Mode::Observe || changed).then_some((n, d, src, changed))
        })
        .collect();
    let ids: Vec<Uuid> = picked.iter().map(|p| p.0.id).collect();
    let hosts: Vec<(Uuid, String, i64, i64)> = if ids.is_empty() {
        vec![]
    } else {
        sqlx::query_as(
            "select node_id, dest_host, sum(conns)::bigint, sum(blocked)::bigint from access_hourly
             where node_id = any($1) and hour > now() - make_interval(hours => $2) group by node_id, dest_host",
        )
        .bind(&ids)
        .bind(PREVIEW_HOURS as i32)
        .fetch_all(&st.db)
        .await?
    };
    let mut by_node: HashMap<Uuid, Vec<(String, i64, i64)>> = HashMap::new();
    for (n, h, c, b) in hosts {
        by_node.entry(n).or_default().push((h, c, b));
    }
    let mut out: Vec<Value> = vec![];
    for (n, d, src, changed) in &picked {
        let (mut total, mut would, mut cur_blocked) = (0i64, 0i64, 0i64);
        let mut blocked_hosts: Vec<(String, i64)> = vec![];
        for (h, c, b) in by_node.get(&n.id).into_iter().flatten() {
            total += c;
            cur_blocked += b;
            if policy::would_block(d, h) {
                would += c;
                blocked_hosts.push((h.clone(), *c));
            }
        }
        blocked_hosts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        blocked_hosts.truncate(PREVIEW_DOMAINS);
        out.push(json!({
            "node_id": n.id, "name": n.name, "team": n.team_name,
            "mode": d.mode.as_str(), "source": match src { Source::Node => "node", Source::Team => "team", Source::Default => "default" },
            "changed": changed, "total_conns": total, "would_block": would, "current_blocked": cur_blocked,
            "domains": blocked_hosts.into_iter().map(|(h, c)| json!({"host": h, "conns": c})).collect::<Vec<_>>(),
        }));
    }
    out.sort_by(|a, b| {
        b["would_block"].as_i64().cmp(&a["would_block"].as_i64()).then_with(|| a["name"].as_str().cmp(&b["name"].as_str()))
    });
    let changes = diff_json(&map_of(&rows, false), &map_of(&rows, true), &names);
    Ok(ok(json!({
        "draft_hash": hash, "draft_count": rows.iter().filter(|r| r.draft.is_some()).count(),
        "window_hours": PREVIEW_HOURS, "affected_relays": relays.len(), "changes": changes, "nodes": out,
    })))
}

// ── 发布 / 丢弃 ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct HashIn {
    draft_hash: String,
}

/// 草稿存在且哈希一致（草稿与生效版本都没变），否则 409。在锁内调用。
async fn check_hash(tx: &mut Tx<'_>, rows: &[Row], given: &str) -> ApiResult<()> {
    let ver = policy::current_version(&mut **tx).await?;
    let Some(h) = policy::draft_hash(rows, ver) else { return Err(ApiError::conflict("没有未发布的草稿")) };
    if h != given.trim() {
        return Err(ApiError::conflict("草稿或生效版本已变化，请重新预览"));
    }
    Ok(())
}

pub async fn publish(a: Actor, State(st): State<AppState>, ApiJson(b): ApiJson<HashIn>) -> ApiResult<Json<Value>> {
    let names = policy::scope_names(&st.db).await?;
    let mut tx = st.db.begin().await?;
    lock(&mut tx).await?;
    // 在锁内读在用节点：新建入口拿同一把锁的共享锁，读到的就是发布时真实挂着的入口
    let nodes = policy::live_nodes(&mut *tx).await?;
    let rows = policy::load_rows(&mut *tx, true).await?;
    check_hash(&mut tx, &rows, &b.draft_hash).await?;
    let before = policy::set_current(&rows);
    let after = policy::set_with_drafts(&rows);
    for r in rows.iter().filter(|r| r.draft.is_some()) {
        let d = r.draft.as_ref().unwrap_or(&r.cur);
        sqlx::query(
            "update policy set mode = $3, domains = $4, note = $5, draft = null, revision = revision + 1, updated_by = $6, updated_at = now()
             where scope = $1 and scope_id = $2",
        )
        .bind(r.scope.as_str())
        .bind(r.scope_id)
        .bind(d.mode.as_str())
        .bind(&d.domains)
        .bind(&d.note)
        .bind(&a.name)
        .execute(&mut *tx)
        .await?;
    }
    let (vid, relays) = record_version(&mut tx, &a, "publish", None, &rows, &before, &after, &nodes, &names).await?;
    tx.commit().await?;
    done(&st, &relays);
    Ok(ok(version_one(&st, vid).await?))
}

pub async fn discard(a: Actor, State(st): State<AppState>, ApiJson(b): ApiJson<HashIn>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    lock(&mut tx).await?;
    let rows = policy::load_rows(&mut *tx, true).await?;
    check_hash(&mut tx, &rows, &b.draft_hash).await?;
    // 只清读出来算有草稿的行（与预览、发布同一个口径）：规范化后与生效值相同的草稿本来就当作没有，不算丢弃、revision 不动
    let (scopes, ids): (Vec<&str>, Vec<Uuid>) = rows.iter().filter(|r| r.draft.is_some()).map(|r| (r.scope.as_str(), r.scope_id)).unzip();
    let n = sqlx::query(
        "update policy p set draft = null, revision = revision + 1, updated_by = $1, updated_at = now()
         from unnest($2::text[], $3::uuid[]) k(scope, id) where p.scope = k.scope and p.scope_id = k.id",
    )
    .bind(&a.name)
    .bind(&scopes)
    .bind(&ids)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    audit(&mut tx, &a.name, "policy.discard", "policy", None, json!({"drafts": n})).await?;
    tx.commit().await?;
    st.emit(Event::Policies);
    Ok(ok(json!({"discarded": n})))
}

/// 策略行已改完之后：算受影响的线路机并 +1、写版本快照与审计。返回 (版本 id, 被 +1 的线路机)。
#[allow(clippy::too_many_arguments)]
async fn record_version(
    tx: &mut Tx<'_>,
    a: &Actor,
    kind: &str,
    rollback_of: Option<Uuid>,
    rows_before: &[Row],
    before: &PolicySet,
    after: &PolicySet,
    nodes: &[policy::NodeRef],
    names: &HashMap<Uuid, String>,
) -> ApiResult<(Uuid, Vec<Uuid>)> {
    let rows_after = policy::load_rows(&mut **tx, false).await?;
    let snapshot = Value::Array(policy::snapshot_of(&rows_after));
    let changes = diff_json(&map_of(rows_before, false), &map_of(&rows_after, false), names);
    let relays = policy::affected_relays(nodes, before, after);
    let mut gens: Vec<Value> = vec![];
    for r in &relays {
        gens.push(json!({"relay_id": r, "generation": bump_relay(tx, *r).await?}));
    }
    let vid = new_id();
    let (seq, rollback_of_seq): (i64, Option<i64>) = sqlx::query_as(
        "insert into policy_version (id, kind, rollback_of, actor, snapshot, changes, relays) values ($1, $2, $3, $4, $5, $6, $7)
         returning seq, (select p.seq from policy_version p where p.id = $3)",
    )
    .bind(vid)
    .bind(kind)
    .bind(rollback_of)
    .bind(&a.name)
    .bind(&snapshot)
    .bind(Value::Array(changes.clone()))
    .bind(Value::Array(gens))
    .fetch_one(&mut **tx)
    .await?;
    audit(
        tx,
        &a.name,
        &format!("policy.{kind}"),
        "policy_version",
        Some(vid),
        json!({"seq": seq, "rollback_of_seq": rollback_of_seq, "changes": changes, "relays": relays.len()}),
    )
    .await?;
    Ok((vid, relays))
}

fn done(st: &AppState, relays: &[Uuid]) {
    for r in relays {
        st.relay_bumped(*r);
    }
    st.emit(Event::Policies);
}

// ── 版本与回滚 ──────────────────────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct VRow {
    id: Uuid,
    seq: i64,
    kind: String,
    rollback_of_seq: Option<i64>,
    actor: String,
    created_at: OffsetDateTime,
    /// 与当前生效值的快照逐字相同时为 None（不用取回、解析）
    snapshot: Option<Value>,
    changes: Value,
    relays: Value,
}

async fn load_versions(st: &AppState, only: Option<Uuid>) -> ApiResult<Vec<Value>> {
    // 「与当前相同」和回滚用同一个比法（规范化、去掉已删除的团队与节点，见 rollback_target）：点得出回滚按钮的就一定回滚得了。
    // 先在库里比快照是否与当前逐字相同（通常就是最新一版），相同的不取回；回滚会改什么另由 rollback_preview 按版本给
    let rows = policy::load_rows(&st.db, false).await?;
    let cur_snap = Value::Array(policy::snapshot_of(&rows));
    let current = policy::parse_snapshot(&cur_snap);
    let vs: Vec<VRow> = sqlx::query_as(
        "select v.id, v.seq, v.kind, p.seq rollback_of_seq, v.actor, v.created_at,
                case when v.snapshot = $3 then null else v.snapshot end snapshot, v.changes, v.relays
         from policy_version v left join policy_version p on p.id = v.rollback_of
         where ($1::uuid is null or v.id = $1) order by v.seq desc limit $2",
    )
    .bind(only)
    .bind(VERSIONS_SHOWN)
    .bind(&cur_snap)
    .fetch_all(&st.db)
    .await?;
    let exists = existing_scopes(&st.db).await?;
    // 与当前逐字相同的快照：去掉已删除的范围后通常仍相同（当前生效值里还留着已彻底删除的节点时不同）
    let same_as_cur = rollback_target(&cur_snap, &exists) == current;
    let latest: Option<i64> = sqlx::query_scalar("select max(seq) from policy_version").fetch_one(&st.db).await?;
    let latest = latest.unwrap_or(0);

    let relay_ids: Vec<Uuid> = vs
        .iter()
        .flat_map(|v| v.relays.as_array().into_iter().flatten().filter_map(|r| r["relay_id"].as_str().and_then(|s| s.parse().ok())))
        .collect();
    #[derive(sqlx::FromRow)]
    struct RelayRow {
        id: Uuid,
        name: Option<String>,
        lifecycle: String,
        applied: i64,
        error: Option<String>,
    }
    let infos: Vec<RelayRow> = sqlx::query_as(
        "select m.id, m.label name, m.lifecycle::text lifecycle, r.applied_generation applied, r.apply_error error
         from relay r join machine m on m.id = r.machine_id where r.machine_id = any($1)",
    )
    .bind(&relay_ids)
    .fetch_all(&st.db)
    .await?;
    let infos: HashMap<Uuid, RelayRow> = infos.into_iter().map(|r| (r.id, r)).collect();

    Ok(vs
        .into_iter()
        .map(|v| {
            let mut rel: Vec<Value> = vec![];
            let (mut failed, mut applying) = (false, false);
            for r in v.relays.as_array().into_iter().flatten() {
                let (Some(rid), Some(gen)) = (r["relay_id"].as_str().and_then(|s| s.parse::<Uuid>().ok()), r["generation"].as_i64()) else { continue };
                let (name, status, applied, error) = match infos.get(&rid) {
                    Some(i) if i.lifecycle == "active" => {
                        let status = if i.applied >= gen {
                            "applied"
                        } else if i.error.is_some() {
                            failed = true;
                            "failed"
                        } else {
                            applying = true;
                            "applying"
                        };
                        (i.name.clone(), status, Some(i.applied), i.error.clone().filter(|_| i.applied < gen))
                    }
                    // 线路机已经退役或被清除：不再等它
                    Some(i) => (i.name.clone(), "gone", Some(i.applied), None),
                    None => (None, "gone", None, None),
                };
                rel.push(json!({"relay_id": rid, "name": name, "generation": gen, "applied_generation": applied, "status": status, "error": error}));
            }
            let status = if failed {
                "failed"
            } else if applying {
                "applying"
            } else {
                "applied"
            };
            // 内容与当前相同的不能回滚
            let is_current = v.snapshot.as_ref().map_or(same_as_cur, |s| rollback_target(s, &exists) == current);
            json!({
                "id": v.id, "seq": v.seq, "kind": v.kind, "rollback_of_seq": v.rollback_of_seq, "actor": v.actor,
                "created_at": crate::meta::fmt_ts(v.created_at), "status": status, "changes": v.changes, "relays": rel,
                "is_latest": v.seq == latest, "is_current": is_current, "rollbackable": !is_current,
            })
        })
        .collect())
}

async fn version_one(st: &AppState, id: Uuid) -> ApiResult<Value> {
    load_versions(st, Some(id)).await?.into_iter().next().ok_or_else(|| ApiError::not_found("没有这个版本"))
}

/// 最近 30 个版本，含每台线路机的应用状态
pub async fn versions(_a: Actor, State(st): State<AppState>) -> ApiResult<Json<Value>> { Ok(ok(load_versions(&st, None).await?)) }

/// 现存的团队与节点（不含线路机）
async fn existing_scopes<'e, E: sqlx::PgExecutor<'e>>(ex: E) -> sqlx::Result<HashSet<Uuid>> {
    Ok(sqlx::query_scalar("select id from team union all select id from machine where kind <> 'relay'").fetch_all(ex).await?.into_iter().collect())
}

/// 回滚到这份快照后各范围的值：快照之后被删掉的团队、彻底删除的节点跳过（不给它们建出没有主人的规则行）
fn rollback_target(snap: &Value, exists: &HashSet<Uuid>) -> BTreeMap<(Scope, Uuid), Data> {
    let mut t = policy::parse_snapshot(snap);
    t.retain(|(_, sid), _| exists.contains(sid));
    t
}

/// 回滚到这个版本会改什么（当前 → 这个版本，确认框里列出）：`{seq, rollbackable, changes}`，`changes` 形状同版本的 `changes`，
/// 与当前相同时 `rollbackable` 为 false、`changes` 为空。比法同回滚
pub async fn rollback_preview(_a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let (snap, seq): (Value, i64) = sqlx::query_as("select snapshot, seq from policy_version where id = $1")
        .bind(id)
        .fetch_optional(&st.db)
        .await?
        .ok_or_else(|| ApiError::not_found("没有这个版本"))?;
    let rows = policy::load_rows(&st.db, false).await?;
    let target = rollback_target(&snap, &existing_scopes(&st.db).await?);
    let current = policy::parse_snapshot(&Value::Array(policy::snapshot_of(&rows)));
    let changes = if target == current { vec![] } else { diff_json(&current, &target, &policy::scope_names(&st.db).await?) };
    Ok(ok(json!({"seq": seq, "rollbackable": target != current, "changes": changes})))
}

/// 把某个历史版本的快照作为新版本发布
pub async fn rollback(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>) -> ApiResult<Json<Value>> {
    let names = policy::scope_names(&st.db).await?;
    let mut tx = st.db.begin().await?;
    lock(&mut tx).await?;
    let nodes = policy::live_nodes(&mut *tx).await?;
    let (snap, seq): (Value, i64) = sqlx::query_as("select snapshot, seq from policy_version where id = $1")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::not_found("没有这个版本"))?;
    let rows = policy::load_rows(&mut *tx, true).await?;
    let target = rollback_target(&snap, &existing_scopes(&mut *tx).await?);
    let current = policy::parse_snapshot(&Value::Array(policy::snapshot_of(&rows)));
    if target == current {
        return Err(ApiError::conflict(format!("版本 {seq} 和当前生效的规则一致，不需要回滚")));
    }
    let before = policy::set_current(&rows);
    let mut after = PolicySet::default();
    for ((s, sid), d) in &target {
        match s {
            Scope::Team => after.team.insert(*sid, d.clone()),
            Scope::Node => after.node.insert(*sid, d.clone()),
        };
    }
    // 现有行 → 快照里的值（快照里没有的范围回到默认值）；快照里有而现在没有行的 → 新建
    let existing = index(&rows);
    let mut keys: Vec<(Scope, Uuid)> = existing.keys().chain(target.keys()).copied().collect();
    keys.sort();
    keys.dedup();
    for (s, sid) in keys {
        let want = target.get(&(s, sid)).cloned().unwrap_or_else(|| s.default_data());
        match existing.get(&(s, sid)) {
            Some(r) if r.cur == want => {}
            Some(r) => {
                // 草稿正好就是回滚后的值：清掉，不留下一份与生效值相同的草稿（读时当作没有，库里却一直在）
                sqlx::query(
                    "update policy set mode = $3, domains = $4, note = $5, draft = case when $7 then null else draft end,
                            revision = revision + 1, updated_by = $6, updated_at = now()
                     where scope = $1 and scope_id = $2",
                )
                .bind(s.as_str())
                .bind(sid)
                .bind(want.mode.as_str())
                .bind(&want.domains)
                .bind(&want.note)
                .bind(&a.name)
                .bind(r.draft.as_ref() == Some(&want))
                .execute(&mut *tx)
                .await?;
            }
            None => {
                sqlx::query("insert into policy (scope, scope_id, mode, domains, note, updated_by) values ($1, $2, $3, $4, $5, $6)")
                    .bind(s.as_str())
                    .bind(sid)
                    .bind(want.mode.as_str())
                    .bind(&want.domains)
                    .bind(&want.note)
                    .bind(&a.name)
                    .execute(&mut *tx)
                    .await?;
            }
        }
    }
    let (vid, relays) = record_version(&mut tx, &a, "rollback", Some(id), &rows, &before, &after, &nodes, &names).await?;
    tx.commit().await?;
    done(&st, &relays);
    Ok(ok(version_one(&st, vid).await?))
}
