//! 生命周期操作：换线路机（move_entry）、搬空线路机（evacuate）、恢复已退役的机器（restore_*）。
//! 都是作业：这里只做校验、建作业和写审计，真正的步骤在 jobs/steps。

use super::ok;
use crate::auth::Actor;
use crate::db::{audit, bump_relay, Tx};
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::jobs;
use crate::meta;
use crate::names::{display_name, node_name};
use crate::ssh;
use crate::state::{AppState, Event};
use axum::extract::State;
use axum::Json;
use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::PgConnection;
use std::collections::HashSet;
use std::net::IpAddr;
use std::time::Duration;
use uuid::Uuid;

const SHARE_NOTE: &str = "换线路机后分享链接会变：客户端需要换成新的分享链接（作业完成后到节点的入口里复制）";

// ── 换线路机 ──────────────────────────────────────────────────────────────

/// (kind, lifecycle, label, 团队名, 编号)
type NodeRow = (String, String, Option<String>, Option<String>, Option<i32>);
/// (id, kind, label, 团队名, 编号, lifecycle)
type MachineRow = (Uuid, String, Option<String>, Option<String>, Option<i32>, String);

struct MovePlan {
    node_name: String,
    from_relay: Uuid,
    from_name: String,
    to_name: String,
    old_entry: Uuid,
}

