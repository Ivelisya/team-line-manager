//! 上架：一批机器先校验（check），全部通过再创建（create）。创建 = 机器（provisioning）+ 秘密 + 账单 + 作业。

use crate::error::ApiJson;
use super::ok;
use crate::auth::Actor;
use crate::crypto::reality_keypair;
use crate::db::{audit, new_id, Tx};
use crate::error::{ApiError, ApiResult};
use crate::jobs;
use crate::names::node_name;
use crate::state::{AppState, Event};
use axum::extract::State;
use axum::Json;
use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
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

#[derive(Deserialize, Clone)]
pub struct SubIn {
    price_cents: Option<i32>,
    currency: Option<String>,
    due_date: Date,
    cycle: Option<String>,
    billing_tz: Option<String>,
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
    note: Option<String>,
    relay_id: Option<Uuid>,
    login: Option<Login>,
    upstream: Option<Upstream>,
    ss_port: Option<u16>,
    subscription: Option<SubIn>,
    /// 机器重装过、主机指纹变了：允许预检时覆盖已记录的指纹
    reset_host_key: Option<bool>,
    /// 同机 IP 节点：加在这台落地机（宿主机）上，SSH 与 agent 用宿主机的；`ip` 是它自己的出口 IP，要已配在宿主机网卡上。
    /// 这时 ssh_port、login、ss_port、provider_id、plan_id、reset_host_key 不适用（忽略）：SSH 端口与供应商取宿主机的、不带套餐，SS 端口由中控分配
    host_id: Option<Uuid>,
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
    ssh_port: i32,
    provider_id: Option<Uuid>,
    name: String,
}

