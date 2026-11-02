//! 上架：一批机器先校验（check），全部通过再创建（create）。创建 = 机器（provisioning）+ 秘密 + 账单 + 作业。

use crate::error::ApiJson;
use super::machines::Egress;
use super::ok;
use crate::auth::Actor;
use crate::crypto::reality_keypair;
use crate::db::{audit, new_id, Tx};
use crate::error::{ApiError, ApiResult};
use crate::jobs;
use crate::lock;
use crate::names::node_name;
use crate::state::{AppState, Event};
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use base64::Engine;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
use time::Date;
use uuid::Uuid;

#[derive(Deserialize, Clone)]
pub struct Login {
    password: Option<String>,
}

#[derive(Deserialize, Clone)]
pub struct Upstream {
    protocol: String,
    host: String,
    port: u16,
    username: Option<String>,
    password: Option<String>,
}

/// 订阅只要到期日；其余是「单独设置」，没给 = 跟随套餐 / 供应商（同 PUT subscription）。没有套餐（含同机 IP 节点）要给 price_cents
#[derive(Deserialize, Clone)]
pub struct SubIn {
    due_date: Date,
    #[serde(flatten)]
    ov: super::machines::SubOverrides,
}

#[derive(Deserialize, Clone)]
pub struct Item {
    kind: String,
    ip: String,
    ssh_port: Option<u16>,
    team_id: Option<Uuid>,
    number: Option<i32>,
    label: Option<String>,
    provider_id: Option<Uuid>,
    plan_id: Option<Uuid>,
    service_ref: Option<String>,
    /// 国家（ISO 3166-1 两位代码，名字旁的国旗）；不填 = 按 IP 自动定
    country: Option<String>,
    note: Option<String>,
    relay_id: Option<Uuid>,
    login: Option<Login>,
    upstream: Option<Upstream>,
    /// 供应商映射到节点 ssserver 的公网端口（`ip` 上的端口）。不填 = 8388；填了不是 8388（同号映射：公网 P → 机器 P）时，节点上多一条把 P 转到 8388 的 nft 规则
    ss_port: Option<u16>,
    /// 出口 IP：端口转发型 NAT 节点入口 IP（`ip`，线路机连它）与出站公网 IP 不同时填；供应商的出口会变的填 "auto"（动态，只验收能出网）。只用于 VPS 节点（不含同机 IP 节点）；和 `ip` 相同等于不填
    egress_ip: Option<String>,
    subscription: Option<SubIn>,
    /// 机器重装过、主机指纹变了：允许预检时覆盖已记录的指纹
    reset_host_key: Option<bool>,
    /// 同机 IP 节点：加在这台 VPS 节点（宿主机）上，SSH 与 agent 用宿主机的；`ip` 是它自己的出口 IP，要已配在宿主机网卡上。
    /// 这时 ssh_port、login、ss_port、provider_id、plan_id、reset_host_key 不适用（忽略）：SSH 端口与供应商取宿主机的、不带套餐，SS 端口由中控分配
    host_id: Option<Uuid>,
    /// 入口模式 normal / reverse（穿透，见 crate::reverse）；不填 = 面板设置的新入口默认模式。家宽代理、同机 IP 节点不支持穿透，
    /// 自动按普通建（预览的 warnings、作业日志里写明）；线路机 agent 太旧等作业到建入口时才知道的，同样退回普通并记日志
    entry_mode: Option<String>,
}

#[derive(Deserialize)]
pub struct Batch {
    items: Vec<Item>,
}

/// 宿主机（同机 IP 节点的 host_id 指向的机器）
#[derive(sqlx::FromRow)]
struct HostRow {
    id: Uuid,
    kind: String,
    lifecycle: String,
    host_id: Option<Uuid>,
    provider_id: Option<Uuid>,
    name: String,
}

struct Checked {
    item: Item,
    ip: IpAddr,
    number: Option<i32>,
    name: String,
    /// 同机 IP 节点：宿主机的供应商
    host: Option<Option<Uuid>>,
    /// 同机 IP 节点暂定的 SS 端口（作业在宿主机上核对时发现被占用会改）
    ss_port: Option<u16>,
    /// 规范化后的出口 IP 设置（Follow = 出口就是 ip；Auto = 动态）
    egress: Egress,
    /// 所选套餐的供应商
    plan_provider: Option<Uuid>,
    /// 规范化后的国家（None = 按 IP 自动）
    country: Option<String>,
    errors: Vec<String>,
    /// 不影响通过的提醒（发行版已过标准支持期、试登录没有结果、要穿透但不支持）：只有「检查」会填
    warnings: Vec<String>,
    /// 新入口的模式（家宽代理、同机 IP 节点已退回普通；线路机为 None）与退回的原因
    entry_mode: Option<&'static str>,
    entry_mode_note: Option<String>,
}