/// 校验并规划一次换线路机。外层 Err = 数据库/不存在；内层 Err = 业务上不能换（给用户看的原因，evacuate 用它做「跳过」）。
/// `from`：要换掉哪个线路机上的入口；None = 节点必须恰有一个在用入口。
async fn plan_move(tx: &mut Tx<'_>, node_id: Uuid, from: Option<Uuid>, to: Uuid) -> ApiResult<Result<MovePlan, String>> {
    let node: Option<NodeRow> = sqlx::query_as(
        "select m.kind::text, m.lifecycle::text, m.label, t.name, m.number from machine m left join team t on t.id = m.team_id
         where m.id = $1 for share of m",
    )
    .bind(node_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((kind, lc, label, team, number)) = node else { return Err(ApiError::not_found("没有这台机器")) };
    let name = display_name(&kind, label.as_deref(), team.as_deref(), number);
    if kind == "relay" {
        return Ok(Err(format!("「{name}」是线路机，只有节点（VPS 节点 / 家宽代理）能换线路机")));
    }
    if lc != "active" {
        return Ok(Err(format!("「{name}」现在是「{}」，只有在用的节点能换线路机", meta::lifecycle_label(&lc))));
    }
    let target: Option<(String, String)> =
        sqlx::query_as("select coalesce(label, ''), lifecycle::text from machine where id = $1 and kind = 'relay' for share").bind(to).fetch_optional(&mut **tx).await?;
    let Some((to_name, to_lc)) = target else { return Ok(Err("目标线路机不存在".into())) };
    if to_lc != "active" {
        return Ok(Err(format!("目标线路机「{to_name}」现在是「{}」，只能换到在用的线路机", meta::lifecycle_label(&to_lc))));
    }
    let busy: Option<(String,)> = sqlx::query_as("select kind from job where target_id = $1 and state in ('queued', 'running', 'waiting', 'cancelling') limit 1")
        .bind(node_id)
        .fetch_optional(&mut **tx)
        .await?;
    if let Some((k,)) = busy {
        return Ok(Err(format!("「{name}」还有进行中的作业（{}）：等它结束再换线路机", meta::job_kind_label(&k))));
    }
    let live: Vec<(Uuid, Uuid, String, String)> = sqlx::query_as(
        "select e.id, e.relay_id, e.lifecycle::text, coalesce(rm.label, '') from entry e join machine rm on rm.id = e.relay_id
         where e.node_id = $1 and e.lifecycle in ('pending', 'active', 'disabled') order by e.created_at",
    )
    .bind(node_id)
    .fetch_all(&mut **tx)
    .await?;
    let cands: Vec<&(Uuid, Uuid, String, String)> = live.iter().filter(|e| from.is_none_or(|f| f == e.1)).collect();
    let (old_entry, from_relay, elc, from_name) = match cands.as_slice() {
        [] => return Ok(Err(format!("「{name}」{}没有在用的入口", if from.is_some() { "在指定的线路机上" } else { "" }))),
        [one] => (*one).clone(),
        many => {
            let relays: Vec<&str> = many.iter().map(|e| e.3.as_str()).collect();
            return Ok(Err(format!("「{name}」有 {} 个入口（{}）：请指定要换掉哪条线路机上的入口（from_relay_id）", many.len(), relays.join("、"))));
        }
    };
    if elc != "active" {
        return Ok(Err(format!("「{name}」在 {from_name} 上的入口现在是「{}」：先启用或等它应用完再换线路机", crate::entries::state_zh(&elc))));
    }
    if from_relay == to {
        return Ok(Err(format!("目标线路机「{to_name}」就是节点当前所在的线路机")));
    }
    let dup: Option<(Uuid,)> = sqlx::query_as("select id from entry where node_id = $1 and relay_id = $2 and lifecycle <> 'removed'")
        .bind(node_id)
        .bind(to)
        .fetch_optional(&mut **tx)
        .await?;
    if dup.is_some() {
        return Ok(Err(format!("「{name}」在「{to_name}」上已经有入口")));
    }
    Ok(Ok(MovePlan { node_name: name, from_relay, from_name, to_name, old_entry }))
}

async fn enqueue_move(tx: &mut Tx<'_>, actor: &str, node: Uuid, p: &MovePlan, to: Uuid, via: &str) -> ApiResult<Uuid> {
    let job = jobs::enqueue(tx, "move_entry", Some(node), json!({"from_relay_id": p.from_relay, "to_relay_id": to, "old_entry_id": p.old_entry}), actor).await?;
    audit(
        tx,
        actor,
        "entry.move",
        "machine",
        Some(node),
        json!({"name": p.node_name, "from": p.from_name, "to": p.to_name, "from_relay_id": p.from_relay, "to_relay_id": to, "old_entry_id": p.old_entry, "job_id": job, "via": via}),
    )
    .await?;
    Ok(job)
}

#[derive(Deserialize)]
pub struct MoveIn {
    to_relay_id: Uuid,
    /// 节点有多个入口时，指定换掉哪条线路机上的
    from_relay_id: Option<Uuid>,
}

/// 把节点换到另一台线路机：新入口验收通过后才摘旧入口，期间不断网。
pub async fn move_node(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<MoveIn>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let plan = plan_move(&mut tx, id, b.from_relay_id, b.to_relay_id).await?.map_err(ApiError::conflict)?;
    let job = enqueue_move(&mut tx, &a.name, id, &plan, b.to_relay_id, "move").await?;
    tx.commit().await?;
    st.wake_jobs();
    st.emit(Event::Job { id: job, target: Some(id) });
    Ok(ok(json!({
        "job_id": job, "name": plan.node_name, "from": plan.from_name, "to": plan.to_name,
        "share_changed": true, "note": SHARE_NOTE,
    })))
}

#[derive(Deserialize)]
pub struct EvacuateIn {
    to_relay_id: Uuid,
}

/// 搬空线路机：为这台线路机上每个在用入口的节点各建一个换线路机作业（线路机退役前用）。
/// 暂时不能搬的节点（停用、有别的作业、已在目标线路机上有入口…）列在 skipped 里，不影响其它节点。
pub async fn evacuate(a: Actor, State(st): State<AppState>, ApiPath(relay): ApiPath<Uuid>, ApiJson(b): ApiJson<EvacuateIn>) -> ApiResult<Json<Value>> {
    if relay == b.to_relay_id {
        return Err(ApiError::bad_request("目标线路机不能和要搬空的是同一台"));
    }
    let mut tx = st.db.begin().await?;
    let src: Option<(String, String, String)> = sqlx::query_as("select kind::text, coalesce(label, ''), lifecycle::text from machine where id = $1")
        .bind(relay)
        .fetch_optional(&mut *tx)
        .await?;
    let (kind, from_name, lc) = src.ok_or_else(|| ApiError::not_found("没有这台机器"))?;
    if kind != "relay" {
        return Err(ApiError::conflict("只有线路机能搬空"));
    }
    if lc == "retired" {
        return Err(ApiError::conflict("这台线路机已经退役"));
    }
    let to: Option<(String, String)> = sqlx::query_as("select coalesce(label, ''), lifecycle::text from machine where id = $1 and kind = 'relay'")
        .bind(b.to_relay_id)
        .fetch_optional(&mut *tx)
        .await?;
    let (to_name, to_lc) = to.ok_or_else(|| ApiError::not_found("目标线路机不存在"))?;
    if to_lc != "active" {
        return Err(ApiError::conflict(format!("目标线路机「{to_name}」现在是「{}」，只能搬到在用的线路机", meta::lifecycle_label(&to_lc))));
    }
    let nodes: Vec<(Uuid,)> = sqlx::query_as(
        "select distinct e.node_id from entry e join machine n on n.id = e.node_id
         where e.relay_id = $1 and e.lifecycle in ('pending', 'active', 'disabled') and n.kind <> 'relay' order by 1",
    )
    .bind(relay)
    .fetch_all(&mut *tx)
    .await?;
    let mut created = vec![];
    let mut skipped = vec![];
    for (node,) in nodes {
        match plan_move(&mut tx, node, Some(relay), b.to_relay_id).await? {
            Ok(plan) => {
                let job = enqueue_move(&mut tx, &a.name, node, &plan, b.to_relay_id, "evacuate").await?;
                created.push(json!({"machine_id": node, "name": plan.node_name, "job_id": job}));
            }
            Err(reason) => {
                let name: Option<(String,)> = sqlx::query_as(
                    "select display_name(m.kind::text, m.label, t.name, m.number)
                     from machine m left join team t on t.id = m.team_id where m.id = $1",
                )
                .bind(node)
                .fetch_optional(&mut *tx)
                .await?;
                skipped.push(json!({"machine_id": node, "name": name.map(|(n,)| n), "reason": reason}));
            }
        }
    }
    audit(
        &mut tx,
        &a.name,
        "relay.evacuate",
        "machine",
        Some(relay),
        json!({"from": from_name, "to": to_name, "to_relay_id": b.to_relay_id, "jobs": created, "skipped": skipped}),
    )
    .await?;
    tx.commit().await?;
    st.wake_jobs();
    for j in &created {
        let uuid = |k: &str| j[k].as_str().and_then(|s| Uuid::try_parse(s).ok());
        if let Some(id) = uuid("job_id") {
            st.emit(Event::Job { id, target: uuid("machine_id") });
        }
    }
    st.emit(Event::Machines);
    Ok(ok(json!({"from": from_name, "to": to_name, "jobs": created, "skipped": skipped, "share_changed": !created.is_empty(), "note": SHARE_NOTE})))
}

// ── 恢复已退役的机器 ──────────────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct RM {
    kind: String,
    team_id: Option<Uuid>,
    number: Option<i32>,
    label: Option<String>,
    ip: String,
    /// SSH 端口（同机 IP 节点是宿主机的）
    ssh_port: i32,
    lifecycle: String,
    reason: Option<String>,
    /// 同机 IP 节点的宿主机
    host_id: Option<Uuid>,
    /// 退役时存下的主机公钥（known_hosts 行）
    host_keys: Option<String>,
    /// 退役时留下的出站参数（秘密已清掉）：VPS 节点是 SS 端口（NAT 映射的公网端口），家宽代理是上游。迁移 0029 之前退役的没有
    o_protocol: Option<String>,
    o_host: Option<String>,
    o_port: Option<i32>,
    o_user: Option<String>,
}

