//! 各作业步骤的实现。每步幂等：重跑不会重复产生副作用。

mod lifecycle;
mod replace;

use super::scripts::{self, vars, SsInst};
use super::{clip, log, tune, JobRow, Outcome};
use crate::crypto::{random_token, sha256};
use crate::entries;
use crate::ssh::{self, kind_of, ErrKind, Output, StepError, Target};
use crate::state::{AppState, Event};
use anyhow::{anyhow, bail, Context};
use base64::Engine;
use serde::{Deserialize, Serialize};
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
    /// SSH 登录的地址和端口：同机 IP 节点没有自己的 SSH，是宿主机的
    ip: String,
    ssh_port: i32,
    lifecycle: String,
    /// 同机 IP 节点的宿主机
    host_id: Option<Uuid>,
    /// 机器自己的 IP（同机 IP 节点的出口 IP；其它机器同 `ip`）
    own_ip: String,
    /// 同机 IP 节点的公网 IP 是 1:1 NAT 映射的：映射到的宿主机本地地址（ssserver 监听并从它出站）；在网卡上的、其它机器为 None
    bind_ip: Option<String>,
}

/// 等外部事件的步骤（首次上报、线路机回报应用 / 摘除）的兜底重查间隔。事件到了由 `jobs::wake_for` 把作业提前并唤醒引擎，
/// 重查只补没有唤醒的情形（升级、恢复后的上报不算首次上报，中控重启等）；搬空 N 个节点时 N 个作业不再每 2–3 秒
/// 各走一遍领取、续租、读写步骤
const EVENT_POLL_S: u64 = 5;

/// 远端脚本/配置层面的失败
fn remote(msg: impl Into<String>) -> anyhow::Error { StepError::err(ErrKind::Remote, msg) }

impl Ctx<'_> {
    fn target_id(&self) -> anyhow::Result<Uuid> { self.job.target_id.ok_or_else(|| anyhow!("作业没有目标机器")) }

    /// 目标机器。同机 IP 节点的 ip / ssh_port 取宿主机的：步骤里的 SSH 都自动登录宿主机
    async fn machine(&self) -> anyhow::Result<M> {
        Ok(sqlx::query_as(
            "select m.id, m.kind::text kind, host(coalesce(h.primary_ip, m.primary_ip)) ip, coalesce(h.ssh_port, m.ssh_port) ssh_port,
                    m.lifecycle::text lifecycle, m.host_id, host(m.primary_ip) own_ip, host(m.bind_ip) bind_ip
             from machine m left join machine h on h.id = m.host_id where m.id = $1",
        )
        .bind(self.target_id()?)
        .fetch_one(&self.st.db)
        .await?)
    }

    /// 节点在机器上的 ssserver 实例：普通 VPS 节点（宿主机）固定 meta::SS_PORT；同机 IP 节点的端口是它出站参数里的 SS 端口，
    /// 监听与出站用 bind 地址（1:1 NAT 映射的用 bind_ip，否则就是它的公网 IP）。收敛、退役清理、换线路机的补偿都从这里取
    async fn ss_inst(&self, m: &M) -> anyhow::Result<SsInst> {
        if m.host_id.is_none() {
            return Ok(SsInst::host());
        }
        let port: i32 = sqlx::query_scalar("select port from node_outbound where machine_id = $1").bind(m.id).fetch_one(&self.st.db).await?;
        Ok(SsInst::ip_node(port as u16, &m.own_ip, m.bind_ip.as_deref()))
    }

    /// 退役 / 取消上架时清理机器的脚本。同机 IP 节点只删自己的实例（宿主机的 agent 与 ssserver 不动）
    async fn cleanup_script(&self, m: &M) -> anyhow::Result<String> {
        Ok(match m.kind.as_str() {
            "relay" => scripts::RELAY_CLEANUP.to_string(),
            _ if m.host_id.is_some() => vars(&[("SS_PORT", &self.ss_inst(m).await?.port.to_string())]) + scripts::NODE_IP_CLEANUP,
            _ => scripts::NODE_CLEANUP.to_string(),
        })
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

    // 步骤的日志、警告常带远端输出：按 clip 截短
    async fn log(&self, msg: impl AsRef<str>) { log(self.st, self.job.id, "info", clip(msg.as_ref())).await }
    async fn warn(&self, msg: impl AsRef<str>) { log(self.st, self.job.id, "warn", clip(msg.as_ref())).await }

    fn dist(&self, name: &str) -> std::path::PathBuf { self.st.cfg.dist_dir.join(name) }
}

pub async fn run(c: &Ctx<'_>, key: &str) -> anyhow::Result<Outcome> {
    match key {
        "preflight" => preflight(c).await,
        "ip_check" => ip_check(c).await,
        "bootstrap" => bootstrap(c).await,
        "converge" => converge_node(c).await,
        "converge_relay" => converge_relay(c).await,
        "tune" => tune_step(c).await,
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
        // 替换节点
        "routers" => replace::routers(c).await,
        "renumber" => replace::renumber(c).await,
        _ => bail!("未知步骤 {key}"),
    }
}

/// 缺 root 密码时告诉用户怎么补：密码用过即丢，失败作业的密码 3 天后也会清掉
const NO_PASSWORD_HINT: &str = "在网页作业的「重试」里填写 root 密码，或 fleetctl retry <作业> --password-env 变量名（提供过的密码用过即丢，失败作业留存的密码 3 天后清除）";

impl Ctx<'_> {
    /// 中控密钥登录不了、又没有 root 密码：两条出路都写上，录入中控公钥那条带公钥全文（放在最后，方便复制）。
    /// `detail` 是 ssh 的报错（可以为空）
    async fn no_login(&self, detail: &str) -> anyhow::Error {
        let detail = detail.trim();
        let detail = if detail.is_empty() { String::new() } else { format!("（{detail}）") };
        let key = ssh::add_key_hint(&self.st.cfg.ssh_key).await;
        StepError::err(ErrKind::Auth, format!("中控密钥登录不了，也没有可用的 root 密码{detail}。二选一后重试：{NO_PASSWORD_HINT}；或{key}"))
    }
}

// ── 上架 ──────────────────────────────────────────────────────────────────

