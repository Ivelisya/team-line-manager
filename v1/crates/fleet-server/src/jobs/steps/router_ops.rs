//! 软路由 OpenClash 的三种作业（计划见 api/router_ops.rs，路由上的脚本见 routers::ops）：
//! - 重启 OpenClash（router_restart，步骤 restart_openclash）：配置不动，重启、等 API、测 fleet 出口的延迟。重启前通、重启后测了 3 次都不通的出口
//!   让作业失败并写明是哪几个（重启前就不通的只记一句）；重启会掐断隧道的路由，脚本在后台跑完，中控等隧道回连后取结果（同改出口）。
//! - 出口改名（router_rename）与来源 IP 分流规则（router_rules），步骤 patch_config：动手前读路由现在的源配置，按发起时核对过的参数重新算一遍
//!   要改的行（routers::ops::rename_patch / rules_patch）：发起之后路由的配置被改得对不上了就不改、作业失败；脚本自己备份、改、重启、核对，不行整份换回。
//!   改名成功后「已建立的对应」与出口的历史指标改记到新名字下（参数 `renamed_db` 记下做过了，重试不再做第二次）。
//!
//! 同一台路由同时只有一个作业在改（同「改软路由出口」的 Locks；拿不到就排队），路由上另有一把锁（BUSY 时等一分钟再来）。

use super::replace::{pinned_stamps, reason, run_script, ssh_failed, Locks, BUSY_RETRY};
use super::*;
use crate::routers::{self, collect, ops, replace as rr};
use std::collections::HashSet;

/// 拿锁，拿不到排队（同 router_exits）。返回 None = 已经排上队
async fn lock_or_queue<'a>(c: &Ctx<'a>, r: &routers::Row) -> anyhow::Result<Option<Locks<'a>>> {
    let mut locks = Locks::new(&c.st.db);
    if !locks.take(r.id).await? {
        if !c.job.params["routers_queued"].is_string() {
            c.log(format!("软路由「{}」正在被别的作业改，排队等它改完", r.name)).await;
            c.set_params(json!({"routers_queued": r.name})).await?;
        }
        return Ok(None);
    }
    if c.job.params["routers_queued"].is_string() {
        c.set_params(json!({"routers_queued": null})).await?;
    }
    Ok(Some(locks))
}

async fn online_router(c: &Ctx<'_>, what: &str) -> anyhow::Result<routers::Row> {
    let id = c.target_id()?;
    let Some(r) = routers::load(&c.st.db, Some(id)).await?.into_iter().next() else { return Err(remote("这台软路由已经删掉了")) };
    if r.state() != "online" {
        return Err(StepError::err(ErrKind::Network, format!("软路由「{}」现在不在线，{what}：等它在线后点重试", r.name)));
    }
    Ok(r)
}