async fn load_rm(conn: &mut PgConnection, id: Uuid, lock: bool) -> ApiResult<RM> {
    let sql = format!(
        "select m.kind::text kind, m.team_id, m.number, m.label, host(m.primary_ip) ip, coalesce(h.ssh_port, m.ssh_port) ssh_port, m.lifecycle::text lifecycle,
            m.lifecycle_reason reason, m.host_id, m.retired_host_keys host_keys, o.protocol o_protocol, o.host o_host, o.port o_port, o.username o_user
         from machine m left join machine h on h.id = m.host_id left join node_outbound o on o.machine_id = m.id
         where m.id = $1{}",
        if lock { " for update of m" } else { "" }
    );
    sqlx::query_as(&sql).bind(id).fetch_optional(conn).await?.ok_or_else(|| ApiError::not_found("没有这台机器（已彻底删除的机器不能恢复）"))
}

/// 恢复会落成什么样：团队/编号/名字，以及不能恢复的原因
struct RPlan {
    old_name: String,
    team_id: Option<Uuid>,
    team_name: Option<String>,
    number: Option<i32>,
    label: Option<String>,
    new_name: String,
    blockers: Vec<String>,
    /// blockers 里属于输入错误的（编号超范围、指定的团队不存在）：提交时回 400，其余阻断回 409
    invalid: Vec<String>,
    ip_conflict: Option<Value>,
}