async fn preflight(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let m = c.machine().await?;
    // 该 IP 已有记录且指纹变了：失败，除非作业参数明确允许重置（机器重装过）
    let allow_reset = c.job.params["reset_host_key"].as_bool().unwrap_or(false);
    // 同上架检查：要读到 SSH 版本行才算通（中控出网经透明代理时只看 TCP 什么地址都「连得上」，要等 ssh 自己超时才失败）
    let mut probe = Ok(());
    for _ in 0..3 {
        probe = ssh::ssh_probe(&m.ip, m.ssh_port as u16, Duration::from_secs(5)).await;
        if probe.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    if let Err(why) = probe {
        return Err(StepError::err(ErrKind::Network, format!("SSH {}:{} {why}", m.ip, m.ssh_port)));
    }
    let fp = ssh::pin_host_key(&c.st.cfg.known_hosts, &m.ip, m.ssh_port as u16, allow_reset).await?;
    c.log(format!("主机公钥已记录/核对 {}", ssh::fingerprint(&fp))).await;
    let mut out = c.ssh(&m, None).run(scripts::FACTS, Duration::from_secs(30)).await?;
    if !out.ok() {
        // 只有「密钥登录被拒」才改用密码；连接/指纹类的失败直接报，不要拿密码去试
        if out.ssh_kind() != Some(ErrKind::Auth) {
            return Err(out.fail("登录失败"));
        }
        let Some(pw) = c.password()? else { return Err(c.no_login(&out.stderr).await) };
        out = c.ssh(&m, Some(pw)).run(scripts::FACTS, Duration::from_secs(30)).await?;
        if !out.ok() {
            return Err(out.fail("用密码也登录不了"));
        }
    }
    // 已过标准支持期的发行版只提醒，不拦
    if let Some(w) = scripts::os_eol_warning(out.kv("os").unwrap_or("")) {
        c.warn(w).await;
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
        c.warn(format!("{} 端口已经有程序在监听；收敛时会被 fleet-ssserver 接管", crate::meta::SS_PORT)).await;
    }
    Ok(Outcome::Done(json!({
        "host_key": ssh::fingerprint(&fp), "arch": arch, "os": out.kv("os"), "kernel": out.kv("kernel"), "mem_mb": out.kv("mem_mb"),
    })))
}

/// 同机 IP 节点上架的第一步，在宿主机上执行：IP 要直接配在宿主机网卡上、或 1:1 NAT 映射到网卡上的某个本地地址
/// （记进 bind_ip），经它访问中控回显的来源要正是这个 IP（出站路由对），暂定的 SS 端口被别的程序占着时锁住宿主机行，改成最小的空闲端口
async fn ip_check(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let m = c.machine().await?;
    let host = m.host_id.ok_or_else(|| anyhow!("这台机器不是同机 IP 节点"))?;
    let (hname,): (String,) = sqlx::query_as("select display_name(m.kind::text, m.label, t.name, m.number) from machine m left join team t on t.id = m.team_id where m.id = $1")
        .bind(host)
        .fetch_one(&c.st.db)
        .await?;
    if let Err(why) = ssh::ssh_probe(&m.ip, m.ssh_port as u16, Duration::from_secs(5)).await {
        return Err(StepError::err(ErrKind::Network, format!("宿主机 {hname} 的 SSH {}:{} {why}", m.ip, m.ssh_port)));
    }
    let ip = m.own_ip.as_str();
    let echo = format!("{}/echo", c.st.cfg.public_url.trim_end_matches('/'));
    let out = c.ssh(&m, None).run(&(vars(&[("IP", ip), ("ECHO", &echo)]) + scripts::IP_CHECK), Duration::from_secs(60)).await?;
    if !out.ok() {
        return Err(out.fail(&format!("在宿主机 {hname} 上核对 IP 失败")));
    }
    let local_ips = out.kv("local_ips").unwrap_or("");
    // 不在网卡上、经 NAT 映射的：找到的本地地址就是 bind 地址（脚本探测时它的回显已经是这个 IP）
    let bind = scripts::bind_addr(ip, &hname, local_ips, out.kv("nat").unwrap_or("")).map_err(remote)?;
    let local = local_ips.replace(',', ", ");
    let got = out.kv("echo").unwrap_or("").trim();
    if got != ip {
        return Err(remote(match got.parse::<IpAddr>() {
            Ok(_) => format!("经 {ip} 出站时中控看到的来源是 {got}：宿主机没让这个 IP 的流量从它自己出去（检查策略路由 / NAT）"),
            Err(_) => format!("经 {ip} 访问中控 {echo} 失败：{got}"),
        }));
    }
    // 记下（或清掉）bind 地址：收敛时 ssserver 监听并从它出站。每次按这次核对的结果写
    sqlx::query("update machine set bind_ip = $2::inet where id = $1").bind(m.id).bind(&bind).execute(&c.st.db).await?;
    c.st.emit(Event::Machine { id: m.id });
    if let Some(b) = &bind {
        c.log(format!("{ip} 不在宿主机网卡上，是 1:1 NAT 映射到本地地址 {b} 的：ssserver 监听并从 {b} 出站")).await;
    }
    // 正在监听的端口里去掉配置本来就绑着这个 IP 的（这台节点上次退役没清掉的旧实例）：收敛会用新配置覆盖并重启它，
    // 算作占用的话会改用别的端口，旧实例就成了没人清理的孤儿
    let ports = |k: &str| -> BTreeSet<u16> { out.kv(k).unwrap_or("").split(',').filter_map(|p| p.trim().parse().ok()).collect() };
    let busy: BTreeSet<u16> = ports("busy").difference(&ports("own")).copied().collect();
    let mut port = c.ss_inst(&m).await?.port;
    if busy.contains(&port) {
        let mut tx = c.st.db.begin().await?;
        sqlx::query("select 1 from machine where id = $1 for update").bind(host).execute(&mut *tx).await?;
        let mut taken = crate::db::ip_node_ports(&mut tx, host, Some(m.id)).await?;
        taken.extend(&busy);
        let free = crate::db::free_ss_port(&taken).ok_or_else(|| remote(format!("宿主机 {hname} 上没有空闲的 SS 端口")))?;
        sqlx::query("update node_outbound set port = $2, updated_at = now() where machine_id = $1").bind(m.id).bind(free as i32).execute(&mut *tx).await?;
        tx.commit().await?;
        c.warn(format!("SS 端口 {port} 在宿主机上已被占用，改用 {free}")).await;
        port = free;
    }
    Ok(Outcome::Done(json!({"ss_port": port, "local_ips": local, "egress_ip": got, "bind_ip": bind})))
}

async fn bootstrap(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let m = c.machine().await?;
    let key_ok = c.ssh(&m, None).run("true", Duration::from_secs(20)).await.map(|o| o.ok()).unwrap_or(false);
    if key_ok {
        // 有密码也立刻丢掉
        c.set_params(json!({"login": null})).await?;
        return Ok(Outcome::Skipped("中控密钥已经能登录".into()));
    }
    let Some(pw) = c.password()? else { return Err(c.no_login("").await) };
    let pubkey = ssh::public_key(&c.st.cfg.ssh_key).await.ok_or_else(|| anyhow!("读不到中控公钥 {}.pub", c.st.cfg.ssh_key.display()))?;
    let script = vars(&[("PUBKEY", &pubkey)]) + scripts::BOOTSTRAP_KEY;
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

/// 校验节点防火墙脚本的输出：规则加载成功，且放行集合恰好等于期望的线路机 IP。`table` 是 nft 表名（`SsInst::table`）
fn check_nft(out: &Output, allow: &str, table: &str) -> anyhow::Result<()> {
    match out.kv("nft") {
        Some("missing") => return Err(remote(format!("节点上没有防火墙放行文件（/etc/fleet/nft/{table}.nft），这台节点还没收敛过？"))),
        Some("error") => return Err(remote(format!("nft 规则校验或加载失败：{}", out.stderr.trim()))),
        _ => {}
    }
    let (want, got) = (ip_set(allow), ip_set(out.kv("nft_relays").unwrap_or("")));
    if want != got {
        let list = |s: &BTreeSet<String>| if s.is_empty() { "无".to_string() } else { s.iter().cloned().collect::<Vec<_>>().join("、") };
        return Err(remote(format!("节点防火墙放行名单和期望不一致：期望 {}；实际 {}", list(&want), list(&got))));
    }
    Ok(())
}

async fn converge_node(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let m = c.machine().await?;
    let t = c.ssh(&m, None);
    // 同机 IP 节点登录的是宿主机：系统基线和调优宿主机上架时已经做过
    let mut tuned = None;
    if m.host_id.is_none() {
        let base = t.run(scripts::BASELINE, Duration::from_secs(600)).await?;
        if !base.ok() {
            return Err(base.fail("系统基线失败"));
        }
        if base.kv("sshd_password") != Some("no") {
            return Err(remote(format!("sshd 仍允许密码登录（{}）", base.kv("sshd_password").unwrap_or("?"))));
        }
        tuned = Some(apply_tune(c, &m, &t).await?);
    }
    let pushed = t.push(&c.dist("ssserver"), "/usr/local/bin/ssserver", "0755", Duration::from_secs(180)).await?;
    let (method, secret): (Option<String>, Vec<u8>) = sqlx::query_as("select method, secret_enc from node_outbound where machine_id = $1")
        .bind(m.id)
        .fetch_one(&c.st.db)
        .await?;
    let password = c.st.secrets.open_str(&secret)?;
    let allow = relay_ips_for(c, m.id, c.param_uuid("relay_id").ok()).await?;
    let inst = c.ss_inst(&m).await?;
    let script = vars(&[
        ("SS_PORT", &inst.port.to_string()),
        ("SS_METHOD", method.as_deref().unwrap_or(crate::meta::SS_METHOD)),
        ("SS_PASSWORD", &password),
        ("ALLOW", &allow),
    ]) + &scripts::node_ss(&inst);
    let out = t.run(&script, Duration::from_secs(120)).await?;
    if !out.ok() {
        return Err(out.fail("配置 ssserver / 防火墙失败"));
    }
    check_nft(&out, &allow, &inst.table())?;
    if out.kv("ss") != Some("listening") {
        return Err(remote(format!("ssserver 没有起来：{}", out.stdout.trim())));
    }
    warn_linklocal_dns(c, &out, "ssserver").await;
    if pushed {
        // 只重启这台节点自己的实例：同一台机器上别的 ssserver（宿主机的、别的同机 IP 节点的）下次重启时才换上新程序
        t.run(&format!("systemctl restart {}", inst.svc()), Duration::from_secs(30)).await?;
    }
    let bbr = tuned.as_ref().map(|x| x["bbr"] == true);
    Ok(Outcome::Done(json!({"bbr": bbr, "tune": tuned, "nft_relays": out.kv("nft_relays"), "ssserver_updated": pushed})))
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
    let tuned = apply_tune(c, &m, &t).await?;
    t.push(&c.dist("xray"), "/usr/local/bin/xray", "0755", Duration::from_secs(300)).await?;
    // 渲染出的配置只用到 geoip:private（策略不允许 geosite: 规则），geosite.dat 不推
    t.push(&c.dist("geoip.dat"), "/usr/local/share/xray/geoip.dat", "0644", Duration::from_secs(300)).await?;
    let out = t.run(&scripts::relay_xray(), Duration::from_secs(60)).await?;
    if !out.ok() {
        return Err(out.fail("安装 fleet-xray 失败"));
    }
    warn_linklocal_dns(c, &out, "Xray").await;
    Ok(Outcome::Done(json!({"xray": out.kv("xray_version"), "bbr": tuned["bbr"], "tune": tuned})))
}

/// 系统调优（当前档见 tune.rs）：上架收敛和作业 `tune` 共用。脚本自己出错（写文件、原值快照失败）才失败；写不进的键、没同步的时钟、
/// 第三方调优残留记警告，默认路由上的初始窗口记日志；逐键结果就是返回值（存进步骤结果）。带宽取套餐（渲染时代入），远端回显的要对得上。
/// 成功后记 machine.tune_profile / tuned_at / tune_params（不动 revision：不是用户编辑）。同机 IP 节点不调用
async fn apply_tune(c: &Ctx<'_>, m: &M, t: &Target) -> anyhow::Result<Value> {
    let role = if m.kind == "relay" {
        let (port_min, port_max): (i32, i32) = sqlx::query_as("select port_min, port_max from relay where machine_id = $1").bind(m.id).fetch_one(&c.st.db).await?;
        tune::Role::Relay { port_min, port_max }
    } else {
        tune::Role::Node
    };
    let plan_bw: Option<i32> = sqlx::query_scalar("select pl.bandwidth_mbps from machine m left join plan pl on pl.id = m.plan_id where m.id = $1")
        .bind(m.id)
        .fetch_one(&c.st.db)
        .await?;
    let out = t.run(&tune::script(role, plan_bw), Duration::from_secs(480)).await?;
    if !out.ok() {
        return Err(out.fail("系统调优失败"));
    }
    if out.kv("tune_profile") != Some(tune::PROFILE) {
        return Err(remote(format!("调优脚本没有跑到最后：{}", out.stdout.trim())));
    }
    let bw = tune::bandwidth(plan_bw).0.to_string();
    if out.kv("bw") != Some(bw.as_str()) {
        return Err(remote(format!("调优脚本回显的带宽 {} 和代入的 {bw} 不一致", out.kv("bw").unwrap_or("?"))));
    }
    let r = tune::parse(&out.stdout);
    for w in &r.warns {
        c.warn(w).await;
    }
    for i in &r.infos {
        c.log(i).await;
    }
    sqlx::query("update machine set tune_profile = $2, tuned_at = now(), tune_params = $3 where id = $1")
        .bind(m.id)
        .bind(tune::PROFILE)
        .bind(r.result.get("params").filter(|p| !p.is_null()))
        .execute(&c.st.db)
        .await?;
    c.st.emit(Event::Machine { id: m.id });
    Ok(r.result)
}

/// 作业 `tune`：重做调优；参数 `rollback: true` 时按机器上的原值快照撤销（写回原值、删调优写的 sysctl 与开机载入文件），
/// 成功后清掉 machine.tune_profile / tuned_at / tune_params（视图显示「未调优」）。没有快照就失败，什么都不改
async fn tune_step(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let m = c.machine().await?;
    let t = c.ssh(&m, None);
    if c.job.params["rollback"] != true {
        return Ok(Outcome::Done(apply_tune(c, &m, &t).await?));
    }
    let out = t.run(&tune::rollback_script(), Duration::from_secs(120)).await?;
    if !out.ok() {
        return Err(out.fail("撤销系统调优失败"));
    }
    let Some(r) = tune::parse_rollback(&out.stdout) else {
        return Err(remote(format!("这台机器上没有调优前的原值快照（{}）：tune-v2 起第一次调优时才记，之前的档没有记原值，没法自动撤销", tune::ORIG)));
    };
    for w in &r.warns {
        c.warn(w).await;
    }
    sqlx::query("update machine set tune_profile = null, tuned_at = null, tune_params = null where id = $1").bind(m.id).execute(&c.st.db).await?;
    c.st.emit(Event::Machine { id: m.id });
    Ok(Outcome::Done(r.result))
}

/// 装单元的脚本报了 `linklocal_dns=1`（见 scripts::LINKLOCAL_DNS）：记一条 warn。`what` 是哪个程序
async fn warn_linklocal_dns(c: &Ctx<'_>, out: &Output, what: &str) {
    if out.kv("linklocal_dns") == Some("1") {
        c.warn(format!(
            "这台机器的 DNS（/etc/resolv.conf）是链路本地地址（多半和云厂商的元数据服务是同一个）：{what} 改用公共解析器（1.1.1.1、8.8.8.8）解析域名，元数据服务照旧拦着；这台机器出不了 53 端口的话域名会解析失败"
        ))
        .await;
    }
}

/// agent 程序在机器上的位置；推新版本时旧的留在同目录的 `.prev`
const AGENT_BIN: &str = "/usr/local/bin/fleet-agent";

/// 升级 agent：重启后等新版本上报的时长，过了还没等到就换回旧版本
const ROLLBACK_AFTER_S: i64 = 120;

/// 线路机现在的 Xray 状态：agent 最近报的（`reported_by` 是报它的 agent 版本）与应用错误（中控记的、agent 自报的都算）
#[derive(sqlx::FromRow)]
struct XrayNow {
    generation: i64,
    reported_by: Option<String>,
    running: Option<bool>,
    applied: Option<i64>,
    error: Option<String>,
}

async fn xray_now(c: &Ctx<'_>, relay: Uuid) -> anyhow::Result<XrayNow> {
    Ok(sqlx::query_as(
        "select r.generation, a.info->'xray'->>'agent_version' reported_by, (a.info->'xray'->>'running')::bool running,
                (a.info->'xray'->>'applied_generation')::bigint applied, coalesce(r.apply_error, a.info->'agent_health'->>'last_apply_error') error
         from relay r left join agent_state a on a.machine_id = r.machine_id where r.machine_id = $1",
    )
    .bind(relay)
    .fetch_one(&c.st.db)
    .await?)
}

/// 升级线路机的 agent 之后新版本要做到的几项（wait_report 查）。只查升级前就做到的：升级前 Xray 就没在跑、
/// 配置没应用到当时的代数、有应用错误的，不能因此永远升级不了
#[derive(Serialize, Deserialize)]
struct XrayChecks {
    /// Xray 在跑
    running: bool,
    /// 已应用的配置要追上这一代（升级开始时的期望代数）
    generation: Option<i64>,
    /// 没有应用错误
    no_error: bool,
}

impl XrayChecks {
    fn before(x: &XrayNow) -> Self {
        Self {
            running: x.running == Some(true),
            generation: x.applied.is_some_and(|a| a >= x.generation).then_some(x.generation),
            no_error: x.error.is_none(),
        }
    }

    fn any(&self) -> bool { self.running || self.generation.is_some() || self.no_error }

    fn describe(&self) -> String {
        let mut v = vec![];
        if self.running {
            v.push("Xray 在跑".to_string());
        }
        if let Some(g) = self.generation {
            v.push(format!("配置应用到代数 {g}"));
        }
        if self.no_error {
            v.push("没有应用错误".into());
        }
        if v.is_empty() { "不查（升级前 Xray 就不正常）".into() } else { v.join("、") }
    }

    /// 新版本自己报的状态里没做到的项；都做到了返回 None
    fn unmet(&self, x: &XrayNow, version: &str) -> Option<String> {
        if x.reported_by.as_deref() != Some(version) {
            return Some("没有报 Xray 状态".into());
        }
        let mut v = vec![];
        if self.running && x.running != Some(true) {
            v.push("Xray 没在跑".to_string());
        }
        if let Some(g) = self.generation.filter(|g| x.applied.is_none_or(|a| a < *g)) {
            v.push(format!("已应用的配置代数 {}，没到 {g}", x.applied.unwrap_or(0)));
        }
        if let Some(e) = x.error.as_deref().filter(|_| self.no_error) {
            v.push(format!("应用配置出错：{e}"));
        }
        (!v.is_empty()).then(|| v.join("；"))
    }
}

/// 装 agent：先签发新令牌装上，**不**撤销旧令牌；等 wait_report 看到新令牌真的上报了，才撤销这台机器的其它令牌。
/// 这样安装中途失败时旧 agent 还能继续上报。
/// 升级时程序换成了新版本（`agent_replaced`），之后装不上、起不来（或 wait_report 等不到新版本上报、线路机检查没通过）就换回 .prev
async fn agent(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let upgrading = c.job.kind == "upgrade_agent";
    // pushed：「全部升级」据此看失败在推送之前还是之后（见 jobs::halted）。每次尝试从 false 起；
    // 同一次尝试里重跑这一步（中控重启后接着做）不清：前一次可能已经换上程序、重启过了
    if upgrading && c.first {
        c.set_params(json!({"pushed": false})).await?;
    }
    let m = c.machine().await?;
    // 升级线路机：记下升级前的 Xray 状态。本作业已经换过程序的（重试）沿用第一次记的：之后机器被新版本动过
    let mut checks = serde_json::from_value::<XrayChecks>(c.job.params["xray_checks"].clone()).ok();
    if upgrading && m.kind == "relay" && c.job.params["agent_replaced"] != true {
        checks = Some(XrayChecks::before(&xray_now(c, m.id).await?));
        c.set_params(json!({"xray_checks": checks})).await?;
    }
    let t = c.ssh(&m, None);
    let dist = c.dist("fleet-agent");
    let bin = tokio::fs::read(&dist).await.with_context(|| format!("读取 {}", dist.display()))?;
    // 「全部升级」：推的必须是发起时 dist 里的那个程序。作业开始前核对过（jobs::halted），这里核对真正要推的内容：
    // 重新部署中控是先换 dist、后重启中控，其间开始推送的作业读到的已经是新程序
    if c.job.params["rollout"]["sha256"].as_str().is_some_and(|w| w != hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&bin))) {
        bail!(super::rollout_changed(&c.job.params));
    }
    let pushed = t.push_keep_prev(bin, AGENT_BIN, "0755", Duration::from_secs(180)).await?;
    // 重试时推送发现已经是新程序会返回 false：以本作业第一次换上的为准
    let replaced = pushed || c.job.params["agent_replaced"] == true;
    let can_rollback = upgrading && replaced;
    let token = format!("fa1_{}", random_token(32));
    let token_id = crate::db::new_id();
    // 只给上架中、运行中的机器签：这一步跑着时机器被退役了（退役的「撤销令牌」可能已经做完），不能再留下一枚有效令牌
    let n = sqlx::query(
        "insert into agent_token (id, machine_id, token_hash)
         select $1, $2, $3 where exists (select 1 from machine where id = $2 and lifecycle in ('provisioning', 'active'))",
    )
    .bind(token_id)
    .bind(m.id)
    .bind(sha256(token.as_bytes()))
    .execute(&c.st.db)
    .await?
    .rows_affected();
    if n == 0 {
        bail!("机器已经不在上架中或运行中（可能刚被退役），不再签发 agent 令牌");
    }
    // 记下这枚新令牌，wait_report 用它判断「新 agent 已经在用新令牌上报」。
    // 从这里起要用 dist 里的程序重启 agent（pushed）：之后的失败（起不来、没上报、线路机检查没通过）与新版本有关
    c.set_params(json!({"agent_token_id": token_id, "agent_replaced": replaced, "pushed": true})).await?;
    // 线路机先收敛 Xray 单元、后换 agent（见 scripts::relay_agent）
    let script = vars(&[
        ("SERVER_URL", &c.st.cfg.public_url),
        ("ROLE", if m.kind == "relay" { "relay" } else { "node" }),
        ("MACHINE_ID", &m.id.to_string()),
        ("TOKEN", &token),
        ("BIN_CHANGED", if pushed { "1" } else { "0" }),
    ]) + &if m.kind == "relay" { scripts::relay_agent() } else { scripts::agent() };
    let out = match t.run(&script, Duration::from_secs(120)).await {
        Ok(o) => o,
        Err(e) if can_rollback => return Err(rollback(c, &m, kind_of(&e), &format!("新版本装上后没能配置、重启 agent（{e:#}）")).await),
        Err(e) => return Err(e),
    };
    let state = out.kv("agent").unwrap_or("?");
    let version = out.kv("version").filter(|v| !v.is_empty());
    let Some(version) = version.filter(|_| out.ok() && state == "active") else {
        if !can_rollback {
            return Err(remote(format!("agent 没有起来：{} {}", out.stdout.trim(), out.stderr.trim())));
        }
        let why = match state {
            _ if !out.ok() => format!("配置脚本出错：{}", out.stderr.trim()),
            "crashing" => "主进程起来就退出".to_string(),
            "active" => "读不出版本号：--version 没有输出".to_string(),
            s => format!("agent 是 {s}"),
        };
        let v = version.map(|v| format!(" {v}")).unwrap_or_default();
        return Err(rollback(c, &m, ErrKind::Remote, &format!("新版本{v} 起不来（{why}）")).await);
    };
    // wait_report 要等这个版本上报
    c.set_params(json!({"agent_version": version})).await?;
    if out.kv("xray_unit") == Some("error") {
        c.warn(format!("fleet-xray 单元没更新上：{}", out.stderr.trim())).await;
    }
    warn_linklocal_dns(c, &out, "Xray").await;
    // 链路本地 DNS 的公共解析器挂载要 Xray 重启才生效（见 scripts::XRAY_RESOLV_SH）
    let xray_restarted = out.kv("xray_restart") == Some("resolv");
    if xray_restarted {
        c.log("重启了一次 Xray（这台线路机上客户的连接断开了一下，客户端会自动重连）：它还在用被拦掉的链路本地解析器，重启后才用上公共解析器").await;
    } else if out.kv("xray_restart") == Some("error") {
        c.warn(format!("重启 Xray 换上公共解析器没成功，Xray 可能解析不了域名，需要手动 systemctl restart fleet-xray：{}", out.stderr.trim())).await;
    }
    if out.kv("logrotate") == Some("error") {
        c.warn(format!("访问日志轮转配置没装上：{}", out.stderr.trim())).await;
    }
    // xray_checks：等新版本上报时线路机还要做到的几项（程序换过才查，见 wait_report）
    Ok(Outcome::Done(json!({
        "binary_updated": pushed, "token_id": token_id, "agent_version": version, "prev_version": out.kv("prev_version").filter(|_| replaced),
        "xray_restarted": xray_restarted.then_some(true),
        "xray_checks": checks.filter(|_| can_rollback).map(|k| k.describe()),
    })))
}