/// 定下每台的入口模式：给了的用给的（写错了报错），没给的按面板的默认模式；家宽代理、同机 IP 节点要穿透时退回普通并记原因
async fn entry_modes(st: &AppState, v: &mut [Checked]) -> ApiResult<()> {
    let (default, _) = crate::reverse::default_mode(&st.db).await?;
    for c in v.iter_mut().filter(|c| c.item.kind != "relay") {
        let want = match c.item.entry_mode.as_deref().filter(|m| !m.trim().is_empty()) {
            Some(m) => match crate::reverse::parse_mode(m) {
                Ok(m) => m,
                Err(e) => {
                    c.errors.push(e);
                    continue;
                }
            },
            None => default,
        };
        let mut why: Option<String> = None;
        if want == crate::reverse::REVERSE {
            why = crate::reverse::kind_unsupported(&c.item.kind, c.item.host_id).map(String::from);
            if why.is_none() {
                if let Some(relay) = c.item.relay_id {
                    why = crate::reverse::relay_unsupported(&st.db, relay).await?;
                }
            }
        }
        c.entry_mode = Some(if why.is_some() { crate::reverse::NORMAL } else { want });
        c.entry_mode_note = why.map(|w| format!("按普通模式建入口：{w}"));
    }
    Ok(())
}

impl Item {
    /// 填了 root 密码（同 create 写进作业参数的条件）
    fn has_password(&self) -> bool { self.login.as_ref().and_then(|l| l.password.as_deref()).is_some_and(|p| !p.is_empty()) }
}

/// SS 密码：32 位，去掉易混字符
pub(super) fn ss_password() -> String {
    use rand::Rng;
    const A: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";
    let mut r = rand::thread_rng();
    (0..32).map(|_| A[r.gen_range(0..A.len())] as char).collect()
}