async fn restore_plan(conn: &mut PgConnection, id: Uuid, m: &RM, team_ov: Option<Uuid>, num_ov: Option<i32>, label_ov: Option<String>, lock: bool) -> ApiResult<RPlan> {
    let (mut blockers, mut invalid) = (vec![], vec![]);
    let old_team: Option<String> = match m.team_id {
        Some(t) => sqlx::query_scalar("select name from team where id = $1").bind(t).fetch_optional(&mut *conn).await?,
        None => None,
    };
    let old_name = display_name(&m.kind, m.label.as_deref(), old_team.as_deref(), m.number);
    if m.lifecycle != "retired" {
        blockers.push(format!("这台机器现在是「{}」，只有已退役的机器能恢复", meta::lifecycle_label(&m.lifecycle)));
    }
    // IP 已被别的未退役机器占用（退役时 IP 就释放了，可能已经上架给了别的机器）
    let conflict: Option<MachineRow> = sqlx::query_as(
        "select x.id, x.kind::text, x.label, t.name, x.number, x.lifecycle::text from machine x left join team t on t.id = x.team_id
         where host(x.primary_ip) = $1 and x.lifecycle <> 'retired' and x.id <> $2",
    )
    .bind(&m.ip)
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;
    let ip_conflict = conflict.map(|(cid, k, l, t, n, lc)| {
        let name = display_name(&k, l.as_deref(), t.as_deref(), n);
        // 占用者常常和这台同名（退役后编号回收、同一个 IP 又上架了一次）：写明是另一台，不然读着像自己占自己
        let who = if name == old_name { format!("另一台未退役的「{name}」（与这台同名）") } else { format!("未退役的机器「{name}」") };
        blockers.push(format!("IP {} 已被{who}占用：先把它退役，或确认这台机器的 IP 没有变", m.ip));
        json!({"machine_id": cid, "name": name, "lifecycle": lc})
    });

    let (mut team_id, mut team_name, mut number, mut label) = (None, None, None, None);
    let new_name;
    if m.kind == "relay" {
        let l = label_ov.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(String::from).or_else(|| m.label.clone()).unwrap_or_default();
        let dup: Option<(Uuid,)> = sqlx::query_as("select id from machine where kind = 'relay' and lifecycle <> 'retired' and label = $1 and id <> $2")
            .bind(&l)
            .bind(id)
            .fetch_optional(&mut *conn)
            .await?;
        if dup.is_some() {
            blockers.push(format!("已经有同名线路机「{l}」：恢复时换一个名字"));
        }
        if let Err(e) = super::machines::check_text(Some(&l), None) {
            invalid.push(e);
        }
        new_name = l.clone();
        label = Some(l);
    } else {
        let tid = team_ov.or(m.team_id);
        let tn: Option<String> = match tid {
            Some(t) => {
                let sql = if lock { "select name from team where id = $1 for update" } else { "select name from team where id = $1" };
                sqlx::query_scalar(sql).bind(t).fetch_optional(&mut *conn).await?
            }
            None => None,
        };
        match &tn {
            None => {
                blockers.push("团队不存在".into());
                if team_ov.is_some() {
                    invalid.push("团队不存在".into());
                }
                new_name = old_name.clone();
            }
            Some(tn) => {
                let taken: Vec<(i32,)> = sqlx::query_as(
                    "select number from machine where team_id = $1 and number is not null and kind <> 'relay' and lifecycle <> 'retired' and id <> $2",
                )
                .bind(tid)
                .bind(id)
                .fetch_all(&mut *conn)
                .await?;
                let taken: HashSet<i32> = taken.into_iter().map(|(n,)| n).collect();
                let n = match num_ov {
                    Some(n) if !(1..=999).contains(&n) => {
                        blockers.push("编号范围 1–999".into());
                        invalid.push("编号范围 1–999".into());
                        None
                    }
                    Some(n) if taken.contains(&n) => {
                        blockers.push(format!("编号 {n} 已被占用"));
                        None
                    }
                    Some(n) => Some(n),
                    // 默认接在团队末尾（编号连续时就是 N+1）
                    None => (1..=999).find(|n| !taken.contains(n)),
                };
                if n.is_none() && num_ov.is_none() {
                    blockers.push("这个团队的编号用完了".into());
                }
                new_name = n.map(|n| node_name(tn, n)).unwrap_or_else(|| old_name.clone());
                number = n;
            }
        }
        team_id = tid;
        team_name = tn;
    }
    // 同机 IP 节点：宿主机要在用（SSH、agent 都是它的）。提交时锁住宿主机行（在团队之后），重新分 SS 端口不和别的上架撞车
    if let Some(h) = m.host_id {
        let sql = format!(
            "select h.lifecycle::text, display_name(h.kind::text, h.label, t.name, h.number) from machine h left join team t on t.id = h.team_id where h.id = $1{}",
            if lock { " for update of h" } else { "" }
        );
        let (hl, hn): (String, String) = sqlx::query_as(&sql).bind(h).fetch_one(&mut *conn).await?;
        if hl != "active" {
            blockers.push(format!("宿主机「{hn}」现在是「{}」：同机 IP 节点要等宿主机在用才能恢复", meta::lifecycle_label(&hl)));
        }
    }
    Ok(RPlan { old_name, team_id, team_name, number, label, new_name, blockers, invalid, ip_conflict })
}