struct Checked {
    item: Item,
    ip: IpAddr,
    number: Option<i32>,
    name: String,
    /// 同机 IP 节点：宿主机的 (SSH 端口, 供应商)
    host: Option<(i32, Option<Uuid>)>,
    /// 同机 IP 节点暂定的 SS 端口（作业在宿主机上核对时发现被占用会改）
    ss_port: Option<u16>,
    errors: Vec<String>,
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
        "select m.id, m.kind::text kind, m.lifecycle::text lifecycle, m.host_id, m.ssh_port, m.provider_id,
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
    let live_ips: Vec<(String,)> = sqlx::query_as("select host(primary_ip) from machine where lifecycle <> 'retired'").fetch_all(&mut **tx).await?;
    let live_ips: HashSet<String> = live_ips.into_iter().map(|(s,)| s).collect();
    let relays: Vec<(Uuid,)> = sqlx::query_as("select id from machine where kind = 'relay' and lifecycle = 'active'").fetch_all(&mut **tx).await?;
    let relays: HashSet<Uuid> = relays.into_iter().map(|(r,)| r).collect();
    let labels: Vec<(String,)> =
        sqlx::query_as("select label from machine where kind = 'relay' and lifecycle <> 'retired'").fetch_all(&mut **tx).await?;
    let mut labels: HashSet<String> = labels.into_iter().map(|(s,)| s).collect();
    let plans: Vec<(Uuid,)> = sqlx::query_as("select id from plan where archived_at is null").fetch_all(&mut **tx).await?;
    let plans: HashSet<Uuid> = plans.into_iter().map(|(p,)| p).collect();
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
        if live_ips.contains(&ip.to_string()) {
            errors.push("这个 IP 已经属于一台未退役的机器".into());
        }
        if !batch_ips.insert(ip) {
            errors.push("这一批里 IP 重复".into());
        }
        // 节点防火墙的放行集合是 ipv4_addr、ssserver 只监听 0.0.0.0：线路机和落地机目前只支持 IPv4
        // （家宽代理不受限，它的出站地址另填）
        if ip.is_ipv6() && matches!(it.kind.as_str(), "vps" | "relay") {
            errors.push("暂不支持 IPv6 地址：线路机和落地机的防火墙放行名单、SS 监听都只处理 IPv4，请填 IPv4 地址".into());
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
        if let Some(p) = it.plan_id {
            if !plans.contains(&p) {
                errors.push("套餐不存在或已归档".into());
            }
        }
        if let Some(p) = it.provider_id {
            if !providers.contains(&p) {
                errors.push("供应商不存在".into());
            }
        }
        let (mut host, mut ss_port) = (None, None);
        if let Some(hid) = it.host_id {
            match hosts.get(&hid) {
                None => errors.push("宿主机不存在".into()),
                Some(_) if it.kind != "vps" => errors.push("只有落地机（vps）能加在宿主机上".into()),
                Some(h) if h.kind != "vps" || h.host_id.is_some() => {
                    errors.push(format!("「{}」不能当宿主机：宿主机要是普通落地机（不能是线路机、家宽代理或同机 IP 节点）", h.name))
                }
                Some(h) if h.lifecycle != "active" => {
                    errors.push(format!("宿主机「{}」现在是「{}」，只能加在运行中的落地机上", h.name, jobs::lifecycle_zh(&h.lifecycle)))
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
                    host = Some((h.ssh_port, h.provider_id));
                }
            }
        }
        out.push(Checked { item: it.clone(), ip, number: it.number, name: String::new(), host, ss_port, errors });
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
    json!({"items": v.iter().map(|c| json!({"ok": c.errors.is_empty(), "errors": c.errors, "name": c.name, "number": c.number, "ss_port": c.ss_port})).collect::<Vec<_>>()})
}

pub async fn check(_a: Actor, State(st): State<AppState>, ApiJson(b): ApiJson<Batch>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let v = validate(&mut tx, &b.items).await?;
    tx.rollback().await?;
    Ok(ok(report(&v)))
}

pub async fn create(a: Actor, State(st): State<AppState>, ApiJson(b): ApiJson<Batch>) -> ApiResult<Json<Value>> {
    let mut tx = st.db.begin().await?;
    let v = validate(&mut tx, &b.items).await?;
    if v.iter().any(|c| !c.errors.is_empty()) {
        return Err(ApiError::new(axum::http::StatusCode::UNPROCESSABLE_ENTITY, "invalid", "有机器没通过校验，请先检查").with_detail(report(&v)));
    }
    let mut created = vec![];
    for c in &v {
        let it = &c.item;
        let id = new_id();
        let kind = it.kind.as_str();
        // 同机 IP 节点：SSH 端口（只展示）和供应商取宿主机的，不带套餐（费用默认不记，要记到它的「费用」里单独填）
        let (ssh_port, provider, plan) = match c.host {
            Some((p, pv)) => (p, pv, None),
            None => (it.ssh_port.unwrap_or(22) as i32, it.provider_id, it.plan_id),
        };
        sqlx::query(
            "insert into machine (id, kind, team_id, number, label, primary_ip, ssh_port, provider_id, plan_id, service_ref, note, host_id, lifecycle)
             values ($1, $2::machine_kind, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, 'provisioning')",
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
        .execute(&mut *tx)
        .await?;
        match kind {
            "vps" => {
                sqlx::query(
                    "insert into node_outbound (machine_id, protocol, host, port, method, secret_enc) values ($1, 'shadowsocks', $2, $3, 'aes-256-gcm', $4)",
                )
                .bind(id)
                .bind(c.ip.to_string())
                .bind(c.ss_port.or(it.ss_port).unwrap_or(8388) as i32)
                .bind(st.secrets.seal_str(&ss_password()))
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
        // 账单：价格 / 币种 / 周期没给的取套餐，时区没给的取供应商（机器的，否则套餐的）；都没有用列默认
        if let Some(s) = &it.subscription {
            if let Some(tz) = &s.billing_tz {
                super::machines::check_tz(&mut tx, tz).await?;
            }
            sqlx::query(
                "insert into subscription (machine_id, price_cents, currency, cycle, due_date, billing_tz)
                 select $1, coalesce($2, p.price_cents, 0), coalesce($3, p.currency, 'USD'), coalesce($4::billing_cycle, p.cycle, 'monthly'),
                        $5, coalesce($6, v.timezone, 'Asia/Shanghai')
                 from (select 1) one left join plan p on p.id = $7 left join provider v on v.id = coalesce($8, p.provider_id)",
            )
            .bind(id)
            .bind(s.price_cents)
            .bind(&s.currency)
            .bind(&s.cycle)
            .bind(s.due_date)
            .bind(&s.billing_tz)
            .bind(plan)
            .bind(provider)
            .execute(&mut *tx)
            .await?;
        }
        let mut params = json!({"relay_id": it.relay_id});
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
            json!({"name": c.name, "ip": c.ip.to_string(), "kind": kind, "job_id": job, "host_id": it.host_id}),
        )
        .await?;
        created.push(json!({"machine_id": id, "job_id": job, "name": c.name}));
    }
    tx.commit().await?;
    st.wake_jobs();
    st.emit(Event::Machines);
    Ok(ok(json!({"jobs": created})))
}