async fn validate(tx: &mut Tx<'_>, items: &[Item]) -> ApiResult<Vec<Checked>> {
    if items.is_empty() || items.len() > 100 {
        return Err(ApiError::bad_request("一次上架 1–100 台"));
    }
    // 锁住涉及的团队，防止并发分配同一编号
    let teams: Vec<Uuid> = items.iter().filter_map(|i| i.team_id).collect::<HashSet<_>>().into_iter().collect();
    let team_rows: Vec<(Uuid, String)> = sqlx::query_as("select id, name from team where id = any($1) order by id for update")
        .bind(&teams)
        .fetch_all(&mut **tx)
        .await?;
    let team_name: HashMap<Uuid, String> = team_rows.into_iter().collect();
    // 同机 IP 节点的宿主机：在团队之后锁住（和退役宿主机互斥；同一台宿主机上的 SS 端口一个事务一个事务地分）
    let host_ids: Vec<Uuid> = items.iter().filter_map(|i| i.host_id).collect::<HashSet<_>>().into_iter().collect();
    let host_rows: Vec<HostRow> = sqlx::query_as(
        "select m.id, m.kind::text kind, m.lifecycle::text lifecycle, m.host_id, m.provider_id,
                display_name(m.kind::text, m.label, t.name, m.number) name
         from machine m left join team t on t.id = m.team_id where m.id = any($1) order by m.id for update of m",
    )
    .bind(&host_ids)
    .fetch_all(&mut **tx)
    .await?;
    let mut host_ports: HashMap<Uuid, HashSet<u16>> = HashMap::new();
    for h in &host_rows {
        host_ports.insert(h.id, crate::db::ip_node_ports(tx, h.id, None).await?);
    }
    let hosts: HashMap<Uuid, HostRow> = host_rows.into_iter().map(|h| (h.id, h)).collect();
    // 每个团队已占用的编号（只算未退役的；退役的号立即回收）
    let taken_rows: Vec<(Uuid, i32)> = sqlx::query_as(
        "select team_id, number from machine where team_id = any($1) and number is not null and lifecycle <> 'retired'",
    )
    .bind(&teams)
    .fetch_all(&mut **tx)
    .await?;
    let mut taken: HashMap<Uuid, HashSet<i32>> = HashMap::new();
    for (t, n) in taken_rows {
        taken.entry(t).or_default().insert(n);
    }
    // 未退役机器的 IP → 机器显示名（被占用时告诉用户是哪台）
    let live_ips: Vec<(String, String)> = sqlx::query_as(
        "select host(m.primary_ip), display_name(m.kind::text, m.label, t.name, m.number)
         from machine m left join team t on t.id = m.team_id where m.lifecycle <> 'retired'",
    )
    .fetch_all(&mut **tx)
    .await?;
    let live_ips: HashMap<String, String> = live_ips.into_iter().collect();
    let relays: Vec<(Uuid,)> = sqlx::query_as("select id from machine where kind = 'relay' and lifecycle = 'active'").fetch_all(&mut **tx).await?;
    let relays: HashSet<Uuid> = relays.into_iter().map(|(r,)| r).collect();
    let labels: Vec<(String,)> =
        sqlx::query_as("select label from machine where kind = 'relay' and lifecycle <> 'retired'").fetch_all(&mut **tx).await?;
    let mut labels: HashSet<String> = labels.into_iter().map(|(s,)| s).collect();
    // 在用套餐 → 它的供应商
    let plans: Vec<(Uuid, Uuid)> = sqlx::query_as("select id, provider_id from plan where archived_at is null").fetch_all(&mut **tx).await?;
    let plans: HashMap<Uuid, Uuid> = plans.into_iter().collect();
    let providers: Vec<(Uuid,)> = sqlx::query_as("select id from provider").fetch_all(&mut **tx).await?;
    let providers: HashSet<Uuid> = providers.into_iter().map(|(p,)| p).collect();

    // 先收下手动指定的编号，再给空着的自动分配
    let mut batch_ips: HashSet<IpAddr> = HashSet::new();
    let mut out: Vec<Checked> = vec![];
    for it in items {
        let mut errors = vec![];
        let ip = match it.ip.trim().parse::<IpAddr>() {
            Ok(ip) => ip,
            Err(_) => {
                errors.push(format!("IP「{}」不合法", it.ip));
                IpAddr::from([0, 0, 0, 0])
            }
        };
        if let Some(owner) = live_ips.get(&ip.to_string()) {
            errors.push(format!("这个 IP 已经属于 {owner}"));
        }
        if !batch_ips.insert(ip) {
            errors.push("这一批里 IP 重复".into());
        }
        // 节点防火墙的放行集合是 ipv4_addr、ssserver 只监听 0.0.0.0：线路机和 VPS 节点目前只支持 IPv4
        // （家宽代理不受限，它的出站地址另填）
        if ip.is_ipv6() && matches!(it.kind.as_str(), "vps" | "relay") {
            errors.push("暂不支持 IPv6 地址：线路机和 VPS 节点的防火墙放行名单、SS 监听都只处理 IPv4，请填 IPv4 地址".into());
        }
        match it.kind.as_str() {
            "vps" | "proxy" => {
                match it.team_id {
                    Some(t) if team_name.contains_key(&t) => {}
                    _ => errors.push("要选团队".into()),
                }
                match it.relay_id {
                    Some(r) if relays.contains(&r) => {}
                    _ => errors.push("要选一台在用的线路机".into()),
                }
                if it.kind == "proxy" {
                    match &it.upstream {
                        Some(u) if matches!(u.protocol.as_str(), "socks5" | "http") && !u.host.trim().is_empty() && u.port > 0 => {}
                        _ => errors.push("家宽代理要填上游协议（socks5/http）、主机和端口".into()),
                    }
                }
                if let Some(n) = it.number {
                    if !(1..=999).contains(&n) {
                        errors.push("编号范围 1–999".into());
                    } else if let Some(t) = it.team_id {
                        if !taken.entry(t).or_default().insert(n) {
                            errors.push(format!("编号 {n} 已被占用"));
                        }
                    }
                }
            }
            "relay" => {
                let l = it.label.as_deref().unwrap_or("").trim().to_string();
                if l.is_empty() {
                    errors.push("线路机要填名称".into());
                } else if !labels.insert(l) {
                    errors.push("线路机名称重复".into());
                }
            }
            k => errors.push(format!("类型「{k}」不对（vps / proxy / relay）")),
        }
        if let Err(e) = super::machines::check_text(it.label.as_deref().map(str::trim), it.note.as_deref()) {
            errors.push(e);
        }
        let country = crate::country::normalize(it.country.as_deref().unwrap_or("")).unwrap_or_else(|e| {
            errors.push(e);
            None
        });
        // 同机 IP 节点不带套餐（填了也忽略）
        let plan = it.plan_id.filter(|_| it.host_id.is_none());
        if let Some(p) = plan {
            match plans.get(&p) {
                None => errors.push("套餐不存在或已归档".into()),
                // 有套餐时供应商就是套餐的
                Some(pv) if it.provider_id.is_some_and(|x| x != *pv) => errors.push("套餐不属于所选的供应商".into()),
                Some(_) => {}
            }
            // 选了套餐就建订阅（费用统计、到期提醒要它）：价格等跟随套餐，只要到期日
            if it.subscription.is_none() {
                errors.push("选了套餐要填到期日（费用、流量跟随套餐）".into());
            }
        }
        if let Some(p) = it.provider_id {
            if !providers.contains(&p) {
                errors.push("供应商不存在".into());
            }
        }
        // 没有套餐时价格没有可跟随的
        if it.subscription.as_ref().is_some_and(|s| !s.ov.has_price()) && plan.is_none() {
            errors.push("没有套餐：订阅要填费用（免费填 0）".into());
        }
        if let Some(s) = &it.subscription {
            // 时区不认时 PG 报错会让事务作废：放在保存点里校验
            let mut sp = sqlx::Connection::begin(&mut **tx).await?;
            if let Err(e) = s.ov.clone().check(&mut sp).await {
                errors.push(e.message);
            }
            sp.rollback().await?;
        }
        let (mut host, mut ss_port) = (None, None);
        if let Some(hid) = it.host_id {
            match hosts.get(&hid) {
                None => errors.push("宿主机不存在".into()),
                Some(_) if it.kind != "vps" => errors.push("只有 VPS 节点能加在宿主机上".into()),
                Some(h) if h.kind != "vps" || h.host_id.is_some() => {
                    errors.push(format!("「{}」不能当宿主机：宿主机要是普通 VPS 节点（不能是线路机、家宽代理或同机 IP 节点）", h.name))
                }
                Some(h) if h.lifecycle != "active" => {
                    errors.push(format!("宿主机「{}」现在是「{}」，只能加在运行中的 VPS 节点上", h.name, crate::meta::lifecycle_label(&h.lifecycle)))
                }
                Some(h) => {
                    let taken = host_ports.entry(hid).or_default();
                    ss_port = crate::db::free_ss_port(taken);
                    match ss_port {
                        Some(p) => {
                            taken.insert(p);
                        }
                        None => errors.push(format!("宿主机「{}」上没有空闲的 SS 端口", h.name)),
                    }
                    host = Some(h.provider_id);
                }
            }
        }
        // 出口 IP：只给 VPS 节点（不含同机 IP 节点）；和 IP 相同按没填
        let egress = match it.egress_ip.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            None => Egress::Follow,
            Some(_) if it.kind != "vps" || it.host_id.is_some() => {
                errors.push("出口 IP 只用于 VPS 节点（线路机、家宽代理、同机 IP 节点不用）".into());
                Egress::Follow
            }
            Some(raw) => super::machines::parse_egress(raw, &ip.to_string()).unwrap_or_else(|e| {
                errors.push(e);
                Egress::Follow
            }),
        };
        let plan_provider = plan.and_then(|p| plans.get(&p).copied());
        out.push(Checked { item: it.clone(), ip, number: it.number, name: String::new(), host, ss_port, egress, plan_provider, country, errors, warnings: vec![], entry_mode: None, entry_mode_note: None });
    }
    for c in out.iter_mut() {
        if c.item.kind == "relay" {
            c.name = c.item.label.clone().unwrap_or_default().trim().to_string();
            continue;
        }
        let Some(t) = c.item.team_id else { continue };
        let Some(tn) = team_name.get(&t) else { continue };
        if c.number.is_none() {
            let set = taken.entry(t).or_default();
            let n = (1..=999).find(|n| !set.contains(n));
            match n {
                Some(n) => {
                    set.insert(n);
                    c.number = Some(n);
                }
                None => c.errors.push("这个团队的编号用完了".into()),
            }
        }
        if let Some(n) = c.number {
            c.name = node_name(tn, n);
        }
    }
    Ok(out)
}