/// 用中控密钥试登录这台机器（预检用）：先 keyscan 到临时 known_hosts，不碰正式的。
/// 退役时存过主机公钥的，另给 host_key_changed（现在的公钥不在存的里面）。
async fn probe_login(st: &AppState, m: &RM) -> Value {
    let port = m.ssh_port as u16;
    // 要读到 SSH 版本行才算通（中控出网经透明代理时，TCP 对任何地址都连得上）
    if let Err(why) = ssh::ssh_probe(&m.ip, port, Duration::from_secs(3)).await {
        return json!({"reachable": false, "key_login": "unreachable", "detail": why});
    }
    let tmp = std::env::temp_dir().join(format!("fleet-restore-probe-{}", Uuid::new_v4().simple()));
    let run = async {
        let key = ssh::pin_host_key(&tmp, &m.ip, port, true).await?;
        let t = ssh::Target { ip: m.ip.clone(), port, key: st.cfg.ssh_key.clone(), known_hosts: tmp.clone(), password: None };
        let out = t.run("true", Duration::from_secs(15)).await?;
        anyhow::Ok((key, out))
    };
    let res = tokio::time::timeout(Duration::from_secs(30), run).await;
    let _ = tokio::fs::remove_file(&tmp).await;
    let changed = match (&res, m.host_keys.as_deref()) {
        (Ok(Ok((key, _))), Some(saved)) => Some(!ssh::keys_of(saved).contains(key)),
        _ => None,
    };
    let mut v = match res {
        Ok(Ok((key, out))) if out.ok() => json!({"reachable": true, "key_login": "ok", "host_key": ssh::fingerprint(&key)}),
        Ok(Ok((key, out))) if out.ssh_kind() == Some(ssh::ErrKind::Auth) => json!({"reachable": true, "key_login": "denied", "host_key": ssh::fingerprint(&key)}),
        Ok(Ok((key, out))) => json!({"reachable": true, "key_login": "error", "host_key": ssh::fingerprint(&key), "detail": out.stderr.trim()}),
        Ok(Err(e)) => json!({"reachable": true, "key_login": "error", "detail": format!("{e:#}")}),
        Err(_) => json!({"reachable": true, "key_login": "error", "detail": "探测超时"}),
    };
    if let Some(ch) = changed {
        v["host_key_changed"] = json!(ch);
    }
    v
}

/// 预检看到的主机公钥与退役时存的不同：没勾「重置主机指纹」是阻断，勾了只提醒
fn host_key_note(m: &RM, info: &Value, reset: bool) -> Option<(bool, String)> {
    if info["host_key_changed"] != true {
        return None;
    }
    let was = m.host_keys.as_deref().and_then(|s| ssh::keys_of(s).into_iter().next()).map(|k| ssh::fingerprint(&k)).unwrap_or_default();
    let now = info["host_key"].as_str().unwrap_or("");
    Some(if reset {
        (false, format!("主机指纹与退役时不同（退役时 {was}，现在 {now}）：已勾选「重置主机指纹」，恢复时按现在的公钥重新记录"))
    } else {
        (true, format!("主机指纹与退役时不同（退役时 {was}，现在 {now}）：确认机器重装过再勾选「重置主机指纹」；否则可能是这个 IP 已经换了主人"))
    })
}

#[derive(Deserialize)]
pub struct PreviewQ {
    team_id: Option<Uuid>,
    number: Option<i32>,
    label: Option<String>,
    /// false = 不试 SSH 登录（只做库内检查）
    probe: Option<bool>,
    /// 打算勾「重置主机指纹」：主机指纹与退役时不同只提醒、不阻断
    reset_host_key: Option<bool>,
}