/// 升级失败：把 .prev 换回去并重启 agent。返回作业的错误（`kind` 是升级本身失败的类别）：
/// 换回去了写「已回滚到 <旧版本>」；换不回去写清楚要人工处理
async fn rollback(c: &Ctx<'_>, m: &M, kind: ErrKind, why: &str) -> anyhow::Error {
    c.warn(format!("{why}：换回旧版本")).await;
    let manual = format!("需要人工处理（旧程序在 {AGENT_BIN}.prev，换回去后重启 fleet-agent；先看 agent 日志找原因）");
    match c.ssh(m, None).run(&scripts::agent_rollback(), Duration::from_secs(60)).await {
        Ok(o) if o.ok() && o.kv("agent") == Some("active") => {
            let v = o.kv("version").filter(|v| !v.is_empty()).unwrap_or("旧版本");
            c.warn(format!("已回滚到 {v}")).await;
            StepError::err(kind, format!("{why}，已回滚到 {v}"))
        }
        Ok(o) if o.kv("rollback") == Some("no_prev") => {
            remote(format!("{why}，机器上没有 {AGENT_BIN}.prev，没能回滚：需要人工处理（先看 agent 日志找原因，修好后重试这个作业）"))
        }
        Ok(o) => {
            let detail = if o.ok() { format!("旧版本也没起来：agent={}", o.kv("agent").unwrap_or("?")) } else { format!("换回去的脚本出错：{}", o.stderr.trim()) };
            remote(format!("{why}，回滚也没成功（{detail}）：{manual}"))
        }
        Err(e) => StepError::err(kind_of(&e), format!("{why}，回滚也没成功（{e:#}）：{manual}")),
    }
}