fn report(v: &[Checked]) -> Value {
    json!({"items": v.iter().map(|c| json!({
        "ok": c.errors.is_empty(), "errors": c.errors, "warnings": c.warnings, "name": c.name, "number": c.number, "ss_port": c.ss_port,
        "entry_mode": c.entry_mode,
    })).collect::<Vec<_>>()})
}

/// 「检查」里探测 SSH 端口的限速，按操作人：一批最多 100 台，连着查三批没问题，之后每秒补 1 次。
/// 挡的是拿「检查」当端口扫描器扫中控所在内网（一次 100 个地址、不落库不留审计）；真正上架要落库、记审计，不在此列
const PROBE_BURST: f64 = 300.0;
const PROBE_REFILL_PER_S: f64 = 1.0;
/// 同一批里同时探测的个数
const PROBE_CONCURRENCY: usize = 8;
static PROBE_BUDGET: LazyLock<Mutex<HashMap<String, (f64, Instant)>>> = LazyLock::new(Default::default);

/// 扣 `n` 次探测；不够时整批不扣，返回还要等几秒
fn take_probes(m: &mut HashMap<String, (f64, Instant)>, who: &str, n: usize, now: Instant) -> Result<(), u64> {
    let (tokens, at) = m.entry(who.to_string()).or_insert((PROBE_BURST, now));
    *tokens = (*tokens + now.saturating_duration_since(*at).as_secs_f64() * PROBE_REFILL_PER_S).min(PROBE_BURST);
    *at = now;
    if *tokens < n as f64 {
        return Err(((n as f64 - *tokens) / PROBE_REFILL_PER_S).ceil().max(1.0) as u64);
    }
    *tokens -= n as f64;
    Ok(())
}