/// 恢复预检：IP 冲突、将分配的团队/编号/名字、还需要提供哪些参数
pub async fn restore_preview(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiQuery(q): ApiQuery<PreviewQ>) -> ApiResult<Json<Value>> {
    let (m, mut p, last_relay) = {
        let mut conn = st.db.acquire().await?;
        let m = load_rm(&mut conn, id, false).await?;
        let p = restore_plan(&mut conn, id, &m, q.team_id, q.number, q.label.clone(), false).await?;
        let last: Option<(Uuid, String)> = sqlx::query_as(
            "select e.relay_id, coalesce(r.label, '') from entry e join machine r on r.id = e.relay_id
             where e.node_id = $1 and r.lifecycle = 'active' order by e.created_at desc limit 1",
        )
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
        (m, p, last)
    };
    let mut warnings: Vec<String> = vec![];
    // 同机 IP 节点不试登录：它用中控密钥登录宿主机，宿主机在用就能登
    let ssh_info = if m.kind != "proxy" && m.host_id.is_none() && m.lifecycle == "retired" && q.probe != Some(false) {
        // 试登录会用中控密钥 SSH 上机器：和 POST 恢复一样要 write，并记审计
        a.require_scope("write")?;
        let info = probe_login(&st, &m).await;
        let mut tx = st.db.begin().await?;
        audit(&mut tx, &a.name, "restore.probe", "machine", Some(id), json!({"name": p.old_name, "ip": m.ip, "key_login": info["key_login"]})).await?;
        tx.commit().await?;
        Some(info)
    } else {
        None
    };
    // root 密码：中控密钥还能登录就不需要；被拒就必填；探测不到时可选（填了也不亏）
    let password = match (m.kind.as_str(), ssh_info.as_ref().and_then(|i| i["key_login"].as_str())) {
        ("proxy", _) => "no",
        _ if m.host_id.is_some() => "no",
        (_, Some("ok")) => "no",
        (_, Some("denied")) => "required",
        _ => "optional",
    };
    if let Some(i) = &ssh_info {
        if i["key_login"] == "unreachable" {
            let why = i["detail"].as_str().unwrap_or("连不上");
            warnings.push(format!("SSH {}:{} {why}：恢复作业会在「预检与身份」一步失败，先确认机器已开机、IP 没变", m.ip, m.ssh_port));
        }
        // 只是提示：POST 不再探测，真正的拦截在恢复作业的预检（放回退役时的公钥再核对）
        match host_key_note(&m, i, q.reset_host_key == Some(true)) {
            Some((true, msg)) => p.blockers.push(msg),
            Some((false, msg)) => warnings.push(msg),
            None => {}
        }
    }
    Ok(super::ok(json!({
        "id": id,
        "kind": m.kind,
        "lifecycle": m.lifecycle,
        "ip": m.ip,
        "old_name": p.old_name,
        "name": p.new_name,
        "team": p.team_id.zip(p.team_name).map(|(id, name)| json!({"id": id, "name": name})),
        "number": p.number,
        "label": p.label,
        "restorable": p.blockers.is_empty(),
        "blockers": p.blockers,
        "ip_conflict": p.ip_conflict,
        "warnings": warnings,
        "last_relay": last_relay.map(|(id, name)| json!({"id": id, "name": name})),
        "ssh": ssh_info,
        // 退役时留下的出站参数（不含秘密），恢复时默认沿用
        "outbound": m.o_protocol.as_ref().map(|p| json!({"protocol": p, "host": m.o_host, "port": m.o_port, "username": m.o_user})),
        "needs": {
            "relay": m.kind != "relay",
            // 家宽代理没留下上游（迁移 0029 之前退役的）：协议、地址、端口要重新给
            "upstream": m.kind == "proxy" && m.o_protocol.is_none(),
            // 留下的上游有用户名：密码退役时已清掉，要重新给
            "upstream_password": m.kind == "proxy" && m.o_user.as_deref().is_some_and(|u| !u.is_empty()),
            "password": password,
        },
        "retired_reason": m.reason,
    })))
}

#[derive(Deserialize)]
pub struct LoginIn {
    password: Option<String>,
}

/// 家宽代理的上游：没给的字段沿用退役时留下的（协议、地址、端口、用户名；用户名给空串 = 去掉）
#[derive(Deserialize)]
pub struct UpstreamIn {
    protocol: Option<String>,
    host: Option<String>,
    port: Option<u16>,
    username: Option<String>,
    password: Option<String>,
}

/// 恢复后的上游：(协议, 地址, 端口, 用户名, 密码)。密码退役时已清掉，有用户名就要重新给
fn restore_upstream(m: &RM, u: Option<&UpstreamIn>) -> Result<(String, String, u16, Option<String>, String), String> {
    let protocol = u.and_then(|u| u.protocol.clone()).or_else(|| m.o_protocol.clone()).unwrap_or_default();
    let host = u.and_then(|u| u.host.as_deref()).or(m.o_host.as_deref()).unwrap_or("").trim().to_string();
    let port = u.and_then(|u| u.port).or_else(|| m.o_port.and_then(|p| u16::try_from(p).ok())).unwrap_or(0);
    let username = u.and_then(|u| u.username.as_deref()).or(m.o_user.as_deref()).map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    let password = u.and_then(|u| u.password.clone()).unwrap_or_default();
    if !matches!(protocol.as_str(), "socks5" | "http") || host.is_empty() || port == 0 {
        return Err("家宽代理退役时没留下上游参数（迁移 0029 之前退役的），要在 upstream 里给全协议（socks5/http）、地址和端口".into());
    }
    if username.is_some() && password.is_empty() {
        return Err("上游有用户名：密码在退役时已清掉，要在 upstream.password 里重新填".into());
    }
    Ok((protocol, host, port, username, password))
}

#[derive(Deserialize)]
pub struct RestoreIn {
    team_id: Option<Uuid>,
    number: Option<i32>,
    /// 线路机恢复时可改名（原名被占用时）
    label: Option<String>,
    /// 节点要接入的线路机
    relay_id: Option<Uuid>,
    login: Option<LoginIn>,
    /// 家宽代理：没给的字段沿用退役时留下的；密码退役时已清掉
    upstream: Option<UpstreamIn>,
    /// VPS 节点的公网（NAT 映射）SS 端口：不给沿用退役前的，都没有为 8388
    ss_port: Option<u16>,
    reason: Option<String>,
    /// 机器重装过、主机指纹变了：允许预检时覆盖已记录的指纹
    reset_host_key: Option<bool>,
}