async fn wait_report(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let id = c.target_id()?;
    let tid = c.param_uuid("agent_token_id").ok();
    let want = c.job.params["agent_version"].as_str();
    // 上报要晚于「装 agent」那一步开始；有新令牌时，新令牌必须被用过（排除旧 agent 用旧令牌的上报）；
    // 报的还得是装上的那个版本（升级时排除换回去的旧程序）
    let row: Option<(OffsetDateTime, Option<String>)> = sqlx::query_as(
        "select a.last_report_at, a.agent_version from agent_state a
         where a.machine_id = $1 and a.last_report_at >= (select started_at from job_step where job_id = $2 and key = 'agent')
           and ($3::uuid is null or exists (select 1 from agent_token t where t.id = $3 and t.machine_id = $1 and t.revoked_at is null and t.last_used_at is not null))
           and ($4::text is null or a.agent_version = $4)",
    )
    .bind(id)
    .bind(c.job.id)
    .bind(tid)
    .bind(want)
    .fetch_optional(&c.st.db)
    .await?
    .and_then(|(t, v): (Option<OffsetDateTime>, Option<String>)| t.map(|t| (t, v)));
    // 升级时程序换过的：重启后这么久还没做到就换回旧版本（程序没换的没有可换回的，等到步骤超时）
    let replaced = c.job.kind == "upgrade_agent" && c.job.params["agent_replaced"] == true;
    let give_up = replaced && (OffsetDateTime::now_utc() - c.started).whole_seconds() >= ROLLBACK_AFTER_S;
    let new = want.unwrap_or("?");
    let Some((t, v)) = row else {
        if !give_up {
            return Ok(Outcome::Wait(EVENT_POLL_S));
        }
        let m = c.machine().await?;
        return Err(rollback(c, &m, ErrKind::Timeout, &format!("新版本 {new} 重启后 {ROLLBACK_AFTER_S} 秒没有上报")).await);
    };
    // 线路机：新版本自己报的 Xray 状态还要做到升级前做到的几项（新版本把 Xray 弄坏、agent 却照常上报时，不能当成功）
    let miss = match serde_json::from_value::<XrayChecks>(c.job.params["xray_checks"].clone()) {
        Ok(k) if replaced && k.any() => k.unmet(&xray_now(c, id).await?, new),
        _ => None,
    };
    if let Some(miss) = miss {
        if !give_up {
            return Ok(Outcome::Wait(EVENT_POLL_S));
        }
        let m = c.machine().await?;
        return Err(rollback(c, &m, ErrKind::Remote, &format!("新版本 {new} 重启后 {ROLLBACK_AFTER_S} 秒线路机检查没通过（{miss}）")).await);
    }
    let mut revoked = 0;
    if let Some(tid) = tid {
        revoked = sqlx::query("update agent_token set revoked_at = now() where machine_id = $1 and id <> $2 and revoked_at is null")
            .bind(id)
            .bind(tid)
            .execute(&c.st.db)
            .await?
            .rows_affected();
        if revoked > 0 {
            c.st.agent_auth.forget(id);
            c.log(format!("新令牌已上报，撤销了 {revoked} 枚旧令牌")).await;
        }
    }
    Ok(Outcome::Done(json!({"first_report_at": crate::access::fmt_ts(t), "agent_version": v, "old_tokens_revoked": revoked})))
}