/// 不是一台机器的地址：回环、链路本地（含云元数据 169.254.169.254）、未指定（0.0.0.0、::）、组播。私网照常（实验室节点就在私网里）
fn not_a_host(ip: IpAddr) -> Option<&'static str> {
    match ip.to_canonical() {
        IpAddr::V4(a) if a.is_loopback() => Some("回环"),
        IpAddr::V4(a) if a.is_link_local() => Some("链路本地"),
        IpAddr::V4(a) if a.is_unspecified() => Some("未指定"),
        IpAddr::V4(a) if a.is_multicast() => Some("组播"),
        IpAddr::V6(a) if a.is_loopback() => Some("回环"),
        IpAddr::V6(a) if a.segments()[0] & 0xffc0 == 0xfe80 => Some("链路本地"),
        IpAddr::V6(a) if a.is_unspecified() => Some("未指定"),
        IpAddr::V6(a) if a.is_multicast() => Some("组播"),
        _ => None,
    }
}

pub async fn check(a: Actor, State(st): State<AppState>, ApiJson(b): ApiJson<Batch>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let mut v = validate(&mut tx, &b.items).await?;
    tx.rollback().await?;
    entry_modes(&st, &mut v).await?;
    for c in v.iter_mut() {
        if let Some(n) = c.entry_mode_note.clone() {
            c.warnings.push(n);
        }
    }
    // 只查库的校验之外再看一眼 SSH 端口通不通（要读到 SSH 版本行，只连上 TCP 不算）：地址填错、防火墙没放行在这里就能看出来，不用等上架作业失败。
    // 通了再用中控密钥试登录一次（`ssh_check`）：没填 root 密码又登不进的，作业第一步一定失败，在这里就报，并给出中控公钥；
    // 登得进的读发行版，已过标准支持期的给警告（不影响通过）。
    // 家宽代理不 SSH；同机 IP 节点登录的是宿主机（在用的，不用再看）；不是一台机器的地址直接报错，不去连
    let mut probe = vec![];
    for (i, c) in v.iter_mut().enumerate() {
        if !c.errors.is_empty() || c.item.kind == "proxy" || c.host.is_some() {
            continue;
        }
        match not_a_host(c.ip) {
            Some(what) => c.errors.push(format!("IP {} 是{what}地址，不能上架", c.ip)),
            None => probe.push(i),
        }
    }
    // 同机 IP 节点：登录宿主机看这个 IP 配没配在它的网卡上、不在时是不是 1:1 NAT 映射到哪个本机地址上（同上架作业的第一步，同一段脚本），不用等作业失败。
    // 每台宿主机只登录一次，拿到的本机 IP 对照它上面的全部条目；每台宿主机也算一次探测
    let on_host: Vec<usize> = (0..v.len()).filter(|&i| v[i].errors.is_empty() && v[i].host.is_some()).collect();
    let host_ids: Vec<Uuid> = on_host.iter().filter_map(|&i| v[i].item.host_id).collect::<HashSet<_>>().into_iter().collect();
    if let Err(wait) = take_probes(&mut lock(&PROBE_BUDGET), &a.name, probe.len() + host_ids.len(), Instant::now()) {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            format!("检查太频繁（每次检查都要探测各台机器的 SSH 端口），{wait} 秒后再试"),
        ));
    }
    let host_rows: Vec<(Uuid, String, i32, String)> = sqlx::query_as(
        "select m.id, host(m.primary_ip), m.ssh_port, display_name(m.kind::text, m.label, t.name, m.number)
         from machine m left join team t on t.id = m.team_id where m.id = any($1)",
    )
    .bind(&host_ids)
    .fetch_all(&st.db)
    .await?;
    if !host_rows.is_empty() {
        tracing::info!(actor = %a.name, hosts = ?host_rows.iter().map(|r| &r.3).collect::<Vec<_>>(), "上架检查：登录宿主机核对同机 IP 节点的 IP");
    }
    // 整个检查最多等 CHECK_TIMEOUT：到点还没查完的条目报超时（排在后面、还没轮到的也一样），不让一次检查挂几分钟
    let deadline = tokio::time::Instant::now() + CHECK_TIMEOUT;
    let timed_out = || format!("检查超时（{} 秒内没查完），稍后再检查", CHECK_TIMEOUT.as_secs());
    let targets: Vec<(String, u16)> = probe.iter().map(|&i| (v[i].ip.to_string(), v[i].item.ssh_port.unwrap_or(22))).collect();
    // 每个探测拿自己的一份地址与密钥路径（借用 targets 的闭包过不了 axum 处理函数的生命周期检查）
    let ports = futures::stream::iter(targets.clone())
        .map(|(ip, port)| {
            let key = st.cfg.ssh_key.clone();
            async move { tokio::time::timeout_at(deadline, ssh_check(&ip, port, &key)).await.unwrap_or_else(|_| Err(timed_out())) }
        })
        .buffered(PROBE_CONCURRENCY)
        .collect::<Vec<_>>();
    // 每台宿主机上要核对的公网 IP：有不在网卡上的，脚本才逐个本机地址探测 NAT 映射
    let mut want: HashMap<Uuid, Vec<String>> = HashMap::new();
    for &i in &on_host {
        if let Some(h) = v[i].item.host_id {
            want.entry(h).or_default().push(v[i].ip.to_string());
        }
    }
    let echo = format!("{}/echo", st.cfg.public_url.trim_end_matches('/'));
    let logins = futures::stream::iter(host_rows)
        .map(|(id, ip, port, name)| {
            let t = crate::ssh::Target { ip, port: port as u16, key: st.cfg.ssh_key.clone(), known_hosts: st.cfg.known_hosts.clone(), password: None };
            let script = jobs::scripts::vars(&[("WANT", &want.get(&id).map(|w| w.join(",")).unwrap_or_default()), ("ECHO", &echo)]) + jobs::scripts::HOST_IPS;
            async move { (id, (tokio::time::timeout_at(deadline, host_ips(&t, &name, &script)).await.unwrap_or_else(|_| Err(timed_out())), name)) }
        })
        .buffered(PROBE_CONCURRENCY)
        .collect::<HashMap<_, _>>();
    let (ports, logins) = futures::join!(ports, logins);
    // 「录入中控公钥」的说明（带公钥全文）只在有条目登不进时读一次
    let mut key_hint: Option<String> = None;
    for ((&i, (ip, port)), r) in probe.iter().zip(&targets).zip(ports) {
        let c = &mut v[i];
        let pw = c.item.has_password();
        match r {
            Err(why) => c.errors.push(format!("SSH {ip}:{port} {why}")),
            Ok(KeyLogin::Os(os)) => c.warnings.extend(jobs::scripts::os_eol_warning(&os)),
            // 填了 root 密码的照旧：作业会用密码登录并装上中控公钥
            Ok(_) if pw => {}
            Ok(KeyLogin::Denied) => {
                if key_hint.is_none() {
                    key_hint = Some(crate::ssh::add_key_hint(&st.cfg.ssh_key).await);
                }
                let hint = key_hint.as_deref().unwrap_or_default();
                c.errors.push(format!(
                    "中控密钥登录不了 {ip}:{port}（密钥被拒），也没填 root 密码。二选一：提供 root 密码（网页填「root 密码」，fleetctl 用 --ssh-password-env 变量名）；或{hint}"
                ));
            }
            Ok(KeyLogin::Unsure(why)) => c.warnings.push(format!("用中控密钥试登录 {ip}:{port} 没有结果（{why}），上架作业第一步会再登录一次")),
        }
    }
    for &i in &on_host {
        let c = &mut v[i];
        // 校验之后宿主机被删了的，查不到行
        let Some((r, host)) = c.item.host_id.and_then(|h| logins.get(&h)) else {
            c.errors.push("宿主机不存在".into());
            continue;
        };
        // 在网卡上、或恰好一个本机地址经 NAT 出去是它，都算通过（bind 地址由上架作业的第一步再核对一次并记下）
        let e = match r {
            Ok((ips, nat)) => jobs::scripts::bind_addr(&c.ip.to_string(), host, ips, nat).err(),
            Err(e) => Some(e.clone()),
        };
        c.errors.extend(e);
    }
    Ok(ok(report(&v)))
}

