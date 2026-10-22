//! 替换节点（replace）的步骤：改软路由出口 → 交换编号。计划（改哪些路由的哪些出口、旧节点去哪）发起时算好存在作业参数里，
//! 见 api/replace.rs；路由上的脚本见 routers/replace.rs。
//! 换线路机（move_entry）在验收新入口之后、摘旧入口之前也走这里的「改软路由出口」（计划见 api/lifecycle.rs，新入口取参数 `entry_id`）。
//!
//! 改路由：先确认计划里的路由全部在线（有一台不在线就一台都不改），再一台一台改（备份 → 改出口 → 重启 OpenClash → 测延迟）。
//! 哪台失败了，它自己已经换回原配置（脚本做），之前改好的也按各自的备份换回去，作业失败：路由保持一致，修好后重试从头改。
//! 改过的路由与备份的时间戳记在参数 `routers_applied`（中控在这一步中途重启、接着做时，回滚仍用第一次改之前的那份备份）。
//! 凭据（uuid、公钥、short-id）执行时才从库里取，只进 ssh 的 stdin，不进参数、日志与审计。

use super::*;
use crate::api::replace::RouterPlan;
use crate::routers::{self, replace as rr, tunnel};

#[derive(Clone, Serialize, Deserialize)]
struct Applied {
    id: Uuid,
    name: String,
    port: i32,
    /// 这台第一次改之前的备份（回滚用它）
    stamp: String,
}

/// 新入口现在的参数（执行时从库里取）
#[derive(sqlx::FromRow)]
struct NewEntry {
    client_uuid: Uuid,
    short_id: String,
    port: i32,
    reality_sni: String,
    reality_public_key: String,
}

/// 脚本自己说的原因（`fleet：` 开头的行），没有就用 stderr 的最后几行
fn reason(o: &Output) -> String {
    let own: Vec<&str> = o.stderr.lines().filter_map(|l| l.strip_prefix("fleet：")).collect();
    if !own.is_empty() {
        return own.join("；");
    }
    let tail: Vec<&str> = o.stderr.lines().rev().filter(|l| !l.trim().is_empty()).take(3).collect();
    format!("退出码 {}：{}", o.code, tail.into_iter().rev().collect::<Vec<_>>().join(" / "))
}

