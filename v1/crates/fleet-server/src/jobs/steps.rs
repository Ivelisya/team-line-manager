//! 各作业步骤的实现。每步幂等：重跑不会重复产生副作用。

use super::scripts::{self, vars};
use super::{log, JobRow, Outcome};
use crate::crypto::{random_token, sha256};
use crate::entries;
use crate::ssh::{self, Target};
use crate::state::AppState;
use anyhow::{anyhow, bail, Context};
use base64::Engine;
use serde_json::{json, Value};
use std::time::Duration;
use time::OffsetDateTime;
use uuid::Uuid;

pub struct Ctx<'a> {
    pub st: &'a AppState,
    pub job: &'a JobRow,
    /// 本步第一次开始的时间
    pub started: OffsetDateTime,
}

#[derive(sqlx::FromRow)]
struct M {
    id: Uuid,
    kind: String,
    ip: String,
    ssh_port: i32,
    lifecycle: String,
}

impl Ctx<'_> {
    fn target_id(&self) -> anyhow::Result<Uuid> { self.job.target_id.ok_or_else(|| anyhow!("作业没有目标机器")) }

    async fn machine(&self) -> anyhow::Result<M> {
        Ok(sqlx::query_as("select id, kind::text kind, host(primary_ip) ip, ssh_port, lifecycle::text lifecycle from machine where id = $1")
            .bind(self.target_id()?)
            .fetch_one(&self.st.db)
            .await?)
    }

    fn ssh(&self, m: &M, password: Option<String>) -> Target {
        Target {
            ip: m.ip.clone(),
            port: m.ssh_port as u16,
            key: self.st.cfg.ssh_key.clone(),
            known_hosts: self.st.cfg.known_hosts.clone(),
            password,
        }
    }

    fn param_uuid(&self, k: &str) -> anyhow::Result<Uuid> {
        self.job.params[k].as_str().and_then(|s| Uuid::try_parse(s).ok()).ok_or_else(|| anyhow!("作业参数缺少 {k}"))
    }

    /// 合并写入作业参数（jsonb ||）
    async fn set_params(&self, patch: Value) -> anyhow::Result<()> {
        sqlx::query("update job set params = params || $2 where id = $1").bind(self.job.id).bind(patch).execute(&self.st.db).await?;
        Ok(())
    }

    fn password(&self) -> anyhow::Result<Option<String>> {
        let Some(b64) = self.job.params["login"]["password_enc"].as_str() else { return Ok(None) };
        let raw = base64::engine::general_purpose::STANDARD.decode(b64)?;
        Ok(Some(self.st.secrets.open_str(&raw)?))
    }

    async fn log(&self, msg: impl AsRef<str>) { log(self.st, self.job.id, "info", msg).await }
    async fn warn(&self, msg: impl AsRef<str>) { log(self.st, self.job.id, "warn", msg).await }

    fn dist(&self, name: &str) -> std::path::PathBuf { self.st.cfg.dist_dir.join(name) }
}

pub async fn run(c: &Ctx<'_>, key: &str) -> anyhow::Result<Outcome> {
    match key {
        "preflight" => preflight(c).await,
        "bootstrap" => bootstrap(c).await,
        "converge" => converge_node(c).await,
        "converge_relay" => converge_relay(c).await,
        "agent" => agent(c).await,
        "wait_report" => wait_report(c).await,
        "attach" => attach(c).await,
        "firewall" => firewall(c).await,
        "wait_applied" => wait_applied(c).await,
        "verify" => verify(c).await,
        "activate" => activate(c).await,
        "detach" => detach(c).await,
        "wait_detached" => wait_detached(c).await,
        "cleanup" => cleanup(c).await,
        "revoke" => revoke(c).await,
        "finalize" => finalize(c).await,
        "purge" => purge(c).await,
        _ => bail!("未知步骤 {key}"),
    }
}