/// 「检查」整体的时限（端口探测、试登录与登录宿主机一起算）
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
/// 用中控密钥试登录一台的时限（整个 ssh 进程；连接超时另设 5 秒）
const KEY_LOGIN_TIMEOUT: Duration = Duration::from_secs(10);

/// 用中控密钥试登录的结果
enum KeyLogin {
    /// 登进去了：os-release 的 `ID-VERSION_ID`（同 FACTS 的 `os=`）
    Os(String),
    /// 密钥被拒：机器上还没录入中控公钥（或不许 root 用密钥登录）
    Denied,
    /// 别的问题（ssh 超时、连上后被断开、脚本没输出…）：说不准，只提醒
    Unsure(String),
}

/// 一台机器的 SSH 检查：端口要说 SSH（`ssh_probe`，连不上时不用等 ssh 自己超时），再用中控密钥试登录一次读发行版。
/// 端口不通为 Err（给人看的原因，接在「SSH ip:port」后面）。填没填 root 密码都试：登得进时才能提醒过期的发行版
async fn ssh_check(ip: &str, port: u16, key: &std::path::Path) -> Result<KeyLogin, String> {
    crate::ssh::ssh_probe(ip, port, Duration::from_secs(3)).await?;
    let out = match crate::ssh::key_login_probe(ip, port, key, jobs::scripts::OS_RELEASE, KEY_LOGIN_TIMEOUT).await {
        Ok(o) => o,
        Err(e) => return Ok(KeyLogin::Unsure(format!("{e:#}"))),
    };
    Ok(match (out.ok(), out.ssh_kind(), out.kv("os")) {
        (true, _, Some(os)) => KeyLogin::Os(os.to_string()),
        (_, Some(crate::ssh::ErrKind::Auth), _) => KeyLogin::Denied,
        _ => {
            let detail = if out.stderr.trim().is_empty() { out.stdout.trim() } else { out.stderr.trim() };
            KeyLogin::Unsure(if detail.is_empty() { format!("退出码 {}", out.code) } else { crate::jobs::clip(detail).into_owned() })
        }
    })
}

