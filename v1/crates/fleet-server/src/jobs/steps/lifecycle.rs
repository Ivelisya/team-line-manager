//! 换线路机（move_entry）专用的步骤与补偿。
//!
//! 顺序：新线路机建入口 → 放行 → 等生效 → 经新入口验收 → 摘旧入口 → 等旧线路机摘除 → 收紧防火墙。
//! 新入口验收通过之前旧入口一直不动，所以节点始终有一个可用入口；摘旧入口是不可回滚的一步。
//! 恢复已退役机器（restore_*）直接复用上架的步骤，差别只在 activate 与 compensate（见父模块）。

use super::*;

/// (旧入口状态, 线路机 id, 线路机名, 端口, 线路机应用错误, 线路机最近上报)
type OldEntryRow = (String, Uuid, String, i32, Option<String>, Option<OffsetDateTime>);

/// 在目标线路机上建新入口（新端口、新 client_uuid / short_id，pending）。幂等：已建过就跳过。
pub(super) async fn attach_new(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    if let Ok(e) = c.param_uuid("entry_id") {
        return Ok(Outcome::Skipped(format!("新入口已建（{e}）")));
    }
    let node = c.target_id()?;
    let to = c.param_uuid("to_relay_id")?;
    let old = c.param_uuid("old_entry_id")?;
    let mut tx = c.st.db.begin().await?;
    // 先拿策略共享锁再锁旧入口行（和发布的加锁顺序一致）
    entries::lock_policy_shared(&mut tx).await.map_err(|e| anyhow!(e.message))?;
    let cur: Option<(String,)> = sqlx::query_as("select lifecycle::text from entry where id = $1 and node_id = $2 for update")
        .bind(old)
        .bind(node)
        .fetch_optional(&mut *tx)
        .await?;
    match cur.as_ref().map(|(l,)| l.as_str()) {
        Some("active") => {}
        Some(l) => bail!("旧入口现在是「{}」，不能再换线路机（旧作业不会自动复活，请重新发起）", entries::state_zh(l)),
        None => bail!("旧入口已经不存在"),
    }
    let dup: Option<(Uuid,)> = sqlx::query_as("select id from entry where node_id = $1 and relay_id = $2 and lifecycle <> 'removed'")
        .bind(node)
        .bind(to)
        .fetch_optional(&mut *tx)
        .await?;
    if dup.is_some() {
        bail!("这个节点在目标线路机上已经有入口");
    }
    let (eid, port, gen) = entries::create(&mut tx, to, node).await.map_err(|e| anyhow!(e.message))?;
    // 记 entry_id 时同时确认租约还在：不在就整个事务回滚，不留下孤儿入口
    let n = sqlx::query("update job set params = params || $2 where id = $1 and lease_owner = $3")
        .bind(c.job.id)
        .bind(json!({"entry_id": eid, "new_entry_id": eid}))
        .bind(c.owner)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if n == 0 {
        bail!("作业租约已不属于本执行者，入口没有创建");
    }
    tx.commit().await?;
    c.st.relay_bumped(to);
    let (label,): (String,) = sqlx::query_as("select coalesce(label, '') from machine where id = $1").bind(to).fetch_one(&c.st.db).await?;
    c.log(format!("新入口已建：{label}:{port}（等线路机应用）；旧入口保持不动，换线期间不断网")).await;
    Ok(Outcome::Done(json!({"entry_id": eid, "relay_id": to, "relay": label, "port": port, "generation": gen})))
}