// ── 上架 ──────────────────────────────────────────────────────────────────

async fn preflight(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let m = c.machine().await?;
    let mut up = false;
    for _ in 0..3 {
        if ssh::tcp_ok(&m.ip, m.ssh_port as u16, Duration::from_secs(5)).await {
            up = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    if !up {
        bail!("{}:{} 的 SSH 端口连不上", m.ip, m.ssh_port);
    }
    let fp = ssh::pin_host_key(&c.st.cfg.known_hosts, &m.ip, m.ssh_port as u16).await?;
    c.log(format!("已记录主机公钥 {}", fp.split_whitespace().next().unwrap_or(""))).await;
    let mut out = c.ssh(&m, None).run(scripts::FACTS, Duration::from_secs(30)).await?;
    if !out.ok() {
        let pw = c.password()?.ok_or_else(|| anyhow!("中控密钥登录不了，也没有提供 root 密码：{}", out.stderr.trim()))?;
        out = c.ssh(&m, Some(pw)).run(scripts::FACTS, Duration::from_secs(30)).await?;
        if !out.ok() {
            bail!("用密码也登录不了：{}", out.stderr.trim());
        }
    }
    let arch = out.kv("arch").unwrap_or("");
    if arch != "x86_64" {
        bail!("只支持 x86_64，这台是 {arch}");
    }
    if out.kv("systemd") != Some("1") {
        bail!("这台机器没有运行 systemd");
    }
    if let Some(other) = out.kv("agent_machine").filter(|s| !s.is_empty()) {
        if other != m.id.to_string() {
            bail!("这台机器上的 agent 登记的是另一台机器（{other}）：先确认是不是在用的节点");
        }
    }
    if m.kind == "vps" && out.kv("port8388") == Some("busy") {
        c.warn("8388 端口已经有程序在监听；收敛时会被 fleet-ssserver 接管").await;
    }
    Ok(Outcome::Done(json!({
        "host_key": fp, "arch": arch, "os": out.kv("os"), "kernel": out.kv("kernel"), "mem_mb": out.kv("mem_mb"),
    })))
}

async fn bootstrap(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let m = c.machine().await?;
    let key_ok = c.ssh(&m, None).run("true", Duration::from_secs(20)).await.map(|o| o.ok()).unwrap_or(false);
    if key_ok {
        // 有密码也立刻丢掉
        c.set_params(json!({"login": null})).await?;
        return Ok(Outcome::Skipped("中控密钥已经能登录".into()));
    }
    let pw = c.password()?.ok_or_else(|| anyhow!("中控密钥登录不了，也没有提供 root 密码"))?;
    let pubkey = tokio::fs::read_to_string(c.st.cfg.ssh_key.with_extension("pub")).await.context("读取中控公钥")?;
    let script = vars(&[("PUBKEY", pubkey.trim())]) + scripts::BOOTSTRAP_KEY;
    let out = c.ssh(&m, Some(pw)).run(&script, Duration::from_secs(40)).await?;
    if !out.stdout.contains("bootstrap=ok") {
        bail!("推送公钥失败：{}", out.stderr.trim());
    }
    if !c.ssh(&m, None).run("true", Duration::from_secs(20)).await?.ok() {
        bail!("公钥已写入，但密钥登录仍失败");
    }
    c.set_params(json!({"login": null})).await?;
    c.warn("引导密码已用过并丢弃；建议在供应商面板重置 root 密码").await;
    Ok(Outcome::Done(json!({"method": "password"})))
}

/// 节点所挂线路机的 IP（含本次要挂的）
async fn relay_ips_for(c: &Ctx<'_>, node: Uuid, extra: Option<Uuid>) -> anyhow::Result<String> {
    let ips: Vec<(String,)> = sqlx::query_as(
        "select distinct host(m.primary_ip) from machine m
         where m.id in (select relay_id from entry where node_id = $1 and lifecycle <> 'removed') or m.id = $2",
    )
    .bind(node)
    .bind(extra)
    .fetch_all(&c.st.db)
    .await?;
    if ips.is_empty() {
        bail!("没有可放行的线路机 IP");
    }
    Ok(ips.into_iter().map(|(s,)| s).collect::<Vec<_>>().join(", "))
}

async fn converge_node(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let m = c.machine().await?;
    let t = c.ssh(&m, None);
    let base = t.run(scripts::BASELINE, Duration::from_secs(600)).await?;
    if !base.ok() {
        bail!("系统基线失败：{}", base.stderr.trim());
    }
    if base.kv("sshd_password") != Some("no") {
        bail!("sshd 仍允许密码登录（{}）", base.kv("sshd_password").unwrap_or("?"));
    }
    if base.kv("cc") != Some("bbr") {
        c.warn(format!("拥塞控制是 {}，没能切到 BBR（容器里属正常）", base.kv("cc").unwrap_or("?"))).await;
    }
    let pushed = t.push(&c.dist("ssserver"), "/usr/local/bin/ssserver", "0755", Duration::from_secs(180)).await?;
    let (method, secret): (Option<String>, Vec<u8>) = sqlx::query_as("select method, secret_enc from node_outbound where machine_id = $1")
        .bind(m.id)
        .fetch_one(&c.st.db)
        .await?;
    let password = c.st.secrets.open_str(&secret)?;
    let allow = relay_ips_for(c, m.id, c.param_uuid("relay_id").ok()).await?;
    let script = vars(&[
        ("SS_PORT", "8388"),
        ("SS_METHOD", method.as_deref().unwrap_or("aes-256-gcm")),
        ("SS_PASSWORD", &password),
        ("ALLOW", &allow),
    ]) + scripts::NODE_SS;
    let out = t.run(&script, Duration::from_secs(120)).await?;
    if !out.ok() || out.kv("ss") != Some("listening") {
        bail!("ssserver 没有起来：{} {}", out.stdout.trim(), out.stderr.trim());
    }
    if pushed {
        t.run("systemctl restart fleet-ssserver", Duration::from_secs(30)).await?;
    }
    Ok(Outcome::Done(json!({"bbr": base.kv("cc") == Some("bbr"), "nft_relays": out.kv("nft_relays"), "ssserver_updated": pushed})))
}

async fn converge_relay(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let m = c.machine().await?;
    let t = c.ssh(&m, None);
    let base = t.run(scripts::BASELINE, Duration::from_secs(600)).await?;
    if !base.ok() || base.kv("sshd_password") != Some("no") {
        bail!("系统基线失败：{}", base.stderr.trim());
    }
    t.push(&c.dist("xray"), "/usr/local/bin/xray", "0755", Duration::from_secs(300)).await?;
    for f in ["geoip.dat", "geosite.dat"] {
        t.push(&c.dist(f), &format!("/usr/local/share/xray/{f}"), "0644", Duration::from_secs(300)).await?;
    }
    let out = t.run(&scripts::relay_xray(), Duration::from_secs(60)).await?;
    if !out.ok() {
        bail!("安装 fleet-xray 失败：{}", out.stderr.trim());
    }
    Ok(Outcome::Done(json!({"xray": out.kv("xray_version"), "bbr": base.kv("cc") == Some("bbr")})))
}

async fn agent(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let m = c.machine().await?;
    let t = c.ssh(&m, None);
    let pushed = t.push(&c.dist("fleet-agent"), "/usr/local/bin/fleet-agent", "0755", Duration::from_secs(180)).await?;
    // 每次都签发新令牌并撤销这台机器的旧令牌：令牌只绑 machine_id，不会波及别的机器
    let token = format!("fa1_{}", random_token(32));
    let mut tx = c.st.db.begin().await?;
    sqlx::query("update agent_token set revoked_at = now() where machine_id = $1 and revoked_at is null").bind(m.id).execute(&mut *tx).await?;
    sqlx::query("insert into agent_token (id, machine_id, token_hash) values ($1, $2, $3)")
        .bind(crate::db::new_id())
        .bind(m.id)
        .bind(sha256(token.as_bytes()))
        .execute(&mut *tx)
        .await?;
    // 先提交：agent 一启动就会用新令牌上报
    tx.commit().await?;
    let script = vars(&[
        ("SERVER_URL", &c.st.cfg.public_url),
        ("ROLE", if m.kind == "relay" { "relay" } else { "node" }),
        ("MACHINE_ID", &m.id.to_string()),
        ("TOKEN", &token),
        ("BIN_CHANGED", if pushed { "1" } else { "0" }),
    ]) + &scripts::agent();
    let out = t.run(&script, Duration::from_secs(120)).await?;
    if !out.ok() || out.kv("agent") != Some("active") {
        bail!("agent 没有起来：{} {}", out.stdout.trim(), out.stderr.trim());
    }
    Ok(Outcome::Done(json!({"binary_updated": pushed})))
}

async fn wait_report(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let id = c.target_id()?;
    let row: Option<(OffsetDateTime, Option<String>)> = sqlx::query_as(
        "select a.last_report_at, a.agent_version from agent_state a
         where a.machine_id = $1 and a.last_report_at >= (select started_at from job_step where job_id = $2 and key = 'agent')",
    )
    .bind(id)
    .bind(c.job.id)
    .fetch_optional(&c.st.db)
    .await?
    .and_then(|(t, v): (Option<OffsetDateTime>, Option<String>)| t.map(|t| (t, v)));
    match row {
        Some((t, v)) => Ok(Outcome::Done(json!({"first_report_at": t.to_string(), "agent_version": v}))),
        None => Ok(Outcome::Wait(3)),
    }
}

async fn attach(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    if let Ok(e) = c.param_uuid("entry_id") {
        return Ok(Outcome::Skipped(format!("入口已建（{e}）")));
    }
    let node = c.target_id()?;
    let relay = c.param_uuid("relay_id")?;
    let mut tx = c.st.db.begin().await?;
    let (eid, port, gen) = entries::create(&mut tx, relay, node).await.map_err(|e| anyhow!(e.message))?;
    sqlx::query("update job set params = params || $2 where id = $1")
        .bind(c.job.id)
        .bind(json!({"entry_id": eid}))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    c.st.relay_bumped(relay);
    Ok(Outcome::Done(json!({"entry_id": eid, "port": port, "generation": gen})))
}

async fn firewall(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let m = c.machine().await?;
    if m.kind != "vps" {
        return Ok(Outcome::Skipped("上游代理没有防火墙要改".into()));
    }
    let allow = relay_ips_for(c, m.id, None).await?;
    let out = c.ssh(&m, None).run(&(vars(&[("SS_PORT", "8388"), ("ALLOW", &allow)]) + scripts::NODE_NFT_ONLY), Duration::from_secs(60)).await?;
    if !out.ok() {
        bail!("更新防火墙失败：{}", out.stderr.trim());
    }
    Ok(Outcome::Done(json!({"nft_relays": out.kv("nft_relays")})))
}

async fn wait_applied(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    if let Ok(eid) = c.param_uuid("entry_id") {
        let (lc, err, applied): (String, Option<String>, bool) = sqlx::query_as(
            "select e.lifecycle::text, e.apply_error, r.applied_generation >= e.changed_gen
             from entry e join relay r on r.machine_id = e.relay_id where e.id = $1",
        )
        .bind(eid)
        .fetch_one(&c.st.db)
        .await?;
        if let Some(e) = err {
            bail!("线路机应用失败：{e}");
        }
        return Ok(if applied && lc == "active" { Outcome::Done(json!({"entry": lc})) } else { Outcome::Wait(2) });
    }
    // 新线路机：等第一次应用
    let id = c.target_id()?;
    let (g, a, err): (i64, i64, Option<String>) =
        sqlx::query_as("select generation, applied_generation, apply_error from relay where machine_id = $1").bind(id).fetch_one(&c.st.db).await?;
    if let Some(e) = err {
        bail!("Xray 配置应用失败：{e}");
    }
    Ok(if a >= g { Outcome::Done(json!({"generation": a})) } else { Outcome::Wait(2) })
}

async fn verify(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let eid = c.param_uuid("entry_id")?;
    let row: Option<(bool, Option<f32>, Option<String>)> = sqlx::query_as(
        "select ok, latency_ms, detail from probe_result where entry_id = $1 and kind = 'egress' and ts >= $2 order by ts desc limit 1",
    )
    .bind(eid)
    .bind(c.started)
    .fetch_optional(&c.st.db)
    .await?;
    match row {
        Some((true, lat, detail)) => Ok(Outcome::Done(json!({"latency_ms": lat, "egress_ip": detail}))),
        Some((false, _, detail)) => {
            c.warn(format!("出口探测未通过：{}（继续等待重试）", detail.unwrap_or_default())).await;
            Ok(Outcome::Wait(10))
        }
        None => Ok(Outcome::Wait(5)),
    }
}

async fn activate(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let id = c.target_id()?;
    sqlx::query(
        "update machine set lifecycle = 'active', activated_at = coalesce(activated_at, now()), lifecycle_reason = null,
            revision = revision + 1, updated_at = now() where id = $1 and lifecycle = 'provisioning'",
    )
    .bind(id)
    .execute(&c.st.db)
    .await?;
    Ok(Outcome::Done(json!({})))
}

// ── 退役 ──────────────────────────────────────────────────────────────────

async fn detach(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let id = c.target_id()?;
    let es: Vec<(Uuid,)> = sqlx::query_as(
        "select id from entry where (node_id = $1 or relay_id = $1) and lifecycle not in ('removing', 'removed')",
    )
    .bind(id)
    .fetch_all(&c.st.db)
    .await?;
    let mut relays = vec![];
    let mut tx = c.st.db.begin().await?;
    for (e,) in &es {
        let (r, _) = entries::transition(&mut tx, *e, "removing", false).await.map_err(|e| anyhow!(e.message))?;
        relays.push(r);
    }
    tx.commit().await?;
    relays.sort();
    relays.dedup();
    for r in &relays {
        c.st.relay_bumped(*r);
    }
    Ok(Outcome::Done(json!({"entries": es.len()})))
}

async fn wait_detached(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let id = c.target_id()?;
    let left: Vec<(String, i32, Option<OffsetDateTime>)> = sqlx::query_as(
        "select rm.label, e.port, a.last_report_at from entry e join machine rm on rm.id = e.relay_id
         left join agent_state a on a.machine_id = e.relay_id
         where (e.node_id = $1 or e.relay_id = $1) and e.lifecycle <> 'removed'",
    )
    .bind(id)
    .fetch_all(&c.st.db)
    .await?;
    if left.is_empty() {
        return Ok(Outcome::Done(json!({})));
    }
    // 线路机本身在退役（摘的是自己身上的入口）：agent 会照常应用空配置
    let now = OffsetDateTime::now_utc();
    for (relay, port, seen) in &left {
        if seen.is_none_or(|t| (now - t).whole_seconds() > 120) && (now - c.started).whole_seconds() > 60 {
            bail!("线路机 {relay} 不在线，入口 {relay}:{port} 摘不掉；线路机恢复后点重试");
        }
    }
    Ok(Outcome::Wait(2))
}

async fn cleanup(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let m = c.machine().await?;
    if m.kind == "proxy" {
        return Ok(Outcome::Skipped("上游代理没有要清理的服务".into()));
    }
    if c.job.params["cleanup"] == json!(false) {
        return Ok(Outcome::Skipped("选择了不清理机器".into()));
    }
    if !ssh::tcp_ok(&m.ip, m.ssh_port as u16, Duration::from_secs(5)).await {
        return Ok(Outcome::Skipped("机器连不上，跳过清理（令牌照样撤销）".into()));
    }
    let script = if m.kind == "relay" { scripts::RELAY_CLEANUP } else { scripts::NODE_CLEANUP };
    match c.ssh(&m, None).run(script, Duration::from_secs(120)).await {
        Ok(o) if o.stdout.contains("cleanup=done") => Ok(Outcome::Done(json!({"ss": o.kv("ss")}))),
        Ok(o) => Ok(Outcome::Skipped(format!("清理没有完成：{}", o.stderr.trim()))),
        Err(e) => Ok(Outcome::Skipped(format!("登录失败，跳过清理：{e}"))),
    }
}

async fn revoke(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let n = sqlx::query("update agent_token set revoked_at = now() where machine_id = $1 and revoked_at is null")
        .bind(c.target_id()?)
        .execute(&c.st.db)
        .await?
        .rows_affected();
    Ok(Outcome::Done(json!({"revoked": n})))
}

async fn finalize(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let m = c.machine().await?;
    let mut tx = c.st.db.begin().await?;
    sqlx::query(
        "update machine set lifecycle = 'retired', retired_at = now(), revision = revision + 1, updated_at = now() where id = $1",
    )
    .bind(m.id)
    .execute(&mut *tx)
    .await?;
    // 未结告警直接关闭（不发恢复通知）
    sqlx::query(
        "update incident set state = 'resolved', resolved_at = now(), detail = detail || '{\"closed_by\":\"retire\"}'
         where state = 'firing' and (subject_id = $1 or subject_id in (select id from entry where node_id = $1 or relay_id = $1))",
    )
    .bind(m.id)
    .execute(&mut *tx)
    .await?;
    // 秘密不再需要
    sqlx::query("delete from node_outbound where machine_id = $1").bind(m.id).execute(&mut *tx).await?;
    // 编号连续：同团队剩下的节点压缩成 01..N
    let mut renamed = vec![];
    let mut relays = vec![];
    if c.st.cfg.numbering.auto_compact {
        let team: Option<(Option<Uuid>,)> = sqlx::query_as("select team_id from machine where id = $1").bind(m.id).fetch_optional(&mut *tx).await?;
        if let Some((Some(t),)) = team {
            let (ch, rs) = crate::db::compact_team(&mut tx, t, &c.job.created_by, "节点退役后自动压缩").await?;
            renamed = ch;
            relays = rs;
        }
    }
    tx.commit().await?;
    for r in relays {
        c.st.relay_bumped(r);
    }
    if !renamed.is_empty() {
        c.log(format!("编号已压缩：{} 台节点改了编号（见审计）", renamed.len())).await;
    }
    ssh::forget_host(&c.st.cfg.known_hosts, &m.ip, m.ssh_port as u16).await?;
    Ok(Outcome::Done(json!({"renumbered": renamed.len()})))
}

async fn purge(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let id = c.target_id()?;
    let mut tx = c.st.db.begin().await?;
    let (lc,): (String,) = sqlx::query_as("select lifecycle::text from machine where id = $1 for update").bind(id).fetch_one(&mut *tx).await?;
    if lc != "retired" {
        bail!("机器不是已退役状态");
    }
    let es: Vec<(Uuid,)> = sqlx::query_as("select id from entry where node_id = $1 or relay_id = $1").bind(id).fetch_all(&mut *tx).await?;
    let eids: Vec<Uuid> = es.into_iter().map(|(e,)| e).collect();
    let mut n = 0u64;
    for t in ["entry_traffic", "probe_result", "access_event", "entry_traffic_hourly", "probe_hourly"] {
        n += sqlx::query(&format!("delete from {t} where entry_id = any($1)")).bind(&eids).execute(&mut *tx).await?.rows_affected();
    }
    for t in ["host_metric", "host_metric_hourly"] {
        n += sqlx::query(&format!("delete from {t} where machine_id = $1")).bind(id).execute(&mut *tx).await?.rows_affected();
    }
    sqlx::query("delete from incident where subject_id = $1 or subject_id = any($2)").bind(id).bind(&eids).execute(&mut *tx).await?;
    sqlx::query("delete from silence where subject_id = $1").bind(id).execute(&mut *tx).await?;
    sqlx::query("delete from entry where id = any($1)").bind(&eids).execute(&mut *tx).await?;
    // 其余（令牌、agent 状态、账单、续费、出站、线路机参数）随 machine 级联删除
    sqlx::query("delete from machine where id = $1").bind(id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Outcome::Done(json!({"rows_deleted": n, "entries": eids.len()})))
}

// ── 取消时的补偿 ──────────────────────────────────────────────────────────

/// 上架/加入口被取消：摘掉已建的入口；上架的机器直接退役（释放 IP 与编号）。尽力而为，错误只记日志。
pub async fn compensate(st: &AppState, job: &JobRow) {
    let c = Ctx { st, job, started: OffsetDateTime::now_utc() };
    if let Ok(eid) = c.param_uuid("entry_id") {
        let mut tx = match st.db.begin().await {
            Ok(t) => t,
            Err(_) => return,
        };
        match entries::transition(&mut tx, eid, "removing", false).await {
            Ok((r, _)) => {
                let _ = tx.commit().await;
                st.relay_bumped(r);
                log(st, job.id, "warn", "已摘除本次建的入口").await;
            }
            Err(e) => log(st, job.id, "warn", format!("摘除入口：{}", e.message)).await,
        }
    }
    if !job.kind.starts_with("provision_") {
        return;
    }
    let Some(id) = job.target_id else { return };
    if let Ok(m) = c.machine().await {
        if m.lifecycle == "provisioning" {
            if m.kind != "proxy" && ssh::tcp_ok(&m.ip, m.ssh_port as u16, Duration::from_secs(5)).await {
                let script = if m.kind == "relay" { scripts::RELAY_CLEANUP } else { scripts::NODE_CLEANUP };
                if let Err(e) = c.ssh(&m, None).run(script, Duration::from_secs(120)).await {
                    log(st, job.id, "warn", format!("清理机器失败（忽略）：{e}")).await;
                }
            }
            let _ = sqlx::query("update agent_token set revoked_at = now() where machine_id = $1 and revoked_at is null").bind(id).execute(&st.db).await;
            let _ = sqlx::query(
                "update machine set lifecycle = 'retired', retired_at = now(), lifecycle_reason = '上架取消', revision = revision + 1 where id = $1",
            )
            .bind(id)
            .execute(&st.db)
            .await;
            let _ = sqlx::query("delete from node_outbound where machine_id = $1").bind(id).execute(&st.db).await;
            let _ = ssh::forget_host(&st.cfg.known_hosts, &m.ip, m.ssh_port as u16).await;
            if st.cfg.numbering.auto_compact {
                let team: Option<(Option<Uuid>,)> = sqlx::query_as("select team_id from machine where id = $1").bind(id).fetch_optional(&st.db).await.ok().flatten();
                if let Some((Some(t),)) = team {
                    if let Ok(mut tx) = st.db.begin().await {
                        if let Ok((_, relays)) = crate::db::compact_team(&mut tx, t, &job.created_by, "上架取消后自动压缩").await {
                            if tx.commit().await.is_ok() {
                                for r in relays {
                                    st.relay_bumped(r);
                                }
                            }
                        }
                    }
                }
            }
            log(st, job.id, "warn", "上架已取消：机器标记为已退役，IP 与编号已释放").await;
        }
    }
}