/// 登录宿主机 `host` 跑 `script`（`HOST_IPS`），取它网卡上的 IPv4 与（要核对的 IP 有不在网卡上的时）各本机地址经 NAT 出去的 IP：
/// `(local_ips, nat)`。先看 SSH 端口（连不上时不用等 ssh 自己超时），再登录。
/// 只读、不改宿主机，不写操作记录；登录过哪些宿主机记在中控的服务日志里
async fn host_ips(t: &crate::ssh::Target, host: &str, script: &str) -> Result<(String, String), String> {
    crate::ssh::ssh_probe(&t.ip, t.port, Duration::from_secs(3)).await.map_err(|why| format!("宿主机 {host} 的 SSH {}:{} {why}", t.ip, t.port))?;
    let out = t.run_once(script, Duration::from_secs(15)).await.map_err(|e| format!("登录宿主机 {host} 核对 IP 失败：{e:#}"))?;
    if !out.ok() {
        return Err(format!("登录宿主机 {host} 核对 IP 失败：{}", out.stderr.trim()));
    }
    Ok((out.kv("local_ips").unwrap_or("").to_string(), out.kv("nat").unwrap_or("").to_string()))
}

pub async fn create(a: Actor, State(st): State<AppState>, ApiJson(b): ApiJson<Batch>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let mut v = validate(&mut tx, &b.items).await?;
    entry_modes(&st, &mut v).await?;
    if v.iter().any(|c| !c.errors.is_empty()) {
        return Err(ApiError::new(axum::http::StatusCode::UNPROCESSABLE_ENTITY, "invalid", "有机器没通过校验，请先检查").with_detail(report(&v)));
    }
    let mut created = vec![];
    for c in &v {
        let it = &c.item;
        let id = new_id();
        let kind = it.kind.as_str();
        // 同机 IP 节点：SSH 端口不存（读时取宿主机的），供应商取宿主机的，不带套餐（费用默认不记，要记到它的「费用」里单独填）
        // 有套餐时供应商取套餐的（校验已保证一致）
        let (ssh_port, provider, plan) = match c.host {
            Some(pv) => (None, pv, None),
            None => (Some(it.ssh_port.unwrap_or(22) as i32), c.plan_provider.or(it.provider_id), it.plan_id),
        };
        sqlx::query(
            "insert into machine (id, kind, team_id, number, label, primary_ip, ssh_port, provider_id, plan_id, service_ref, note, host_id, country, egress_ip, egress_dynamic, lifecycle)
             values ($1, $2::machine_kind, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, 'provisioning')",
        )
        .bind(id)
        .bind(kind)
        .bind(if kind == "relay" { None } else { it.team_id })
        .bind(if kind == "relay" { None } else { c.number })
        .bind(if kind == "relay" { Some(c.name.clone()) } else { None })
        .bind(ipnetwork::IpNetwork::from(c.ip))
        .bind(ssh_port)
        .bind(provider)
        .bind(plan)
        .bind(&it.service_ref)
        .bind(it.note.as_deref().unwrap_or(""))
        .bind(it.host_id)
        .bind(&c.country)
        .bind(c.egress.ip().map(|ip| ipnetwork::IpNetwork::from(IpAddr::V4(ip))))
        .bind(c.egress.is_auto())
        .execute(&mut *tx)
        .await?;
        match kind {
            "vps" => {
                sqlx::query(
                    "insert into node_outbound (machine_id, protocol, host, port, method, secret_enc) values ($1, 'shadowsocks', $2, $3, $5, $4)",
                )
                .bind(id)
                .bind(c.ip.to_string())
                .bind(c.ss_port.or(it.ss_port).unwrap_or(crate::meta::SS_PORT) as i32)
                .bind(st.secrets.seal_str(&ss_password()))
                .bind(crate::meta::SS_METHOD)
                .execute(&mut *tx)
                .await?;
            }
            "proxy" => {
                let u = it.upstream.as_ref().expect("validated");
                sqlx::query(
                    "insert into node_outbound (machine_id, protocol, host, port, username, secret_enc) values ($1, $2, $3, $4, $5, $6)",
                )
                .bind(id)
                .bind(&u.protocol)
                .bind(u.host.trim())
                .bind(u.port as i32)
                .bind(&u.username)
                .bind(st.secrets.seal_str(u.password.as_deref().unwrap_or("")))
                .execute(&mut *tx)
                .await?;
            }
            _ => {
                let (sk, pk) = reality_keypair();
                // generation 从 1 开始：agent 第一次就会拿到（空的）期望状态并生成配置
                sqlx::query(
                    "insert into relay (machine_id, public_host, reality_private_enc, reality_public_key, min_client_ver, generation)
                     values ($1, $2, $3, $4, '1.8.2', 1)",
                )
                .bind(id)
                .bind(c.ip.to_string())
                .bind(st.secrets.seal_str(&sk))
                .bind(pk)
                .execute(&mut *tx)
                .await?;
            }
        }
        // 账单：只存到期日和给了的单独设置，其余跟随套餐（价格、币种、周期、流量）与供应商（时区、流量重置），不再抄一份
        if let Some(s) = &it.subscription {
            let mut ov = s.ov.clone();
            ov.check(&mut tx).await?;
            ov.write(&mut tx, id, s.due_date).await?;
        }
        let mut params = json!({"relay_id": it.relay_id});
        // 入口模式（建入口那一步再按线路机 / 节点 agent 核一次，用不了穿透时退回普通）
        if let Some(m) = c.entry_mode {
            params["entry_mode"] = json!(m);
        }
        if let Some(n) = &c.entry_mode_note {
            params["entry_mode_note"] = json!(n);
        }
        // 同机 IP 节点用中控密钥登录宿主机：不收 root 密码，也不重置宿主机的主机指纹
        if c.host.is_none() {
            if it.reset_host_key == Some(true) {
                params["reset_host_key"] = json!(true);
            }
            if let Some(pw) = it.login.as_ref().and_then(|l| l.password.as_deref()).filter(|p| !p.is_empty()) {
                let enc = base64::engine::general_purpose::STANDARD.encode(st.secrets.seal_str(pw));
                params["login"] = json!({"password_enc": enc});
            }
        }
        let job_kind = if c.host.is_some() { "provision_ip".to_string() } else { format!("provision_{kind}") };
        let job = jobs::enqueue(&mut tx, &job_kind, Some(id), params, &a.name).await?;
        audit(
            &mut tx,
            &a.name,
            "machine.provision",
            "machine",
            Some(id),
            json!({"name": c.name, "ip": c.ip.to_string(), "kind": kind, "job_id": job, "host_id": it.host_id, "egress_ip": c.egress.label()}),
        )
        .await?;
        created.push(json!({"machine_id": id, "job_id": job, "name": c.name, "entry_mode": c.entry_mode, "entry_mode_note": c.entry_mode_note}));
    }
    tx.commit().await?;
    st.wake_jobs();
    st.emit(Event::Machines);
    Ok(ok(json!({"jobs": created})))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 探测限速：一次扣一批，不够时整批不扣、给要等的秒数；按秒补充
    #[test]
    fn probe_budget() {
        let (mut m, t0) = (HashMap::new(), Instant::now());
        assert_eq!(take_probes(&mut m, "admin", 100, t0), Ok(()));
        assert_eq!(take_probes(&mut m, "admin", 200, t0), Ok(()));
        assert_eq!(take_probes(&mut m, "admin", 8, t0), Err(8));
        assert_eq!(take_probes(&mut m, "apikey:ops", 100, t0), Ok(()), "按操作人分开记");
        assert_eq!(take_probes(&mut m, "admin", 8, t0 + Duration::from_secs(8)), Ok(()));
        assert_eq!(take_probes(&mut m, "admin", 0, t0 + Duration::from_secs(8)), Ok(()), "不用探测的批次不受限");
    }

    /// 回环、链路本地、未指定、组播不去连；私网、公网照常
    #[test]
    fn not_a_host_addresses() {
        for (ip, want) in [
            ("127.0.0.1", Some("回环")),
            ("::1", Some("回环")),
            ("::ffff:127.0.0.1", Some("回环")),
            ("169.254.169.254", Some("链路本地")),
            ("fe80::1", Some("链路本地")),
            ("0.0.0.0", Some("未指定")),
            ("::", Some("未指定")),
            ("224.0.0.1", Some("组播")),
            ("ff02::1", Some("组播")),
            ("192.168.31.66", None),
            ("10.0.0.2", None),
            ("203.0.113.7", None),
        ] {
            assert_eq!(not_a_host(ip.parse().unwrap()), want, "{ip}");
        }
    }
}