async fn attach(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    if let Ok(e) = c.param_uuid("entry_id") {
        return Ok(Outcome::Skipped(format!("入口已建（{e}）")));
    }
    let node = c.target_id()?;
    let relay = c.param_uuid("relay_id")?;
    let mut tx = c.st.db.begin().await?;
    entries::lock_policy_shared(&mut tx).await.map_err(|e| anyhow!(e.message))?;
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
    match m.kind.as_str() {
        "vps" => {}
        "relay" => return Ok(Outcome::Skipped("线路机直出入口没有节点，不用改防火墙".into())),
        _ => return Ok(Outcome::Skipped("家宽代理没有防火墙要改".into())),
    }
    let allow = relay_ips_for(c, m.id, None).await?;
    let table = c.ss_inst(&m).await?.table();
    let out = c.ssh(&m, None).run(&nft_only(&table, &allow), Duration::from_secs(60)).await?;
    if !out.ok() {
        // 脚本在 nft=missing / nft=error 时以非 0 退出
        check_nft(&out, &allow, &table)?;
        return Err(out.fail("更新防火墙失败"));
    }
    check_nft(&out, &allow, &table)?;
    Ok(Outcome::Done(json!({"nft_relays": out.kv("nft_relays")})))
}

/// 只改节点防火墙放行名单的脚本
fn nft_only(table: &str, allow: &str) -> String { vars(&[("NFT_TABLE", table), ("ALLOW", allow)]) + scripts::NODE_NFT_ONLY }

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
                return Ok(Outcome::Wait(EVENT_POLL_S));
            }
            return Err(remote(format!("线路机应用失败：{e}")));
        }
        return Ok(if applied && lc == "active" { Outcome::Done(json!({"entry": lc})) } else { Outcome::Wait(EVENT_POLL_S) });
    }
    // 新线路机：等第一次应用
    let id = c.target_id()?;
    let (g, a, err): (i64, i64, Option<String>) =
        sqlx::query_as("select generation, applied_generation, apply_error from relay where machine_id = $1").bind(id).fetch_one(&c.st.db).await?;
    if let Some(e) = err {
        if c.first {
            rebump(c, id, None).await?;
            return Ok(Outcome::Wait(EVENT_POLL_S));
        }
        return Err(remote(format!("Xray 配置应用失败：{e}")));
    }
    Ok(if a >= g { Outcome::Done(json!({"generation": a})) } else { Outcome::Wait(EVENT_POLL_S) })
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
        None => {
            // 等满步骤超时都没有一份出口探测：失败原因写明是线路机没报（probe.rs 在这两种情况下不上报），不只是「等了 300 秒」
            let limit = super::defs(&c.job.kind).iter().find(|d| d.key == "verify").map_or(300, |d| d.timeout_s);
            if (OffsetDateTime::now_utc() - c.started).whole_seconds() + 5 > limit {
                return Err(StepError::err(
                    ErrKind::Timeout,
                    format!("等了 {limit} 秒没有出口探测结果：这段时间线路机没有报出口探测（回显服务直连不通，或经入口访问备用目标通）"),
                ));
            }
            Ok(Outcome::Wait(5))
        }
    }
}