/// 恢复已退役的机器：机器保持原 id，生命周期 retired → provisioning，走和上架相同的收敛步骤
pub async fn restore(a: Actor, State(st): State<AppState>, ApiPath(id): ApiPath<Uuid>, ApiJson(b): ApiJson<RestoreIn>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let m = load_rm(&mut tx, id, true).await?;
    let p = restore_plan(&mut tx, id, &m, b.team_id, b.number, b.label.clone(), true).await?;
    if !p.invalid.is_empty() {
        return Err(ApiError::bad_request(p.invalid.join("；")).with_detail(json!({"blockers": p.blockers, "ip_conflict": p.ip_conflict})));
    }
    if !p.blockers.is_empty() {
        return Err(ApiError::conflict(p.blockers.join("；")).with_detail(json!({"blockers": p.blockers, "ip_conflict": p.ip_conflict})));
    }
    if m.kind != "relay" {
        let Some(relay) = b.relay_id else { return Err(ApiError::bad_request("要选一台在用的线路机")) };
        let r: Option<(String,)> = sqlx::query_as("select host(primary_ip) from machine where id = $1 and kind = 'relay' and lifecycle = 'active'")
            .bind(relay)
            .fetch_optional(&mut *tx)
            .await?;
        let Some((rip,)) = r else { return Err(ApiError::bad_request("要选一台在用的线路机")) };
        if m.kind == "vps" && !matches!(rip.parse::<IpAddr>(), Ok(IpAddr::V4(_))) {
            return Err(ApiError::bad_request("暂不支持 IPv6 线路机：节点防火墙的放行名单只处理 IPv4"));
        }
    }
    let upstream = if m.kind == "proxy" { Some(restore_upstream(&m, b.upstream.as_ref()).map_err(ApiError::bad_request)?) } else { None };
    let reason = b.reason.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    let n = sqlx::query(
        "update machine set lifecycle = 'provisioning', team_id = $2, number = $3, label = coalesce($4, label), lifecycle_reason = '恢复中',
            revision = revision + 1, updated_at = now() where id = $1 and lifecycle = 'retired'",
    )
    .bind(id)
    .bind(p.team_id)
    .bind(p.number)
    .bind(&p.label)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(ApiError::conflict("这台机器刚刚被别人改过，请刷新后再试"));
    }
    match m.kind.as_str() {
        "vps" => {
            let stored = m.o_port.and_then(|p| u16::try_from(p).ok());
            let port = match m.host_id {
                // 同机 IP 节点：原来的 SS 端口没被宿主机上别的同机节点占走就沿用，否则重新分（宿主机行在 restore_plan 里锁着）
                Some(h) => {
                    let taken = crate::db::ip_node_ports(&mut tx, h, Some(id)).await?;
                    stored.filter(|p| !taken.contains(p)).or_else(|| crate::db::free_ss_port(&taken)).ok_or_else(|| ApiError::conflict("宿主机上没有空闲的 SS 端口"))?
                }
                None => b.ss_port.or(stored).unwrap_or(meta::SS_PORT),
            };
            sqlx::query(
                "insert into node_outbound (machine_id, protocol, host, port, method, secret_enc) values ($1, 'shadowsocks', $2, $3, $5, $4)
                 on conflict (machine_id) do update set protocol = excluded.protocol, host = excluded.host, port = excluded.port, method = excluded.method,
                    username = null, secret_enc = excluded.secret_enc, updated_at = now()",
            )
            .bind(id)
            .bind(&m.ip)
            .bind(port as i32)
            .bind(st.secrets.seal_str(&super::provision::ss_password()))
            .bind(meta::SS_METHOD)
            .execute(&mut *tx)
            .await?;
        }
        "proxy" => {
            let (protocol, host, port, username, password) = upstream.expect("validated");
            sqlx::query(
                "insert into node_outbound (machine_id, protocol, host, port, username, secret_enc) values ($1, $2, $3, $4, $5, $6)
                 on conflict (machine_id) do update set protocol = excluded.protocol, host = excluded.host, port = excluded.port, method = null,
                    username = excluded.username, secret_enc = excluded.secret_enc, updated_at = now()",
            )
            .bind(id)
            .bind(protocol)
            .bind(host)
            .bind(port as i32)
            .bind(username)
            .bind(st.secrets.seal_str(&password))
            .execute(&mut *tx)
            .await?;
        }
        _ => {
            // 线路机：期望状态代数 +1，等 agent 重新装好后必须真的应用一次（之前记下的已应用代数不能算数）
            bump_relay(&mut tx, id).await?;
            sqlx::query("update relay set apply_error = null where machine_id = $1").bind(id).execute(&mut *tx).await?;
        }
    }
    let mut params = json!({
        "reason": reason,
        "prev": {"team_id": m.team_id, "number": m.number, "label": m.label, "reason": m.reason, "name": p.old_name},
    });
    if m.kind != "relay" {
        params["relay_id"] = json!(b.relay_id);
    }
    // 同机 IP 节点登录宿主机用中控密钥：不收 root 密码，也不重置宿主机的指纹
    if m.kind != "proxy" && m.host_id.is_none() {
        if b.reset_host_key == Some(true) {
            params["reset_host_key"] = json!(true);
        }
        if let Some(pw) = b.login.as_ref().and_then(|l| l.password.as_deref()).filter(|p| !p.is_empty()) {
            let enc = base64::engine::general_purpose::STANDARD.encode(st.secrets.seal_str(pw));
            params["login"] = json!({"password_enc": enc});
        }
    }
    let kind = if m.host_id.is_some() { "restore_ip".to_string() } else { format!("restore_{}", m.kind) };
    let job = jobs::enqueue(&mut tx, &kind, Some(id), params, &a.name).await?;
    audit(
        &mut tx,
        &a.name,
        "machine.restore",
        "machine",
        Some(id),
        json!({"old_name": p.old_name, "name": p.new_name, "kind": m.kind, "ip": m.ip, "reason": reason, "relay_id": b.relay_id, "job_id": job}),
    )
    .await?;
    tx.commit().await?;
    st.wake_jobs();
    if m.kind == "relay" {
        st.relay_bumped(id);
    }
    st.emit(Event::Machines);
    st.emit(Event::Job { id: job, target: Some(id) });
    Ok(ok(json!({"job_id": job, "machine_id": id, "name": p.new_name, "old_name": p.old_name, "number": p.number})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_key_note_blocks_unless_reset() {
        let m = RM {
            kind: "vps".into(),
            team_id: None,
            number: None,
            label: None,
            ip: "203.0.113.1".into(),
            ssh_port: 22,
            lifecycle: "retired".into(),
            reason: None,
            host_id: None,
            host_keys: Some("203.0.113.1 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOld".into()),
            o_protocol: None,
            o_host: None,
            o_port: None,
            o_user: None,
        };
        let same = json!({"key_login": "ok", "host_key": "SHA256:x", "host_key_changed": false});
        assert!(host_key_note(&m, &same, false).is_none());
        assert!(host_key_note(&m, &json!({"key_login": "unreachable"}), false).is_none(), "探测不到公钥：不判断");
        let changed = json!({"key_login": "ok", "host_key": "SHA256:new", "host_key_changed": true});
        let (block, msg) = host_key_note(&m, &changed, false).unwrap();
        assert!(block && msg.contains("主机指纹与退役时不同") && msg.contains(&ssh::fingerprint("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOld")) && msg.contains("SHA256:new"), "{msg}");
        let (block, msg) = host_key_note(&m, &changed, true).unwrap();
        assert!(!block && msg.contains("已勾选"), "{msg}");
    }

    #[test]
    fn restore_upstream_falls_back_to_stored() {
        let mut m = RM {
            kind: "proxy".into(),
            team_id: None,
            number: None,
            label: None,
            ip: "203.0.113.1".into(),
            ssh_port: 22,
            lifecycle: "retired".into(),
            reason: None,
            host_id: None,
            host_keys: None,
            o_protocol: Some("http".into()),
            o_host: Some("up.example".into()),
            o_port: Some(3128),
            o_user: Some("bob".into()),
        };
        let up = |v: Value| serde_json::from_value::<UpstreamIn>(v).unwrap();
        assert!(restore_upstream(&m, None).unwrap_err().contains("密码"), "有用户名：密码要重新给");
        let r = restore_upstream(&m, Some(&up(json!({"password": "pw"})))).unwrap();
        assert_eq!(r, ("http".into(), "up.example".into(), 3128, Some("bob".into()), "pw".into()));
        let r = restore_upstream(&m, Some(&up(json!({"port": 8080, "username": ""})))).unwrap();
        assert_eq!(r, ("http".into(), "up.example".into(), 8080, None, String::new()), "给了的字段覆盖，用户名空串 = 去掉");
        m.o_protocol = None;
        m.o_host = None;
        m.o_port = None;
        m.o_user = None;
        assert!(restore_upstream(&m, Some(&up(json!({"password": "pw"})))).unwrap_err().contains("给全"), "没留下上游：要给全");
    }
}