/// 摘旧入口（removing，旧线路机代数 +1）。不可回滚，所以：作业正在取消、新入口还没生效时都不动手。幂等。
pub(super) async fn detach_old(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let old = c.param_uuid("old_entry_id")?;
    let new = c.param_uuid("entry_id")?;
    let state: String = sqlx::query_scalar("select state::text from job where id = $1").bind(c.job.id).fetch_one(&c.st.db).await?;
    if state == "cancelling" {
        bail!("作业正在取消，不再摘除旧入口");
    }
    let nl: Option<String> = sqlx::query_scalar("select lifecycle::text from entry where id = $1").bind(new).fetch_optional(&c.st.db).await?;
    if nl.as_deref() != Some("active") {
        bail!("新入口现在是「{}」，还不能摘旧入口", nl.as_deref().map_or("不存在", entries::state_zh));
    }
    let cur: Option<String> = sqlx::query_scalar("select lifecycle::text from entry where id = $1").bind(old).fetch_optional(&c.st.db).await?;
    match cur.as_deref() {
        None => return Ok(Outcome::Skipped("旧入口已经不存在".into())),
        Some("removing" | "removed") => return Ok(Outcome::Skipped("旧入口已经在摘除中".into())),
        Some(_) => {}
    }
    let mut tx = c.st.db.begin().await?;
    let (relay, gen) = entries::transition(&mut tx, old, "removing", false).await.map_err(|e| anyhow!(e.message))?;
    tx.commit().await?;
    c.st.relay_bumped(relay);
    c.warn("旧入口已开始摘除：旧的分享链接即将失效，客户端需要换成新的分享链接").await;
    Ok(Outcome::Done(json!({"old_entry_id": old, "relay_id": relay, "generation": gen})))
}

/// 等旧线路机回报已摘除（旧入口 removed）。旧线路机不在线就失败，恢复后重试。
pub(super) async fn wait_detached_old(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let old = c.param_uuid("old_entry_id")?;
    let row: Option<OldEntryRow> = sqlx::query_as(
        "select e.lifecycle::text, e.relay_id, rm.label, e.port, r.apply_error, a.last_report_at
         from entry e join machine rm on rm.id = e.relay_id join relay r on r.machine_id = e.relay_id
         left join agent_state a on a.machine_id = e.relay_id where e.id = $1",
    )
    .bind(old)
    .fetch_optional(&c.st.db)
    .await?;
    let Some((lc, relay_id, relay, port, err, seen)) = row else { return Ok(Outcome::Done(json!({"already_gone": true}))) };
    if lc == "removed" {
        return Ok(Outcome::Done(json!({"old_entry_id": old})));
    }
    if let Some(e) = err {
        // 本步第一次运行就带着上次的应用错误（重试）：只重新应用一次
        if c.first {
            rebump(c, relay_id, None).await?;
            return Ok(Outcome::Wait(EVENT_POLL_S));
        }
        return Err(remote(format!("旧线路机 {relay} 应用失败：{e}")));
    }
    let now = OffsetDateTime::now_utc();
    if seen.is_none_or(|t| (now - t).whole_seconds() > 120) && (now - c.started).whole_seconds() > 60 {
        return Err(StepError::err(ErrKind::Network, format!("旧线路机 {relay} 不在线，入口 {relay}:{port} 摘不掉；线路机恢复后点重试")));
    }
    Ok(Outcome::Wait(EVENT_POLL_S))
}

/// 收紧节点防火墙：旧线路机上不再有这个节点的入口时，把旧线路机 IP 从放行名单拿掉（proxy 没有防火墙，直接跳过）。
pub(super) async fn firewall_narrow(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let node = c.target_id()?;
    let from = c.param_uuid("from_relay_id")?;
    let (left,): (i64,) = sqlx::query_as("select count(*) from entry where node_id = $1 and relay_id = $2 and lifecycle <> 'removed'")
        .bind(node)
        .bind(from)
        .fetch_one(&c.st.db)
        .await?;
    let out = if left > 0 { Outcome::Skipped("节点在旧线路机上还有入口，防火墙保持放行".into()) } else { firewall(c).await? };
    let new = c.param_uuid("entry_id")?;
    let row: Option<(String, i32)> = sqlx::query_as("select coalesce(m.label, ''), e.port from entry e join machine m on m.id = e.relay_id where e.id = $1")
        .bind(new)
        .fetch_optional(&c.st.db)
        .await?;
    if let Some((relay, port)) = row {
        c.log(format!("换线路机完成：新入口 {relay}:{port}。客户端需要换成新的分享链接（旧链接已失效）")).await;
    }
    Ok(out)
}