pub(super) async fn routers(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    if c.job.params["routers"] == false {
        return Ok(Outcome::Skipped("选择了不改软路由".into()));
    }
    let moving = c.job.kind == "move_entry";
    let (what, redo, kept) = if moving { ("旧入口", "重新发起换线路机", "旧入口没摘") } else { ("旧节点", "重新发起替换", "编号没改") };
    let plan: Vec<RouterPlan> = serde_json::from_value(c.job.params["plan"]["routers"].clone()).unwrap_or_default();
    if plan.is_empty() {
        return Ok(Outcome::Skipped(format!("没有软路由的出口指向{what}")));
    }
    // 新入口属于谁：替换是新节点，换线路机是节点自己
    let new = if moving { c.target_id()? } else { c.param_uuid("new_id")? };
    // 1. 全部在线才动手
    let mut ports = vec![];
    let mut down = vec![];
    for r in &plan {
        match routers::load(&c.st.db, Some(r.id)).await?.pop() {
            None => bail!("软路由「{}」已经删掉了：{redo}", r.name),
            Some(row) if row.state() != "online" => {
                down.push(format!("{}（{}）", row.name, row.error.as_deref().unwrap_or(if row.state() == "offline" { "离线" } else { "还没接入" })))
            }
            Some(row) => ports.push(row.tunnel_port),
        }
    }
    if !down.is_empty() {
        return Err(StepError::err(ErrKind::Network, format!("软路由 {} 不在线，一台都没有改：恢复后点重试", down.join("、"))));
    }
    // 2. 每台要改的出口与新入口的参数
    let mut edits: Vec<Vec<rr::Edit>> = vec![];
    for r in &plan {
        let mut v = vec![];
        for x in &r.exits {
            let e: Option<NewEntry> = sqlx::query_as(
                "select e.client_uuid, e.short_id, e.port, r.reality_sni, r.reality_public_key from entry e join relay r on r.machine_id = e.relay_id
                 where e.id = $1 and e.node_id = $2 and e.lifecycle = 'active'",
            )
            .bind(match x.new_entry_id {
                Some(id) => id,
                None => c.param_uuid("entry_id")?,
            })
            .bind(new)
            .fetch_optional(&c.st.db)
            .await?;
            let Some(e) = e else { bail!("新入口（{}）已经不在用了：{redo}", x.to()) };
            v.push(rr::Edit {
                exit: x.exit.clone(),
                old_server: x.old_server.clone(),
                old_port: x.old_port,
                server: x.server.clone(),
                port: e.port as u16,
                uuid: e.client_uuid.to_string(),
                sni: e.reality_sni,
                pbk: e.reality_public_key,
                sid: e.short_id,
            });
        }
        edits.push(v);
    }
    // 3. 一台一台改
    let mut applied: Vec<Applied> = if c.first { vec![] } else { serde_json::from_value(c.job.params["routers_applied"].clone()).unwrap_or_default() };
    let mut done = vec![];
    for ((r, port), ed) in plan.iter().zip(&ports).zip(&edits) {
        let stamp = rr::stamp(OffsetDateTime::now_utc());
        if !applied.iter().any(|a| a.id == r.id) {
            applied.push(Applied { id: r.id, name: r.name.clone(), port: *port, stamp: stamp.clone() });
        }
        c.set_params(json!({"routers_applied": applied})).await?;
        let what: Vec<String> = ed.iter().map(|e| format!("{} {}:{} → {}:{}", e.exit, e.old_server, e.old_port, e.server, e.port)).collect();
        c.log(format!("软路由「{}」：改出口 {}（先备份，改完重启 OpenClash 并测延迟）", r.name, what.join("，"))).await;
        let res = ssh::exec(tunnel::via_tunnel(&c.st.cfg, r.id, *port), &rr::apply_script(ed, &stamp), rr::APPLY_TIMEOUT).await;
        let (kind, why, self_undone) = match res {
            Ok(o) if o.ok() && o.kv("replace") == Some("ok") => {
                let delays: Vec<String> = (1..=ed.len()).map(|i| format!("{} {} ms", ed[i - 1].exit, o.kv(&format!("delay_{i}")).unwrap_or("?"))).collect();
                c.log(format!("软路由「{}」改好了：备份 {}，延迟 {}", r.name, o.kv("backup").unwrap_or("?"), delays.join("，"))).await;
                done.push(json!({"id": r.id, "name": r.name, "exits": ed.iter().map(|e| json!({"exit": e.exit, "to": format!("{}:{}", e.server, e.port)})).collect::<Vec<_>>(),
                    "backup": o.kv("backup"), "delay_ms": (1..=ed.len()).map(|i| o.kv(&format!("delay_{i}")).and_then(|d| d.parse::<i64>().ok())).collect::<Vec<_>>()}));
                continue;
            }
            Ok(o) if o.code == 255 => {
                if o.ssh_kind() == Some(ErrKind::HostKey) {
                    tunnel::close_master(&c.st.cfg, r.id).await;
                }
                (o.ssh_kind().unwrap_or(ErrKind::Network), tunnel::explain(&o, true), false)
            }
            // 2：一行没写；其它：脚本已经换回这次改之前的备份（rolled_back=1），或换回也失败了
            Ok(o) => (ErrKind::Remote, reason(&o), o.code == 2 || o.kv("rolled_back") == Some("1")),
            Err(e) => (kind_of(&e), format!("{e:#}"), false),
        };
        // 回滚：之前改好的，和这一台（它没自己换回、或这一步中途重启过、这次的备份已经是改过的）
        let mut back = vec![];
        let mut stuck = vec![];
        for a in applied.iter().rev() {
            if a.id == r.id && self_undone && a.stamp == stamp {
                continue;
            }
            let out = ssh::exec(tunnel::via_tunnel(&c.st.cfg, a.id, a.port), &rr::rollback_script(&a.stamp), rr::ROLLBACK_TIMEOUT).await;
            match out {
                Ok(o) if o.ok() && matches!(o.kv("rollback"), Some("ok" | "none")) => back.push(a.name.clone()),
                Ok(o) => stuck.push(format!("{}（{}，备份 /etc/fleet-backup/openclash/*.{}）", a.name, if o.code == 255 { tunnel::explain(&o, true) } else { reason(&o) }, a.stamp)),
                Err(e) => stuck.push(format!("{}（{e:#}，备份 /etc/fleet-backup/openclash/*.{}）", a.name, a.stamp)),
            }
        }
        let mut msg = format!("软路由「{}」：{why}", r.name);
        if !back.is_empty() {
            msg += &format!("；已换回原配置：{}", back.join("、"));
        }
        if stuck.is_empty() {
            c.set_params(json!({"routers_applied": []})).await?;
            msg += &format!("。软路由都是原样，{kept}：处理好后点重试");
        } else {
            msg += &format!("；换回失败、需要人工处理：{}", stuck.join("、"));
        }
        c.warn(&msg).await;
        return Err(StepError::err(kind, msg));
    }
    Ok(Outcome::Done(json!({"routers": done})))
}

