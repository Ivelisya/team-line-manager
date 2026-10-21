//! 各作业步骤的实现。每步幂等：重跑不会重复产生副作用。

mod lifecycle;

use super::scripts::{self, vars};
use super::{log, JobRow, Outcome};
use crate::crypto::{random_token, sha256};
use crate::entries;
use crate::ssh::{self, ErrKind, Output, StepError, Target};
use crate::state::AppState;
use anyhow::{anyhow, bail, Context};
use base64::Engine;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::net::IpAddr;
use std::time::Duration;
use time::OffsetDateTime;
use uuid::Uuid;

pub struct Ctx<'a> {
    pub st: &'a AppState,
    pub job: &'a JobRow,
    /// 本执行者名：对 job 的写入都要求 lease_owner 是它
    pub owner: &'a str,
    /// 本步第一次开始的时间
    pub started: OffsetDateTime,
    /// 本步是第一次运行（含重试重置之后的第一次）；等待后重新领取时为 false
    pub first: bool,
}

#[derive(sqlx::FromRow)]
struct M {
    id: Uuid,
    kind: String,
    ip: String,
    ssh_port: i32,
    lifecycle: String,
}

/// 远端脚本/配置层面的失败
fn remote(msg: impl Into<String>) -> anyhow::Error { StepError::err(ErrKind::Remote, msg) }

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

    /// 合并写入作业参数（jsonb ||）；租约不在手里就报错（写入被拒）
    async fn set_params(&self, patch: Value) -> anyhow::Result<()> {
        let n = sqlx::query("update job set params = params || $2 where id = $1 and lease_owner = $3")
            .bind(self.job.id)
            .bind(patch)
            .bind(self.owner)
            .execute(&self.st.db)
            .await?
            .rows_affected();
        if n == 0 {
            bail!("作业租约已不属于本执行者，参数没有写入");
        }
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
        // 换线路机：能复用的步骤直接复用（防火墙、等应用、出口验收），其余在 lifecycle 子模块
        "attach_new" => lifecycle::attach_new(c).await,
        "firewall_widen" => firewall(c).await,
        "wait_applied_new" => wait_applied(c).await,
        "verify_new" => verify(c).await,
        "detach_old" => lifecycle::detach_old(c).await,
        "wait_detached_old" => lifecycle::wait_detached_old(c).await,
        "firewall_narrow" => lifecycle::firewall_narrow(c).await,
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
        return Err(StepError::err(ErrKind::Network, format!("{}:{} 的 SSH 端口连不上", m.ip, m.ssh_port)));
    }
    // 该 IP 已有记录且指纹变了：失败，除非作业参数明确允许重置（机器重装过）
    let allow_reset = c.job.params["reset_host_key"].as_bool().unwrap_or(false);
    let fp = ssh::pin_host_key(&c.st.cfg.known_hosts, &m.ip, m.ssh_port as u16, allow_reset).await?;
    c.log(format!("主机公钥已记录/核对 {}", ssh::fingerprint(&fp))).await;
    let mut out = c.ssh(&m, None).run(scripts::FACTS, Duration::from_secs(30)).await?;
    if !out.ok() {
        // 只有「密钥登录被拒」才改用密码；连接/指纹类的失败直接报，不要拿密码去试
        if out.ssh_kind() != Some(ErrKind::Auth) {
            return Err(out.fail("登录失败"));
        }
        let pw = c
            .password()?
            .ok_or_else(|| StepError::err(ErrKind::Auth, format!("中控密钥登录不了，也没有提供 root 密码：{}", out.stderr.trim())))?;
        out = c.ssh(&m, Some(pw)).run(scripts::FACTS, Duration::from_secs(30)).await?;
        if !out.ok() {
            return Err(out.fail("用密码也登录不了"));
        }
    }
    let arch = out.kv("arch").unwrap_or("");
    if arch != "x86_64" {
        return Err(remote(format!("只支持 x86_64，这台是 {arch}")));
    }
    if out.kv("systemd") != Some("1") {
        return Err(remote("这台机器没有运行 systemd"));
    }
    if let Some(other) = out.kv("agent_machine").filter(|s| !s.is_empty()) {
        if other != m.id.to_string() {
            return Err(remote(format!("这台机器上的 agent 登记的是另一台机器（{other}）：先确认是不是在用的节点")));
        }
    }
    if m.kind == "vps" && out.kv("port8388") == Some("busy") {
        c.warn("8388 端口已经有程序在监听；收敛时会被 fleet-ssserver 接管").await;
    }
    Ok(Outcome::Done(json!({
        "host_key": ssh::fingerprint(&fp), "arch": arch, "os": out.kv("os"), "kernel": out.kv("kernel"), "mem_mb": out.kv("mem_mb"),
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
    let pw = c.password()?.ok_or_else(|| StepError::err(ErrKind::Auth, "中控密钥登录不了，也没有提供 root 密码"))?;
    let pubkey = tokio::fs::read_to_string(c.st.cfg.ssh_key.with_extension("pub")).await.context("读取中控公钥")?;
    let script = vars(&[("PUBKEY", pubkey.trim())]) + scripts::BOOTSTRAP_KEY;
    let out = c.ssh(&m, Some(pw)).run(&script, Duration::from_secs(40)).await?;
    if !out.stdout.contains("bootstrap=ok") {
        return Err(out.fail("推送公钥失败"));
    }
    if !c.ssh(&m, None).run("true", Duration::from_secs(20)).await?.ok() {
        return Err(StepError::err(ErrKind::Auth, "公钥已写入，但密钥登录仍失败"));
    }
    c.set_params(json!({"login": null})).await?;
    c.warn("引导密码已用过并丢弃；建议在供应商面板重置 root 密码").await;
    Ok(Outcome::Done(json!({"method": "password"})))
}

/// 节点所挂线路机的 IP（含本次要挂的），逗号分隔、按字符串排序。nft 放行集合是 ipv4_addr，遇到 IPv6 直接报错。
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
    let mut v = vec![];
    for (s,) in ips {
        match s.parse::<IpAddr>() {
            Ok(IpAddr::V4(_)) => v.push(s),
            _ => bail!("线路机 IP「{s}」不是 IPv4：节点防火墙的放行集合目前只支持 IPv4 线路机"),
        }
    }
    v.sort();
    Ok(v.join(", "))
}

fn ip_set(s: &str) -> BTreeSet<String> { s.split(',').map(str::trim).filter(|x| !x.is_empty()).map(String::from).collect() }

/// 校验节点防火墙脚本的输出：规则加载成功，且放行集合恰好等于期望的线路机 IP
fn check_nft(out: &Output, allow: &str) -> anyhow::Result<()> {
    match out.kv("nft") {
        Some("missing") => return Err(remote("节点上没有防火墙放行文件（/etc/fleet/nft/ss_guard.nft），这台节点还没收敛过？")),
        Some("error") => return Err(remote(format!("nft 规则校验或加载失败：{}", out.stderr.trim()))),
        _ => {}
    }
    let (want, got) = (ip_set(allow), ip_set(out.kv("nft_relays").unwrap_or("")));
    if want != got {
        return Err(remote(format!(
            "节点防火墙放行名单和期望不一致：期望 [{}]，实际 [{}]",
            want.iter().cloned().collect::<Vec<_>>().join(", "),
            got.iter().cloned().collect::<Vec<_>>().join(", ")
        )));
    }
    Ok(())
}

async fn converge_node(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let m = c.machine().await?;
    let t = c.ssh(&m, None);
    let base = t.run(scripts::BASELINE, Duration::from_secs(600)).await?;
    if !base.ok() {
        return Err(base.fail("系统基线失败"));
    }
    if base.kv("sshd_password") != Some("no") {
        return Err(remote(format!("sshd 仍允许密码登录（{}）", base.kv("sshd_password").unwrap_or("?"))));
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
    if !out.ok() {
        return Err(out.fail("配置 ssserver / 防火墙失败"));
    }
    check_nft(&out, &allow)?;
    if out.kv("ss") != Some("listening") {
        return Err(remote(format!("ssserver 没有起来：{}", out.stdout.trim())));
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
    if !base.ok() {
        return Err(base.fail("系统基线失败"));
    }
    if base.kv("sshd_password") != Some("no") {
        return Err(remote(format!("sshd 仍允许密码登录（{}）", base.kv("sshd_password").unwrap_or("?"))));
    }
    t.push(&c.dist("xray"), "/usr/local/bin/xray", "0755", Duration::from_secs(300)).await?;
    for f in ["geoip.dat", "geosite.dat"] {
        t.push(&c.dist(f), &format!("/usr/local/share/xray/{f}"), "0644", Duration::from_secs(300)).await?;
    }
    let out = t.run(&scripts::relay_xray(), Duration::from_secs(60)).await?;
    if !out.ok() {
        return Err(out.fail("安装 fleet-xray 失败"));
    }
    Ok(Outcome::Done(json!({"xray": out.kv("xray_version"), "bbr": base.kv("cc") == Some("bbr")})))
}

/// 装 agent：先签发新令牌装上，**不**撤销旧令牌；等 wait_report 看到新令牌真的上报了，才撤销这台机器的其它令牌。
/// 这样安装中途失败时旧 agent 还能继续上报。
async fn agent(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let m = c.machine().await?;
    let t = c.ssh(&m, None);
    let pushed = t.push(&c.dist("fleet-agent"), "/usr/local/bin/fleet-agent", "0755", Duration::from_secs(180)).await?;
    let token = format!("fa1_{}", random_token(32));
    let token_id = crate::db::new_id();
    sqlx::query("insert into agent_token (id, machine_id, token_hash) values ($1, $2, $3)")
        .bind(token_id)
        .bind(m.id)
        .bind(sha256(token.as_bytes()))
        .execute(&c.st.db)
        .await?;
    // 记下这枚新令牌，wait_report 用它判断「新 agent 已经在用新令牌上报」
    c.set_params(json!({"agent_token_id": token_id})).await?;
    let script = vars(&[
        ("SERVER_URL", &c.st.cfg.public_url),
        ("ROLE", if m.kind == "relay" { "relay" } else { "node" }),
        ("MACHINE_ID", &m.id.to_string()),
        ("TOKEN", &token),
        ("BIN_CHANGED", if pushed { "1" } else { "0" }),
    ]) + &scripts::agent();
    let out = t.run(&script, Duration::from_secs(120)).await?;
    if !out.ok() || out.kv("agent") != Some("active") {
        return Err(remote(format!("agent 没有起来：{} {}", out.stdout.trim(), out.stderr.trim())));
    }
    Ok(Outcome::Done(json!({"binary_updated": pushed, "token_id": token_id})))
}

async fn wait_report(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let id = c.target_id()?;
    let tid = c.param_uuid("agent_token_id").ok();
    // 上报要晚于「装 agent」那一步开始；且有新令牌时，新令牌必须被用过（排除旧 agent 用旧令牌的上报）
    let row: Option<(OffsetDateTime, Option<String>)> = sqlx::query_as(
        "select a.last_report_at, a.agent_version from agent_state a
         where a.machine_id = $1 and a.last_report_at >= (select started_at from job_step where job_id = $2 and key = 'agent')
           and ($3::uuid is null or exists (select 1 from agent_token t where t.id = $3 and t.machine_id = $1 and t.revoked_at is null and t.last_used_at is not null))",
    )
    .bind(id)
    .bind(c.job.id)
    .bind(tid)
    .fetch_optional(&c.st.db)
    .await?
    .and_then(|(t, v): (Option<OffsetDateTime>, Option<String>)| t.map(|t| (t, v)));
    match row {
        Some((t, v)) => {
            let mut revoked = 0;
            if let Some(tid) = tid {
                revoked = sqlx::query("update agent_token set revoked_at = now() where machine_id = $1 and id <> $2 and revoked_at is null")
                    .bind(id)
                    .bind(tid)
                    .execute(&c.st.db)
                    .await?
                    .rows_affected();
                if revoked > 0 {
                    c.log(format!("新令牌已上报，撤销了 {revoked} 枚旧令牌")).await;
                }
            }
            Ok(Outcome::Done(json!({"first_report_at": crate::access::fmt_ts(t), "agent_version": v, "old_tokens_revoked": revoked})))
        }
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
    // 记 entry_id 时同时确认租约还在：不在就整个事务回滚，不留下孤儿入口
    let n = sqlx::query("update job set params = params || $2 where id = $1 and lease_owner = $3")
        .bind(c.job.id)
        .bind(json!({"entry_id": eid}))
        .bind(c.owner)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if n == 0 {
        bail!("作业租约已不属于本执行者，入口没有创建");
    }
    tx.commit().await?;
    c.st.relay_bumped(relay);
    Ok(Outcome::Done(json!({"entry_id": eid, "port": port, "generation": gen})))
}

async fn firewall(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let m = c.machine().await?;
    if m.kind != "vps" {
        return Ok(Outcome::Skipped("家宽代理没有防火墙要改".into()));
    }
    let allow = relay_ips_for(c, m.id, None).await?;
    let out = c.ssh(&m, None).run(&(vars(&[("SS_PORT", "8388"), ("ALLOW", &allow)]) + scripts::NODE_NFT_ONLY), Duration::from_secs(60)).await?;
    if !out.ok() {
        // 脚本在 nft=missing / nft=error 时以非 0 退出
        check_nft(&out, &allow)?;
        return Err(out.fail("更新防火墙失败"));
    }
    check_nft(&out, &allow)?;
    Ok(Outcome::Done(json!({"nft_relays": out.kv("nft_relays")})))
}

/// 重试时让线路机重新应用一次：代数 +1，清掉上次留下的应用错误（否则一进来又立刻判失败）。
async fn rebump(c: &Ctx<'_>, relay_id: Uuid, entry: Option<Uuid>) -> anyhow::Result<()> {
    let mut tx = c.st.db.begin().await?;
    let gen = crate::db::bump_relay(&mut tx, relay_id).await?;
    sqlx::query("update relay set apply_error = null where machine_id = $1").bind(relay_id).execute(&mut *tx).await?;
    if let Some(e) = entry {
        sqlx::query("update entry set apply_error = null, changed_gen = $2 where id = $1 and lifecycle in ('pending', 'active', 'disabled')")
            .bind(e)
            .bind(gen)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    c.st.relay_bumped(relay_id);
    c.log(format!("重试：线路机期望状态代数提到 {gen}，让 agent 重新应用")).await;
    Ok(())
}

async fn wait_applied(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    if let Ok(eid) = c.param_uuid("entry_id") {
        let (relay_id, lc, err, applied): (Uuid, String, Option<String>, bool) = sqlx::query_as(
            "select e.relay_id, e.lifecycle::text, e.apply_error, r.applied_generation >= e.changed_gen
             from entry e join relay r on r.machine_id = e.relay_id where e.id = $1",
        )
        .bind(eid)
        .fetch_one(&c.st.db)
        .await?;
        if let Some(e) = err {
            // 本步第一次运行就带着上次的应用错误（重试）：只重新应用一次，不是每轮都 bump
            if c.first {
                rebump(c, relay_id, Some(eid)).await?;
                return Ok(Outcome::Wait(2));
            }
            return Err(remote(format!("线路机应用失败：{e}")));
        }
        return Ok(if applied && lc == "active" { Outcome::Done(json!({"entry": lc})) } else { Outcome::Wait(2) });
    }
    // 新线路机：等第一次应用
    let id = c.target_id()?;
    let (g, a, err): (i64, i64, Option<String>) =
        sqlx::query_as("select generation, applied_generation, apply_error from relay where machine_id = $1").bind(id).fetch_one(&c.st.db).await?;
    if let Some(e) = err {
        if c.first {
            rebump(c, id, None).await?;
            return Ok(Outcome::Wait(2));
        }
        return Err(remote(format!("Xray 配置应用失败：{e}")));
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
    if c.job.kind.starts_with("restore_") {
        // 恢复已退役的机器：清掉退役时间，原因记「已恢复：…」，历史、账单、审计都挂在原来的机器 id 上
        let reason = match c.job.params["reason"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
            Some(r) => format!("已恢复：{r}"),
            None => "已恢复".to_string(),
        };
        sqlx::query(
            "update machine set lifecycle = 'active', activated_at = coalesce(activated_at, now()), retired_at = null, lifecycle_reason = $2,
                revision = revision + 1, updated_at = now() where id = $1 and lifecycle = 'provisioning'",
        )
        .bind(id)
        .bind(&reason)
        .execute(&c.st.db)
        .await?;
        c.log("机器已恢复上线").await;
        return Ok(Outcome::Done(json!({"restored": true, "reason": reason})));
    }
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
            return Err(StepError::err(ErrKind::Network, format!("线路机 {relay} 不在线，入口 {relay}:{port} 摘不掉；线路机恢复后点重试")));
        }
    }
    Ok(Outcome::Wait(2))
}

async fn cleanup(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let m = c.machine().await?;
    if m.kind == "proxy" {
        return Ok(Outcome::Skipped("家宽代理没有要清理的服务".into()));
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

/// 按天分批删分区大表里这台机器（和它名下入口）的原始数据：每天一条语句，走 (entry_id/machine_id, ts) 索引，
/// 单条语句只碰一个分区，不会一个事务里锁很久。窗口覆盖整个保留期加前后几天；幂等。
async fn purge_partitioned(c: &Ctx<'_>, machine: Uuid, eids: &[Uuid]) -> anyhow::Result<u64> {
    let ret = &c.st.cfg.retention;
    let back = ret.raw_days.max(ret.access_days) as i64 + 2;
    let today = OffsetDateTime::now_utc().date();
    let mut n = 0u64;
    for i in -back..=3 {
        let d = today + time::Duration::days(i);
        let (lo, hi) = (d.midnight().assume_utc(), (d + time::Duration::days(1)).midnight().assume_utc());
        for t in ["entry_traffic", "probe_result", "access_event"] {
            n += sqlx::query(&format!("delete from {t} where entry_id = any($1) and ts >= $2 and ts < $3")).bind(eids).bind(lo).bind(hi).execute(&c.st.db).await?.rows_affected();
        }
        n += sqlx::query("delete from host_metric where machine_id = $1 and ts >= $2 and ts < $3").bind(machine).bind(lo).bind(hi).execute(&c.st.db).await?.rows_affected();
    }
    Ok(n)
}

async fn purge(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let id = c.target_id()?;
    // 先不加锁地确认状态，再按天分批删大表（每天一条独立语句，不在一个大事务里锁着机器行）；
    // 机器已经不在（上一次跑到一半进程挂了、或重复发起）：目标已达成，直接算完成
    let Some((lc,)) = sqlx::query_as::<_, (String,)>("select lifecycle::text from machine where id = $1").bind(id).fetch_optional(&c.st.db).await? else {
        return Ok(Outcome::Done(json!({"already_gone": true})));
    };
    if lc != "retired" {
        bail!("机器不是已退役状态");
    }
    let es: Vec<(Uuid,)> = sqlx::query_as("select id from entry where node_id = $1 or relay_id = $1").bind(id).fetch_all(&c.st.db).await?;
    let eids: Vec<Uuid> = es.into_iter().map(|(e,)| e).collect();
    let mut n = purge_partitioned(c, id, &eids).await?;

    // 收尾事务：剩下的小表（小时汇总、访问汇总）和机器本身。分区大表在上面已按天清掉。
    let mut tx = c.st.db.begin().await?;
    let Some((lc,)) = sqlx::query_as::<_, (String,)>("select lifecycle::text from machine where id = $1 for update").bind(id).fetch_optional(&mut *tx).await? else {
        return Ok(Outcome::Done(json!({"already_gone": true})));
    };
    if lc != "retired" {
        bail!("机器不是已退役状态");
    }
    for t in ["entry_traffic_hourly", "probe_hourly"] {
        n += sqlx::query(&format!("delete from {t} where entry_id = any($1)")).bind(&eids).execute(&mut *tx).await?.rows_affected();
    }
    n += sqlx::query("delete from host_metric_hourly where machine_id = $1").bind(id).execute(&mut *tx).await?.rows_affected();
    // 访问汇总：access_hourly 按节点或线路机，其余三张按节点（线路机自己没有这三张的行）
    n += sqlx::query("delete from access_hourly where node_id = $1 or relay_id = $1").bind(id).execute(&mut *tx).await?.rows_affected();
    for t in ["access_source_hourly", "access_target_seen", "access_source_late"] {
        n += sqlx::query(&format!("delete from {t} where node_id = $1")).bind(id).execute(&mut *tx).await?.rows_affected();
    }
    sqlx::query("delete from incident where subject_id = $1 or subject_id = any($2)").bind(id).bind(&eids).execute(&mut *tx).await?;
    sqlx::query("delete from silence where subject_id = $1").bind(id).execute(&mut *tx).await?;
    sqlx::query("delete from entry where id = any($1)").bind(&eids).execute(&mut *tx).await?;
    // 其余（令牌、agent 状态、账单、续费、出站、线路机参数）随 machine 级联删除
    sqlx::query("delete from machine where id = $1").bind(id).execute(&mut *tx).await?;
    tx.commit().await?;
    // 小时流量删了：对端机器的流量 / 周期已用缓存跟着作废
    c.st.live.rolled_up();
    Ok(Outcome::Done(json!({"rows_deleted": n, "entries": eids.len()})))
}

// ── 取消时的补偿 ──────────────────────────────────────────────────────────

/// 尽力而为的一步：出错只记日志（不再静默吞掉），返回是否成功
async fn note<T, E: std::fmt::Display>(st: &AppState, job: Uuid, what: &str, r: Result<T, E>) -> Option<T> {
    match r {
        Ok(v) => Some(v),
        Err(e) => {
            log(st, job, "warn", format!("补偿「{what}」失败（已忽略）：{e}")).await;
            None
        }
    }
}

/// 摘掉作业建的入口（补偿用，尽力而为）
async fn remove_created_entry(st: &AppState, job: Uuid, eid: Uuid) {
    if let Some(mut tx) = note(st, job, "开启事务", st.db.begin().await).await {
        match entries::transition(&mut tx, eid, "removing", false).await {
            Ok((r, _)) => {
                if note(st, job, "提交摘除入口", tx.commit().await).await.is_some() {
                    st.relay_bumped(r);
                    log(st, job, "warn", "已摘除本次建的入口").await;
                }
            }
            Err(e) => log(st, job, "warn", format!("摘除入口：{}", e.message)).await,
        }
    }
}

/// 上架/恢复/加入口/换线路机被取消：摘掉已建的入口；上架的机器直接退役，恢复的机器回到已退役（都释放 IP 与编号）。
/// 尽力而为，错误记日志。取消后不再需要保存的 root 密码，由调用方在置 cancelled 时一并清掉。
pub async fn compensate(st: &AppState, job: &JobRow, owner: &str) {
    let c = Ctx { st, job, owner, started: OffsetDateTime::now_utc(), first: false };
    if job.kind == "move_entry" {
        return lifecycle::compensate_move(&c).await;
    }
    if let Ok(eid) = c.param_uuid("entry_id") {
        remove_created_entry(st, job.id, eid).await;
    }
    let restoring = job.kind.starts_with("restore_");
    if !job.kind.starts_with("provision_") && !restoring {
        return;
    }
    let Some(id) = job.target_id else { return };
    let Some(m) = note(st, job.id, "读取机器", c.machine().await).await else { return };
    if m.lifecycle != "provisioning" {
        return;
    }
    if m.kind != "proxy" && ssh::tcp_ok(&m.ip, m.ssh_port as u16, Duration::from_secs(5)).await {
        let script = if m.kind == "relay" { scripts::RELAY_CLEANUP } else { scripts::NODE_CLEANUP };
        note(st, job.id, "清理机器上的服务", c.ssh(&m, None).run(script, Duration::from_secs(120)).await).await;
    }
    note(
        st,
        job.id,
        "撤销令牌",
        sqlx::query("update agent_token set revoked_at = now() where machine_id = $1 and revoked_at is null").bind(id).execute(&st.db).await,
    )
    .await;
    // 压缩编号要用「取消前」这台机器占着编号的团队（恢复取消会把团队改回去，所以先读）
    let team = note(
        st,
        job.id,
        "读取团队",
        sqlx::query_as::<_, (Option<Uuid>,)>("select team_id from machine where id = $1").bind(id).fetch_optional(&st.db).await,
    )
    .await
    .flatten();
    if restoring {
        // 回到恢复之前的样子：原团队、原编号、原名、原退役原因（退役时间恢复期间一直没动过）
        let prev = &job.params["prev"];
        let prev_team = prev["team_id"].as_str().and_then(|s| Uuid::try_parse(s).ok());
        let prev_number = prev["number"].as_i64().map(|n| n as i32);
        note(
            st,
            job.id,
            "机器回到已退役",
            sqlx::query(
                "update machine set lifecycle = 'retired', lifecycle_reason = $2, team_id = coalesce($3, team_id), number = coalesce($4, number),
                    label = coalesce($5, label), revision = revision + 1, updated_at = now() where id = $1",
            )
            .bind(id)
            .bind(prev["reason"].as_str())
            .bind(prev_team)
            .bind(prev_number)
            .bind(prev["label"].as_str())
            .execute(&st.db)
            .await,
        )
        .await;
    } else {
        note(
            st,
            job.id,
            "机器标记已退役",
            sqlx::query("update machine set lifecycle = 'retired', retired_at = now(), lifecycle_reason = '上架取消', revision = revision + 1 where id = $1")
                .bind(id)
                .execute(&st.db)
                .await,
        )
        .await;
    }
    note(st, job.id, "删除出站秘密", sqlx::query("delete from node_outbound where machine_id = $1").bind(id).execute(&st.db).await).await;
    note(st, job.id, "忘记主机公钥", ssh::forget_host(&st.cfg.known_hosts, &m.ip, m.ssh_port as u16).await).await;
    if st.cfg.numbering.auto_compact {
        if let Some((Some(t),)) = team {
            if let Some(mut tx) = note(st, job.id, "开启事务", st.db.begin().await).await {
                if let Some((_, relays)) = note(st, job.id, "压缩编号", crate::db::compact_team(&mut tx, t, &job.created_by, if restoring { "恢复取消后自动压缩" } else { "上架取消后自动压缩" }).await).await {
                    if note(st, job.id, "提交编号压缩", tx.commit().await).await.is_some() {
                        for r in relays {
                            st.relay_bumped(r);
                        }
                    }
                }
            }
        }
    }
    let msg = if restoring { "恢复已取消：机器回到已退役，IP 与编号已释放" } else { "上架已取消：机器标记为已退役，IP 与编号已释放" };
    log(st, job.id, "warn", msg).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(stdout: &str) -> Output { Output { code: 0, stdout: stdout.into(), stderr: String::new() } }

    #[test]
    fn nft_check_compares_sets() {
        // 顺序无关
        assert!(check_nft(&out("nft_relays=2.2.2.2,1.1.1.1\n"), "1.1.1.1, 2.2.2.2").is_ok());
        // 少了一个
        assert!(check_nft(&out("nft_relays=1.1.1.1\n"), "1.1.1.1, 2.2.2.2").is_err());
        // 多了一个
        assert!(check_nft(&out("nft_relays=1.1.1.1,3.3.3.3\n"), "1.1.1.1").is_err());
        // 没输出
        assert!(check_nft(&out(""), "1.1.1.1").is_err());
        // 脚本自己报的失败
        assert!(check_nft(&out("nft=missing\n"), "1.1.1.1").is_err());
        assert!(check_nft(&out("nft=error\nnft_relays=1.1.1.1\n"), "1.1.1.1").is_err());
    }
}
