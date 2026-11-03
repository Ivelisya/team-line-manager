//! 穿透入口（crate::reverse）的作业步骤：推节点上的 bridge、切换入口模式并验收、取消时的补偿。
//!
//! 作业 `reverse`（开 / 关两用，参数 `entry_id`、`on`、`from`）：
//! - 开：推 bridge 并启动（带上这个入口）→ 切到穿透（写期望状态）→ 等线路机应用 → 经入口验收出口 IP；
//! - 关：切回普通 → 等应用 → 验收（经节点 ssserver）→ 收起 bridge（节点上不再有穿透入口就停掉删掉）。
//!
//! 切换那一步失败（线路机应用出错、出口验收超时）**自动切回原来的模式**再让作业失败：这是正在服务客户的入口，
//! 不能停在不通的状态等人来点。重试从这一步重新切；取消时补偿把模式改回发起前的、按库里的穿透入口重推 bridge。
//! 上架、加入口、换线路机建的新入口是穿透模式时，在建入口之后推 bridge（步骤 `bridge`），验收照常经入口。

use super::*;
use crate::reverse;

/// 节点上 bridge 的程序与配置
const XRAY_BIN: &str = "/usr/local/bin/xray";

/// 把节点上的 bridge 对齐到 `reverse::tunnels(node, include, exclude)`：有隧道就装上 / 更新，没有时 `remove_if_empty` 才去节点上停掉删掉
/// （否则什么都不做，不登录：普通模式的节点从来没装过）。只删了隧道时写配置不重启（`reverse::only_removed`），
/// 其余变化（加了隧道、参数变了、程序换了）重启 bridge——会断开这台节点上全部经隧道的连接几秒，调用方在日志里说清楚。
/// 返回步骤结果；`None` = 没有隧道也不用删
pub(super) async fn sync_bridge(c: &Ctx<'_>, node: Uuid, include: Option<Uuid>, exclude: Option<Uuid>, remove_if_empty: bool) -> anyhow::Result<Option<Value>> {
    let tunnels = reverse::tunnels(&c.st.db, node, include, exclude).await?;
    if tunnels.is_empty() && !remove_if_empty {
        return Ok(None);
    }
    let m = c.machine_of(node).await?;
    if m.kind != "vps" || m.host_id.is_some() {
        bail!("{}", reverse::kind_unsupported(&m.kind, m.host_id).unwrap_or("这台机器装不了 bridge"));
    }
    let t = c.ssh(&m, None);
    if tunnels.is_empty() {
        let out = t.run(&(vars(&[("ACTION", "remove"), ("CONFIG", "")]) + &scripts::bridge()), Duration::from_secs(60)).await?;
        if !out.ok() {
            return Err(out.fail("收起 bridge 失败"));
        }
        c.log("节点上已经没有穿透入口：bridge 已停掉并删除（xray 程序留着）").await;
        return Ok(Some(json!({"tunnels": 0, "action": "remove"})));
    }
    let cfg = reverse::render(&tunnels, &c.st.cfg.public_url);
    // 先看节点上现在的配置：只删了隧道时不重启
    let cur = t.run(scripts::BRIDGE_READ, Duration::from_secs(30)).await?;
    if !cur.ok() {
        return Err(cur.fail("读节点上的 bridge 配置失败"));
    }
    let old: Option<Value> = cur
        .kv("config")
        .filter(|s| !s.is_empty())
        .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
        .and_then(|raw| serde_json::from_slice(&raw).ok());
    let running = cur.kv("active") == Some("active");
    let quiet = running && old.as_ref().is_some_and(|o| reverse::only_removed(o, &cfg));
    let xray = c.dist("xray");
    if !xray.exists() {
        return Err(StepError::err(ErrKind::Internal, "中控 dist 里没有 xray（/opt/fleet/dist/xray），装不了 bridge：先把 xray 放进 dist（OPS.md §9）"));
    }
    let pushed = t.push(&xray, XRAY_BIN, "0755", Duration::from_secs(300)).await?;
    let action = if quiet && !pushed { "quiet" } else { "restart" };
    let script = vars(&[("ACTION", action), ("CONFIG", &cfg.to_string()), ("BIN_CHANGED", if pushed { "1" } else { "0" })]) + &scripts::bridge();
    let out = t.run(&script, Duration::from_secs(90)).await?;
    if !out.ok() {
        return Err(if out.kv("bridge") == Some("bad_config") { remote(format!("bridge 配置没通过 xray -test：{}", out.stderr.trim())) } else { out.fail("装 bridge 失败") });
    }
    match out.kv("bridge") {
        Some("active") => {}
        Some(s) => return Err(remote(format!("bridge 没起来（{}）：到节点上看 journalctl -u fleet-xray-bridge", if s == "crashing" { "起来就退出、被反复拉起" } else { s }))),
        None => return Err(remote("装 bridge 的脚本没有输出状态")),
    }
    if out.kv("linklocal_dns").is_some() {
        c.warn("节点的 DNS 是链路本地地址：bridge 改用公共解析器（1.1.1.1、8.8.8.8），元数据服务照旧拦着").await;
    }
    let restarted = out.kv("restarted") == Some("1");
    let names: Vec<String> = tunnels.iter().map(|t| format!("{}:{}", t.relay_ip, t.port)).collect();
    c.log(format!(
        "bridge {}：{} 条隧道（{}）{}",
        if restarted { "已启动 / 重启" } else { "已更新配置（没重启）" },
        tunnels.len(),
        names.join("、"),
        if pushed { "，xray 程序已更新" } else { "" }
    ))
    .await;
    Ok(Some(json!({"tunnels": tunnels.len(), "action": action, "restarted": restarted, "xray_updated": pushed, "xray": out.kv("xray_version")})))
}