/// 补偿后节点还应该放行的线路机 IP：只算还活着的入口（不含 removing / removed）
async fn live_relay_ips(c: &Ctx<'_>, node: Uuid) -> anyhow::Result<String> {
    let ips: Vec<(String,)> = sqlx::query_as(
        "select distinct host(m.primary_ip) from machine m
         where m.id in (select relay_id from entry where node_id = $1 and lifecycle in ('pending', 'active', 'disabled'))",
    )
    .bind(node)
    .fetch_all(&c.st.db)
    .await?;
    let mut v: Vec<String> = ips.into_iter().map(|(s,)| s).filter(|s| matches!(s.parse::<IpAddr>(), Ok(IpAddr::V4(_)))).collect();
    if v.is_empty() {
        bail!("没有可放行的线路机 IP");
    }
    v.sort();
    Ok(v.join(", "))
}

/// 换线路机被取消：移除新入口、恢复防火墙，旧入口保持不动。
/// 旧入口已经开始摘除时（取消赶在摘除之后到达）不回滚——回滚会让节点没有入口，保留新入口。
pub(super) async fn compensate_move(c: &Ctx<'_>) {
    let (st, job) = (c.st, c.job);
    let Ok(new) = c.param_uuid("entry_id") else {
        log(st, job.id, "warn", "还没有建新入口，无需补偿；旧入口保持不动").await;
        return;
    };
    let old_lc: Option<String> = match c.param_uuid("old_entry_id") {
        Ok(o) => note(st, job.id, "读取旧入口", sqlx::query_scalar("select lifecycle::text from entry where id = $1").bind(o).fetch_optional(&st.db).await)
            .await
            .flatten(),
        Err(_) => None,
    };
    if !matches!(old_lc.as_deref(), Some("active" | "disabled" | "pending")) {
        log(st, job.id, "warn", "旧入口已经开始摘除，不回滚：保留新入口（客户端需要换成新的分享链接）").await;
        return;
    }
    remove_created_entry(st, job.id, new).await;
    // 防火墙：放行步骤跑过的话，按剩下的入口把名单收回去
    let Some(node) = job.target_id else { return };
    let widened: Option<String> =
        note(st, job.id, "读取放行步骤", sqlx::query_scalar("select state from job_step where job_id = $1 and key = 'firewall_widen'").bind(job.id).fetch_optional(&st.db).await)
            .await
            .flatten();
    if widened.as_deref().is_some_and(|s| s != "pending") {
        if let Some(m) = note(st, job.id, "读取节点", c.machine().await).await {
            if m.kind == "vps" && ssh::tcp_ok(&m.ip, m.ssh_port as u16, Duration::from_secs(5)).await {
                if let Some(allow) = note(st, job.id, "计算放行名单", live_relay_ips(c, node).await).await {
                    let script = vars(&[("SS_PORT", "8388"), ("ALLOW", &allow)]) + scripts::NODE_NFT_ONLY;
                    match c.ssh(&m, None).run(&script, Duration::from_secs(60)).await {
                        Ok(out) => match check_nft(&out, &allow) {
                            Ok(()) => log(st, job.id, "info", format!("节点防火墙已恢复为 [{allow}]")).await,
                            Err(e) => log(st, job.id, "warn", format!("补偿「恢复防火墙」失败（已忽略）：{e:#}")).await,
                        },
                        Err(e) => log(st, job.id, "warn", format!("补偿「恢复防火墙」失败（已忽略）：{e:#}")).await,
                    }
                }
            }
        }
    }
    log(st, job.id, "warn", "换线路机已取消：新入口已摘除，旧入口保持不动").await;
}