/// 重启 OpenClash，测 fleet 出口
pub(super) async fn restart(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let r = online_router(c, "没有重启").await?;
    let Some(_locks) = lock_or_queue(c, &r).await? else { return Ok(Outcome::Queue(10)) };
    let snap = r.snapshot().unwrap_or_default();
    // 要测的：对上了面板入口的出口（客户自己加的不归我们管）；重启前通不通按最近一次采集
    let fleet: Vec<&collect::Exit> = snap.exits.iter().filter(|x| x.sync != "unmanaged").collect();
    let names: Vec<String> = fleet.iter().map(|x| x.name.clone()).collect();
    let t = OffsetDateTime::now_utc();
    let stamp = rr::stamp(t);
    c.log(format!("软路由「{}」：重启 OpenClash（配置不动；经它的连接会全部断开），重启后测 {} 个面板出口的延迟", r.name, names.len())).await;
    let res = run_script(c, r.id, &r.name, r.tunnel_port, &ops::restart_script(&names, t), "restart", &stamp, rr::APPLY_TIMEOUT).await;
    let o = match res {
        Ok(o) => o,
        Err(e) => return Err(StepError::err(kind_of(&e), format!("软路由「{}」：{e:#}（可能已经重启过了：fleetctl router show 看 OpenClash 的状态再决定要不要重试）", r.name))),
    };
    if o.code == rr::BUSY {
        c.warn(format!("软路由「{}」：{}，{BUSY_RETRY} 秒后再试", r.name, reason(&o))).await;
        return Ok(Outcome::Wait(BUSY_RETRY));
    }
    if o.code == 255 {
        let why = ssh_failed(c, &r.name, &o).await;
        return Err(StepError::err(o.ssh_kind().unwrap_or(ErrKind::Network), format!("软路由「{}」：{why}（可能已经重启过了：fleetctl router show 看 OpenClash 的状态再决定要不要重试）", r.name)));
    }
    c.st.routers_refresh.now([r.id]).await;
    if !o.ok() || o.kv("restart") != Some("ok") {
        let done = if o.kv("restarted") == Some("1") { "；重启命令已经执行，OpenClash 可能停着：马上 fleetctl router show 看" } else { "" };
        return Err(remote(format!("软路由「{}」：{}{done}", r.name, reason(&o))));
    }
    let (mut back, mut lost, mut was_down) = (vec![], vec![], vec![]);
    for (i, x) in fleet.iter().enumerate() {
        match o.kv(&format!("delay_{}", i + 1)).and_then(|v| v.parse::<i64>().ok()) {
            Some(ms) => back.push(json!({"exit": x.name, "delay_ms": ms})),
            None if collect::usable(x) => lost.push(x.name.clone()),
            None => was_down.push(x.name.clone()),
        }
    }
    let list = |v: &[Value]| v.iter().map(|x| format!("{} {} ms", x["exit"].as_str().unwrap_or(""), x["delay_ms"])).collect::<Vec<_>>().join("，");
    c.log(format!("软路由「{}」重启好了，隧道已回连：{} 个出口测通{}", r.name, back.len(), if back.is_empty() { String::new() } else { format!("（{}）", list(&back)) })).await;
    if !was_down.is_empty() {
        c.warn(format!("软路由「{}」：{} 重启前就不通，重启后还是不通", r.name, was_down.join("、"))).await;
    }
    if !lost.is_empty() {
        return Err(remote(format!(
            "软路由「{}」重启后这些出口测了 3 次都不通（重启前是通的）：{}。OpenClash 已经起来了，配置没动：fleetctl router show 看出口，fleetctl incidents 看告警",
            r.name,
            lost.join("、")
        )));
    }
    Ok(Outcome::Done(json!({"router": {"id": r.id, "name": r.name}, "back": back, "down": was_down, "lost": lost})))
}