async fn activate(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let id = c.target_id()?;
    sqlx::query(
        "update machine set lifecycle = 'active', activated_at = coalesce(activated_at, now()), lifecycle_reason = null,
            spare_since = coalesce(spare_since, case when exists (select 1 from team t where t.id = machine.team_id and t.spare) then now() end),
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
    // 按线路机排序处理：一个事务里锁多台线路机时和别的事务同序，不互相死锁
    let es: Vec<(Uuid,)> = sqlx::query_as(
        "select id from entry where (node_id = $1 or relay_id = $1) and lifecycle not in ('removing', 'removed') order by relay_id, id",
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
    // 线路机本身在退役（摘的是自己身上的入口）：agent 会照常应用空配置。
    // 在不在线同机器视图（中控收不到上报的时段不算：中控、库刚恢复时线路机的重发还没到）
    let now = OffsetDateTime::now_utc();
    for (relay, port, seen) in &left {
        if seen.is_none_or(|t| !c.st.report_gaps.online(t, now)) && (now - c.started).whole_seconds() > 60 {
            return Err(StepError::err(ErrKind::Network, format!("线路机 {relay} 不在线，入口 {relay}:{port} 摘不掉；线路机恢复后点重试")));
        }
    }
    Ok(Outcome::Wait(EVENT_POLL_S))
}

async fn cleanup(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let m = c.machine().await?;
    if m.kind == "proxy" {
        return Ok(Outcome::Skipped("家宽代理没有要清理的服务".into()));
    }
    if c.job.params["cleanup"] == json!(false) {
        return Ok(Outcome::Skipped("选择了不清理机器".into()));
    }
    if let Err(why) = ssh::ssh_probe(&m.ip, m.ssh_port as u16, Duration::from_secs(5)).await {
        return Ok(Outcome::Skipped(format!("机器连不上（SSH {why}），跳过清理（令牌照样撤销）")));
    }
    let script = c.cleanup_script(&m).await?;
    match c.ssh(&m, None).run(&script, Duration::from_secs(120)).await {
        Ok(o) if o.stdout.contains("cleanup=done") => Ok(Outcome::Done(json!({"ss": o.kv("ss")}))),
        Ok(o) => Ok(Outcome::Skipped(format!("清理没有完成：{}", o.stderr.trim()))),
        Err(e) => Ok(Outcome::Skipped(format!("登录失败，跳过清理：{e}"))),
    }
}

async fn revoke(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let id = c.target_id()?;
    let n = sqlx::query("update agent_token set revoked_at = now() where machine_id = $1 and revoked_at is null")
        .bind(id)
        .execute(&c.st.db)
        .await?
        .rows_affected();
    c.st.agent_auth.forget(id);
    Ok(Outcome::Done(json!({"revoked": n})))
}

async fn finalize(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let m = c.machine().await?;
    let mut tx = c.st.db.begin().await?;
    // 先锁团队行再改机器行：宿主机连同它的同机 IP 节点退役时，同团队的几个退役作业会同时收尾，
    // 压缩编号要改同团队别的节点，都按「团队 → 机器」的顺序加锁才不会互相等
    let team: Option<Uuid> = sqlx::query_scalar("select team_id from machine where id = $1").bind(m.id).fetch_one(&mut *tx).await?;
    if let Some(t) = team {
        sqlx::query("select 1 from team where id = $1 for update").bind(t).execute(&mut *tx).await?;
    }
    sqlx::query(
        "update machine set lifecycle = 'retired', retired_at = now(), revision = revision + 1, updated_at = now() where id = $1",
    )
    .bind(m.id)
    .execute(&mut *tx)
    .await?;
    // 秘密不再需要；出站参数的其余部分（NAT 映射的 SS 端口、上游代理的地址端口用户名）留作记录
    sqlx::query("update node_outbound set secret_enc = null, updated_at = now() where machine_id = $1").bind(m.id).execute(&mut *tx).await?;
    // 同一个事务里移进回收站（快照、统计列；机器从视图里消失）。作业带的 auto（备用到期）一并记进审计
    let bin = crate::recycle::record(&mut tx, m.id, &c.job.created_by, c.job.params["auto"].as_str()).await?;
    // 编号连续：同团队剩下的节点压缩成 01..N
    let mut renamed = vec![];
    let mut relays = vec![];
    if let Some(t) = team.filter(|_| c.st.cfg.numbering.auto_compact) {
        let (ch, rs) = crate::db::compact_team(&mut tx, t, &c.job.created_by, "节点退役后自动压缩").await?;
        renamed = ch;
        relays = rs;
    }
    tx.commit().await?;
    for r in relays {
        c.st.relay_bumped(r);
    }
    if !renamed.is_empty() {
        c.log(format!("编号已压缩：{} 台节点改了编号（见审计）", renamed.len())).await;
    }
    // 同机 IP 节点登录的是宿主机：记着的主机公钥是宿主机的，不能忘
    if m.host_id.is_none() {
        ssh::forget_host(&c.st.cfg.known_hosts, &m.ip, m.ssh_port as u16).await?;
    }
    Ok(Outcome::Done(json!({"renumbered": renamed.len(), "recycle_bin_id": bin})))
}

/// 按天分批删分区大表里这台机器（和它名下入口）的原始数据：每天一条语句，走 (entry_id/machine_id, ts) 索引，
/// 单条语句只碰一个分区，不会一个事务里锁很久。窗口覆盖整个保留期加前后几天；幂等。
async fn purge_partitioned(c: &Ctx<'_>, machine: Uuid, eids: &[Uuid]) -> anyhow::Result<u64> {
    // 遥测按配置文件的保留期，访问记录按面板里设的
    let back = c.st.cfg.retention.raw_days.max(c.st.access_retention().raw_days) as i64 + 2;
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
    // 排队期间情况可能变了（宿主机上又有了同机 IP 节点…）：执行前再查一次
    if let Some(why) = crate::db::purge_blocker(&c.st.db, id).await? {
        bail!("{why}");
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
    // 访问汇总：access_hourly、access_daily 按节点或线路机，其余三张按节点（线路机自己没有这三张的行）
    for t in ["access_hourly", "access_daily"] {
        n += sqlx::query(&format!("delete from {t} where node_id = $1 or relay_id = $1")).bind(id).execute(&mut *tx).await?.rows_affected();
    }
    for t in ["access_source_hourly", "access_target_seen", "access_source_late"] {
        n += sqlx::query(&format!("delete from {t} where node_id = $1")).bind(id).execute(&mut *tx).await?.rows_affected();
    }
    sqlx::query("delete from incident where subject_id = $1 or subject_id = any($2)").bind(id).bind(&eids).execute(&mut *tx).await?;
    sqlx::query("delete from silence where machine_id = $1").bind(id).execute(&mut *tx).await?;
    sqlx::query("delete from entry where id = any($1)").bind(&eids).execute(&mut *tx).await?;
    // 其余（令牌、agent 状态、账单、续费、出站、线路机参数）随 machine 级联删除
    sqlx::query("delete from machine where id = $1").bind(id).execute(&mut *tx).await?;
    tx.commit().await?;
    // 小时流量删了（只剩 35 天前的、或对端已退役）：流量缓存跟着作废
    c.st.live.rolled_up();
    Ok(Outcome::Done(json!({"rows_deleted": n, "entries": eids.len()})))
}

// ── 取消时的补偿 ──────────────────────────────────────────────────────────

/// 尽力而为的一步：出错只记日志（不再静默吞掉），返回是否成功
async fn note<T, E: std::fmt::Display>(st: &AppState, job: Uuid, what: &str, r: Result<T, E>) -> Option<T> {
    match r {
        Ok(v) => Some(v),
        Err(e) => {
            log(st, job, "warn", clip(&format!("补偿「{what}」失败（已忽略）：{e}"))).await;
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

/// 上架/加入口/换线路机被取消：摘掉已建的入口；上架的机器直接退役（释放 IP 与编号）。
/// 尽力而为，错误记日志。取消后不再需要保存的 root 密码，由调用方在置 cancelled 时一并清掉。
pub async fn compensate(st: &AppState, job: &JobRow, owner: &str) {
    let c = Ctx { st, job, owner, started: OffsetDateTime::now_utc(), first: false };
    if job.kind == "move_entry" {
        return lifecycle::compensate_move(&c).await;
    }
    if let Ok(eid) = c.param_uuid("entry_id") {
        remove_created_entry(st, job.id, eid).await;
    }
    if !job.kind.starts_with("provision_") {
        return;
    }
    let Some(id) = job.target_id else { return };
    let Some(m) = note(st, job.id, "读取机器", c.machine().await).await else { return };
    if !matches!(m.lifecycle.as_str(), "provisioning" | "failed") {
        return;
    }
    if m.kind != "proxy" && ssh::ssh_probe(&m.ip, m.ssh_port as u16, Duration::from_secs(5)).await.is_ok() {
        if let Some(script) = note(st, job.id, "读取清理参数", c.cleanup_script(&m).await).await {
            note(st, job.id, "清理机器上的服务", c.ssh(&m, None).run(&script, Duration::from_secs(120)).await).await;
        }
    }
    note(
        st,
        job.id,
        "撤销令牌",
        sqlx::query("update agent_token set revoked_at = now() where machine_id = $1 and revoked_at is null").bind(id).execute(&st.db).await,
    )
    .await;
    st.agent_auth.forget(id);
    // 压缩编号要用这台机器占着编号的团队
    let team = note(
        st,
        job.id,
        "读取团队",
        sqlx::query_as::<_, (Option<Uuid>,)>("select team_id from machine where id = $1").bind(id).fetch_optional(&st.db).await,
    )
    .await
    .flatten();
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
    note(st, job.id, "清掉出站秘密", sqlx::query("update node_outbound set secret_enc = null, updated_at = now() where machine_id = $1").bind(id).execute(&st.db).await).await;
    // 同机 IP 节点记着的主机公钥是宿主机的，不能忘
    if m.host_id.is_none() {
        note(st, job.id, "忘记主机公钥", ssh::forget_host(&st.cfg.known_hosts, &m.ip, m.ssh_port as u16).await).await;
    }
    if st.cfg.numbering.auto_compact {
        if let Some((Some(t),)) = team {
            if let Some(mut tx) = note(st, job.id, "开启事务", st.db.begin().await).await {
                if let Some((_, relays)) = note(st, job.id, "压缩编号", crate::db::compact_team(&mut tx, t, &job.created_by, "上架取消后自动压缩").await).await {
                    if note(st, job.id, "提交编号压缩", tx.commit().await).await.is_some() {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn out(stdout: &str) -> Output { Output { code: 0, stdout: stdout.into(), stderr: String::new() } }

    /// 等不到新版本上报时要由这一步自己回滚：引擎判「等了太久」（只在 Wait 之后查）之前，下一次重查就得到回滚的时点
    #[test]
    fn upgrade_rolls_back_before_step_timeout() {
        let d = super::super::defs("upgrade_agent").iter().find(|d| d.key == "wait_report").unwrap();
        assert!(ROLLBACK_AFTER_S + (EVENT_POLL_S as i64) < d.timeout_s);
    }

    #[test]
    fn nft_check_compares_sets() {
        let t = "ss_guard";
        // 顺序无关
        assert!(check_nft(&out("nft_relays=2.2.2.2,1.1.1.1\n"), "1.1.1.1, 2.2.2.2", t).is_ok());
        // 少了一个
        assert!(check_nft(&out("nft_relays=1.1.1.1\n"), "1.1.1.1, 2.2.2.2", t).is_err());
        // 多了一个
        assert!(check_nft(&out("nft_relays=1.1.1.1,3.3.3.3\n"), "1.1.1.1", t).is_err());
        // 没输出
        assert!(check_nft(&out(""), "1.1.1.1", t).is_err());
        // 脚本自己报的失败：缺的是哪张表的文件
        let e = check_nft(&out("nft=missing\n"), "1.1.1.1", "ss_guard_8389").unwrap_err();
        assert!(e.to_string().contains("/etc/fleet/nft/ss_guard_8389.nft"), "{e}");
        assert!(check_nft(&out("nft=error\nnft_relays=1.1.1.1\n"), "1.1.1.1", t).is_err());
    }
}