/// 上架、加入口、换线路机：新入口是穿透模式就推 bridge（节点上已有的隧道一起带上）
pub(super) async fn bridge_for_new(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let eid = c.param_uuid("entry_id")?;
    let mode: Option<String> = sqlx::query_scalar("select mode from entry where id = $1").bind(eid).fetch_optional(&c.st.db).await?;
    if mode.as_deref() != Some(reverse::REVERSE) {
        return Ok(Outcome::Skipped("普通模式的入口，不用 bridge".into()));
    }
    match sync_bridge(c, c.target_id()?, None, None, false).await? {
        Some(v) => Ok(Outcome::Done(v)),
        None => Ok(Outcome::Skipped("没有穿透入口".into())),
    }
}

fn want_on(c: &Ctx<'_>) -> bool { c.job.params["on"] == json!(true) }

/// 作业 `reverse` 的第一步：开启时推 bridge（带上这个入口）并启动；关闭时跳过（先切回普通，最后再收 bridge）
pub(super) async fn bridge_up(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    if !want_on(c) {
        return Ok(Outcome::Skipped("关闭穿透：先切回普通模式、验收通过后再收起 bridge".into()));
    }
    let eid = c.param_uuid("entry_id")?;
    match sync_bridge(c, c.target_id()?, Some(eid), None, false).await? {
        Some(v) => Ok(Outcome::Done(v)),
        None => bail!("入口已经不在了"),
    }
}

/// 作业 `reverse` 的最后一步：关闭时按库里的穿透入口重推 bridge（这个入口已经是普通模式，不在里面；一条都不剩就停掉删掉）；开启时跳过
pub(super) async fn bridge_down(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    if want_on(c) {
        return Ok(Outcome::Skipped("开启穿透：bridge 留着".into()));
    }
    let v = sync_bridge(c, c.target_id()?, None, None, true).await?;
    Ok(Outcome::Done(v.unwrap_or(json!({}))))
}

/// 切换并验收的总时限（步骤超时 600 秒里留出切回去、记日志的余量）：线路机应用通常几秒，出口探测失败时按 30 秒重探
const SWITCH_LIMIT_S: i64 = 420;

/// 作业 `reverse` 的切换：第一次运行时改入口模式（线路机代数 +1），之后等线路机应用、再等应用之后的出口探测通过。
/// 线路机应用出错、到时还没通过：切回原来的模式再失败（见模块注释）。重试时从头切
pub(super) async fn switch(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let eid = c.param_uuid("entry_id")?;
    let (to, from) = if want_on(c) { (reverse::REVERSE, reverse::NORMAL) } else { (reverse::NORMAL, reverse::REVERSE) };
    if c.first {
        // 上一次开启失败、切回时已经把这条隧道从 bridge 上撤了（见 revert）：重试先推回去
        if want_on(c) && c.job.params["reverted"] == json!(true) && sync_bridge(c, c.target_id()?, Some(eid), None, false).await?.is_none() {
            bail!("入口已经不在了");
        }
        let mut tx = c.st.db.begin().await?;
        let (relay, gen) = reverse::set_mode(&mut tx, eid, to).await.map_err(|e| anyhow!(e.message))?;
        let n = sqlx::query("update job set params = (params - 'applied_at' - 'reverted') || $2 where id = $1 and lease_owner = $3")
            .bind(c.job.id)
            .bind(json!({"switch_gen": gen}))
            .bind(c.owner)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if n == 0 {
            bail!("作业租约已不属于本执行者，没有切换");
        }
        tx.commit().await?;
        c.st.relay_bumped(relay);
        c.st.emit(Event::Machines);
        c.log(format!("入口切到{}模式：线路机期望状态代数 {gen}（只换这个入口的入站，Xray 不重启）", reverse::mode_zh(to))).await;
        return Ok(Outcome::Wait(EVENT_POLL_S));
    }
    let (mode, lc, err, applied): (String, String, Option<String>, bool) = sqlx::query_as(
        "select e.mode, e.lifecycle::text, e.apply_error, r.applied_generation >= e.changed_gen
         from entry e join relay r on r.machine_id = e.relay_id where e.id = $1",
    )
    .bind(eid)
    .fetch_one(&c.st.db)
    .await?;
    if mode != to {
        bail!("入口的模式在作业进行中被改成了「{}」，不再接着验收：确认之后重新发起", reverse::mode_zh(&mode));
    }
    let elapsed = (OffsetDateTime::now_utc() - c.started).whole_seconds();
    if let Some(e) = err {
        return Err(revert(c, eid, from, &format!("线路机应用失败：{e}")).await);
    }
    if !applied {
        if elapsed > SWITCH_LIMIT_S {
            return Err(revert(c, eid, from, &format!("等了 {SWITCH_LIMIT_S} 秒线路机还没应用（线路机离线？）")).await);
        }
        return Ok(Outcome::Wait(EVENT_POLL_S));
    }
    // 线路机已应用：从这一刻起的出口探测才算新路径（之前的可能还是旧路径的结果）
    let applied_at = match c.job.params["applied_at"].as_str().and_then(|s| OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()) {
        Some(t) => t,
        None => {
            let now = OffsetDateTime::now_utc();
            c.set_params(json!({"applied_at": now.format(&time::format_description::well_known::Rfc3339)?})).await?;
            c.log(format!("线路机已应用（入口{}），等经入口的出口探测", entries::state_zh(&lc))).await;
            now
        }
    };
    let row: Option<(bool, Option<String>)> =
        sqlx::query_as("select ok, detail from probe_result where entry_id = $1 and kind = 'egress' and ts > $2 order by ts desc limit 1")
            .bind(eid)
            .bind(applied_at)
            .fetch_optional(&c.st.db)
            .await?;
    match row {
        Some((true, ip)) => {
            c.log(format!("出口验收通过：经入口出去的是 {}（{}模式）", ip.as_deref().unwrap_or("?"), reverse::mode_zh(to))).await;
            // 关穿透验收通过：bridge 端口不再有用（线路机的入站已只听入口端口），清掉让出这个端口
            if to == reverse::NORMAL {
                sqlx::query("update entry set bridge_port = null where id = $1 and mode = 'normal'").bind(eid).execute(&c.st.db).await?;
            }
            c.st.emit(Event::Machines);
            Ok(Outcome::Done(json!({"mode": to, "egress_ip": ip})))
        }
        Some((false, detail)) if elapsed > SWITCH_LIMIT_S => {
            let why = if to == reverse::REVERSE {
                format!("出口探测一直没通过（{}）：多半是 bridge 连不上线路机入口（节点出站被挡？），到节点上看 journalctl -u fleet-xray-bridge", detail.unwrap_or_default())
            } else {
                format!("出口探测一直没通过（{}）：多半是线路机连不上节点 SS 端口（节点防火墙、供应商丢入站）", detail.unwrap_or_default())
            };
            Err(revert(c, eid, from, &why).await)
        }
        None if elapsed > SWITCH_LIMIT_S => Err(revert(c, eid, from, "线路机一直没有报出口探测").await),
        Some((false, detail)) => {
            if c.job.params["probe_warned"] != json!(true) {
                c.warn(format!("出口探测还没通过（{}），继续等（隧道刚建、探测失败后 30 秒重探）", detail.unwrap_or_default())).await;
                c.set_params(json!({"probe_warned": true})).await?;
            }
            Ok(Outcome::Wait(10))
        }
        None => Ok(Outcome::Wait(5)),
    }
}

/// 切换失败：改回原来的模式（线路机代数 +1）。开启失败时把这条隧道从 bridge 上撤掉（尽力而为）：入口回到普通模式后线路机不再认
/// bridge 的 UUID，bridge 每 2 秒试连一次、线路机的访问日志每次记一行拒绝；重试时 `switch` 先推回去。返回作业的错误
async fn revert(c: &Ctx<'_>, eid: Uuid, from: &str, why: &str) -> anyhow::Error {
    let r = async {
        let mut tx = c.st.db.begin().await?;
        let (relay, gen) = reverse::set_mode(&mut tx, eid, from).await.map_err(|e| anyhow!(e.message))?;
        tx.commit().await?;
        c.st.relay_bumped(relay);
        c.st.emit(Event::Machines);
        anyhow::Ok(gen)
    }
    .await;
    if r.is_ok() && from == reverse::NORMAL {
        let pulled = match c.target_id() {
            Ok(node) => sync_bridge(c, node, None, None, true).await.map(|_| ()),
            Err(e) => Err(e),
        };
        match pulled {
            Ok(()) => {
                let _ = c.set_params(json!({"reverted": true})).await;
            }
            Err(e) => c.warn(format!("把这条隧道从 bridge 上撤掉没成功（不影响客户，取消作业时会再收一次）：{e:#}")).await,
        }
    }
    match r {
        Ok(gen) => {
            c.warn(format!("已自动切回{}模式（线路机代数 {gen}），客户照旧经原来的路径", reverse::mode_zh(from))).await;
            StepError::err(ErrKind::Remote, format!("{why}；已自动切回{}模式。查明原因后点重试（从这一步重新切换），不要了就取消", reverse::mode_zh(from)))
        }
        Err(e) => StepError::err(ErrKind::Remote, format!("{why}；自动切回{}模式也失败了（{e:#}）：fleetctl entry reverse 手动切回", reverse::mode_zh(from))),
    }
}

/// 取消补偿：入口改回发起前的模式（已经是就不动），bridge 按库里的穿透入口重推（尽力而为）
pub(super) async fn compensate(c: &Ctx<'_>) {
    let (st, job) = (c.st, c.job.id);
    let Ok(eid) = c.param_uuid("entry_id") else { return };
    let from = if want_on(c) { reverse::NORMAL } else { reverse::REVERSE };
    let cur: Option<String> = note(st, job, "读取入口", sqlx::query_scalar("select mode from entry where id = $1 and lifecycle <> 'removed'").bind(eid).fetch_optional(&st.db).await).await.flatten();
    if cur.as_deref().is_some_and(|m| m != from) {
        if let Some(mut tx) = note(st, job, "开启事务", st.db.begin().await).await {
            match reverse::set_mode(&mut tx, eid, from).await {
                Ok((relay, gen)) => {
                    if note(st, job, "提交切回", tx.commit().await).await.is_some() {
                        st.relay_bumped(relay);
                        log(st, job, "warn", format!("已切回{}模式（线路机代数 {gen}）", reverse::mode_zh(from))).await;
                    }
                }
                Err(e) => log(st, job, "warn", format!("切回原来的模式：{}", e.message)).await,
            }
        }
    }
    // 取消的是开启穿透：入口回到普通模式，开启时记下的 bridge 端口作废（取消的是关闭穿透则入口回到穿透，端口留着）
    if want_on(c) {
        note(st, job, "清 bridge 端口", sqlx::query("update entry set bridge_port = null where id = $1 and mode = 'normal'").bind(eid).execute(&st.db).await).await;
    }
    // 动过 bridge 的（开启时推过、关闭时收到一半）：按库里此刻的穿透入口重推（没有了就停掉删掉）
    let pushed: bool = note(
        st,
        job,
        "读取步骤",
        sqlx::query_scalar("select exists (select 1 from job_step where job_id = $1 and key in ('bridge_up', 'bridge_down') and state in ('done', 'running', 'failed'))")
            .bind(job)
            .fetch_one(&st.db)
            .await,
    )
    .await
    .unwrap_or(true);
    if pushed {
        if let Ok(node) = c.target_id() {
            note(st, job, "重推 bridge", sync_bridge(c, node, None, None, true).await).await;
        }
    }
    st.emit(Event::Machines);
}