/// 改配置行：出口改名 / 来源 IP 分流规则
pub(super) async fn patch(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let rename = c.job.kind == "router_rename";
    let r = online_router(c, "一行都没改").await?;
    let Some(_locks) = lock_or_queue(c, &r).await? else { return Ok(Outcome::Queue(10)) };
    let text = match ops::read_remote(c.st, r.id, r.tunnel_port, &rr::read_script()).await {
        Ok(t) => t,
        Err((255, why)) => return Err(StepError::err(ErrKind::Network, format!("软路由「{}」：读配置失败，{why}；一行都没改", r.name))),
        Err((_, why)) => return Err(remote(format!("软路由「{}」：读配置失败，{why}；一行都没改", r.name))),
    };
    let snap = r.snapshot().unwrap_or_default();
    let redo = if rename { "重新发起改名（fleetctl router rename-exits）" } else { "重新发起（fleetctl router rules）" };
    let (p, checks, map) = if rename {
        let map: Vec<ops::Rename> = serde_json::from_value(c.job.params["map"].clone()).context("作业参数 map 的格式不对")?;
        let (p, sel) = ops::rename_patch(&text, &map).map_err(|e| remote(format!("软路由「{}」：{}。配置没有改动，{redo}", r.name, e.join("；"))))?;
        let checks = ops::Checks {
            remap: map.iter().map(|m| (m.old.clone(), m.new.clone())).collect(),
            select_groups: sel,
            exist: map.iter().map(|m| m.new.clone()).collect(),
            gone: map.iter().filter(|m| !map.iter().any(|x| x.new == m.old)).map(|m| m.old.clone()).collect(),
            ..Default::default()
        };
        (p, checks, map)
    } else {
        let assigns: Vec<ops::Assign> = serde_json::from_value(c.job.params["assign"].clone()).context("作业参数 assign 的格式不对")?;
        let (p, rows) = ops::rules_patch(&text, &assigns).map_err(|e| remote(format!("软路由「{}」：{}。配置没有改动，{redo}", r.name, e.join("；"))))?;
        // 和发起时的预览比：同一个网段这次要做的不一样了（发起之后路由上又加 / 改了规则）就不做
        let key = |v: &Value| (v["cidr"].as_str().unwrap_or("").to_string(), v["op"].as_str().unwrap_or("").to_string(), v["from"].as_str().unwrap_or("").to_string());
        let then: Vec<_> = c.job.params["rows"].as_array().into_iter().flatten().map(key).collect();
        let now: Vec<_> = rows.iter().map(key).collect();
        if !then.is_empty() && then != now && !now.iter().all(|k| k.1 == "same") {
            return Err(remote(format!("软路由「{}」：发起之后路由上的规则又改过，这次要做的和预览不一样，配置没有改动：{redo}", r.name)));
        }
        let proxies: HashSet<&str> = snap.exits.iter().map(|x| x.name.as_str()).collect();
        let changed: Vec<&Value> = rows.iter().filter(|v| v["op"] != "same").collect();
        let mut delay: Vec<String> = changed.iter().filter_map(|v| v["to"].as_str()).filter(|t| proxies.contains(t)).map(String::from).collect();
        delay.dedup();
        let checks = ops::Checks { delay, rules: changed.iter().filter_map(|v| v["cidr"].as_str()).map(String::from).collect(), ..Default::default() };
        (p, checks, vec![])
    };
    if p.edits.is_empty() {
        c.st.routers_refresh.now([r.id]).await;
        return Ok(Outcome::Skipped("路由上的配置已经是这样了，没有改动".into()));
    }
    let t = OffsetDateTime::now_utc();
    let stamp = rr::stamp(t);
    let pins = pinned_stamps(&c.st.db).await?;
    let what = if rename {
        format!("出口改名 {}", map.iter().map(|m| format!("{} → {}", m.old, m.new)).collect::<Vec<_>>().join("，"))
    } else {
        format!("分流规则改 {} 处", p.edits.len())
    };
    c.log(format!("软路由「{}」：{what}（先备份，改完重启 OpenClash 并核对，不行整份换回）", r.name)).await;
    c.st.routers_refresh.claims.editing(r.id, c.job.id, &c.job.kind);
    let res = run_script(c, r.id, &r.name, r.tunnel_port, &ops::patch_script(&p, &checks, t, &pins), "patch", &stamp, rr::APPLY_TIMEOUT).await;
    c.st.routers_refresh.claims.edited(r.id, c.job.id, res.as_ref().ok().and_then(|o| o.kv("sha")));
    let o = match res {
        Ok(o) => o,
        Err(e) => return Err(StepError::err(kind_of(&e), format!("软路由「{}」：{e:#}（可能已经改了：点重试会按路由现在的配置核对，已经改好的不再改）", r.name))),
    };
    if o.code == rr::BUSY {
        c.warn(format!("软路由「{}」：{}，{BUSY_RETRY} 秒后再试", r.name, reason(&o))).await;
        return Ok(Outcome::Wait(BUSY_RETRY));
    }
    if o.code == 255 {
        let why = ssh_failed(c, &r.name, &o).await;
        return Err(StepError::err(o.ssh_kind().unwrap_or(ErrKind::Network), format!("软路由「{}」：{why}（可能已经改了：点重试会按路由现在的配置核对，已经改好的不再改）", r.name)));
    }
    if !o.ok() {
        let undone = if o.kv("rolled_back") == Some("1") { "；已整份换回改之前的配置" } else { "" };
        c.st.routers_refresh.now([r.id]).await;
        return Err(remote(format!("软路由「{}」：{}{undone}", r.name, reason(&o))));
    }
    // 改名：库里按出口名记的（已建立的对应、历史指标）跟着改；已经做过的（重试）不再做
    if rename && c.job.params["renamed_db"] != true {
        let moved = ops::after_rename(&c.st.db, r.id, &map).await?;
        c.set_params(json!({"renamed_db": true})).await?;
        if !moved {
            c.warn(format!("软路由「{}」：新名字下已经有早先别的出口留下的历史指标，出口的历史没有挪到新名字下（fleetctl router exits 里旧名字的历史还在）", r.name)).await;
        }
    }
    c.st.routers_refresh.now([r.id]).await;
    if o.kv("patch") == Some("none") {
        return Ok(Outcome::Skipped("路由上的配置已经是这次要改成的样子（上一次已经改好），没有再改".into()));
    }
    let reselected: Vec<&str> = o.stdout.lines().filter_map(|l| l.strip_prefix("reselected=")).collect();
    c.log(format!(
        "软路由「{}」改好了：{what}；备份 {}{}",
        r.name,
        o.kv("backup").unwrap_or("?"),
        if reselected.is_empty() { String::new() } else { format!("；策略组 {} 按新名字选回了原来的出口", reselected.join("、")) }
    ))
    .await;
    let mut out = json!({"router": {"id": r.id, "name": r.name}, "backup": o.kv("backup"), "edits": p.edits.len(), "reselected": reselected});
    if rename {
        out["renamed"] = json!(map.iter().map(|m| json!({"from": m.old, "to": m.new})).collect::<Vec<_>>());
    } else {
        out["assigned"] = c.job.params["rows"].clone();
    }
    Ok(Outcome::Done(out))
}