/// 交换编号：旧节点挪去目标团队（备用）/ 空号，新节点接旧编号，同一个事务里走 PATCH 的那套（machines::patch_tx）；
/// auto_compact 时新节点原来的团队随后压缩。已经换好（中控在提交后重启、接着做）的直接算完成
pub(super) async fn renumber(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let old = c.target_id()?;
    let new = c.param_uuid("new_id")?;
    let old_team = c.param_uuid("old_team_id")?;
    let old_number = c.job.params["old_number"].as_i64().map(|n| n as i32).ok_or_else(|| anyhow!("作业参数缺少 old_number"))?;
    let to_team = c.param_uuid("to_team_id")?;
    let given = c.job.params["to_number"].as_i64().map(|n| n as i32);
    let actor = c.job.created_by.as_str();
    let mut tx = c.st.db.begin().await?;
    // 先锁团队（按 id）再锁机器（按 id）：和退役收尾的「团队 → 机器」同序
    let new_team0: Option<Uuid> = sqlx::query_scalar("select team_id from machine where id = $1").bind(new).fetch_optional(&mut *tx).await?.flatten();
    let mut teams: Vec<Uuid> = [Some(old_team), Some(to_team), new_team0].into_iter().flatten().collect();
    teams.sort();
    teams.dedup();
    sqlx::query("select 1 from team where id = any($1) order by id for update").bind(&teams).execute(&mut *tx).await?;
    type Cur = (Uuid, i32, Option<Uuid>, Option<i32>, String);
    let rows: Vec<Cur> = sqlx::query_as("select id, revision, team_id, number, lifecycle::text from machine where id = any($1) order by id for update")
        .bind(vec![old, new])
        .fetch_all(&mut *tx)
        .await?;
    let get = |id: Uuid| rows.iter().find(|r| r.0 == id).cloned();
    let (Some(o), Some(n)) = (get(old), get(new)) else { bail!("旧节点或新节点已经不存在") };
    if n.2 == Some(old_team) && n.3 == Some(old_number) && o.2 == Some(to_team) {
        return Ok(Outcome::Skipped("编号已经换好".into()));
    }
    if n.4 != "active" {
        bail!("新节点现在是「{}」，不能接替：重新发起替换", crate::meta::lifecycle_label(&n.4));
    }
    if o.2 != Some(old_team) || o.3 != Some(old_number) {
        return Err(remote("旧节点的团队或编号在发起替换之后被改过，编号没有动：软路由已经指向新节点，按需要手动改名（fleetctl rename）"));
    }
    let to_number = match given {
        Some(x) => x,
        None => {
            let taken: Vec<i32> = sqlx::query_scalar(
                "select number from machine where team_id = $1 and number is not null and kind <> 'relay' and lifecycle <> 'retired' and id <> all($2)",
            )
            .bind(to_team)
            .bind(vec![old, new])
            .fetch_all(&mut *tx)
            .await?;
            let mut taken: BTreeSet<i32> = taken.into_iter().collect();
            if to_team == old_team {
                taken.insert(old_number);
            }
            (1..=999).find(|x| !taken.contains(x)).ok_or_else(|| remote("旧节点要去的团队编号用完了"))?
        }
    };
    use crate::api::machines::{patch_tx, PatchIn};
    let api = |e: crate::error::ApiError| remote(e.message);
    // 旧节点先挪开，旧编号空出来再给新节点（唯一索引 machine_number_live）
    let mut relays = patch_tx(&mut tx, actor, old, PatchIn::rename(o.1, to_team, to_number)).await.map_err(api)?;
    relays.extend(patch_tx(&mut tx, actor, new, PatchIn::rename(n.1, old_team, old_number)).await.map_err(api)?);
    // 旧节点进了备用：回收站的「原因」写明是被谁替换下来的
    let (old_before, new_before) = (c.job.params["old_name"].as_str().unwrap_or(""), c.job.params["new_name"].as_str().unwrap_or(""));
    sqlx::query("update machine m set lifecycle_reason = $2 from team t where m.id = $1 and t.id = m.team_id and t.spare")
        .bind(old)
        .bind(format!("替换下来：{new_before} 接替了 {old_before}"))
        .execute(&mut *tx)
        .await?;
    // 来源画像跟着客户走：旧节点见过的运营商 / 城市记到新节点（学习期起点取最早的 first_seen），客户换过来不算「新出现」、
    // 新机器也不用重新学 7 天。连接数不搬（统计还算在旧节点上）；新节点自己已有的不动
    sqlx::query(
        "insert into node_source_seen (node_id, dim, value, first_seen, last_seen, conns, flag)
         select $2, dim, value, first_seen, last_seen, 0, null from node_source_seen where node_id = $1 on conflict do nothing",
    )
    .bind(old)
    .bind(new)
    .execute(&mut *tx)
    .await?;
    let mut renumbered = 0;
    if let Some(t) = new_team0.filter(|_| c.st.cfg.numbering.auto_compact) {
        let (ch, rs) = crate::db::compact_team(&mut tx, t, actor, "替换节点后自动压缩").await?;
        renumbered = ch.len();
        relays.extend(rs);
    }
    let names: Vec<(Uuid, String)> = sqlx::query_as(
        "select m.id, display_name(m.kind::text, m.label, t.name, m.number) from machine m left join team t on t.id = m.team_id where m.id = any($1)",
    )
    .bind(vec![old, new])
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    relays.sort();
    relays.dedup();
    for r in relays {
        c.st.relay_bumped(r);
    }
    c.st.emit(crate::state::Event::Machines);
    let name = |id: Uuid| names.iter().find(|x| x.0 == id).map(|x| x.1.clone()).unwrap_or_default();
    let (on, nn) = (name(old), name(new));
    c.log(format!("编号已交换：新节点现在是 {nn}，旧节点现在是 {on}{}", if renumbered > 0 { format!("；{renumbered} 台节点因压缩改了编号（见审计）") } else { String::new() })).await;
    Ok(Outcome::Done(json!({"old": on, "new": nn, "renumbered": renumbered})))
}
