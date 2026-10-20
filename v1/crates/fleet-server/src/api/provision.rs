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
}

#[derive(Deserialize)]
pub struct Batch {
    items: Vec<Item>,
}

struct Checked {
    item: Item,
    ip: IpAddr,
    number: Option<i32>,
    name: String,
    errors: Vec<String>,
}

/// SS 密码：32 位，去掉易混字符
fn ss_password() -> String {
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
        // 节点防火墙的放行集合是 ipv4_addr、ssserver 只监听 0.0.0.0：线路机和 VPS 节点目前只支持 IPv4
        // （上游代理不受限，它的出站地址另填）
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
                        _ => errors.push("上游代理要填协议（socks5/http）、地址和端口".into()),
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
        out.push(Checked { item: it.clone(), ip, number: it.number, name: String::new(), errors });
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
    json!({"items": v.iter().map(|c| json!({"ok": c.errors.is_empty(), "errors": c.errors, "name": c.name, "number": c.number})).collect::<Vec<_>>()})
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
        sqlx::query(
            "insert into machine (id, kind, team_id, number, label, primary_ip, ssh_port, provider_id, plan_id, service_ref, note, lifecycle)
             values ($1, $2::machine_kind, $3, $4, $5, $6, $7, $8, $9, $10, $11, 'provisioning')",
        )
        .bind(id)
        .bind(kind)
        .bind(if kind == "relay" { None } else { it.team_id })
        .bind(if kind == "relay" { None } else { c.number })
        .bind(if kind == "relay" { Some(c.name.clone()) } else { None })
        .bind(ipnetwork::IpNetwork::from(c.ip))
        .bind(it.ssh_port.unwrap_or(22) as i32)
        .bind(it.provider_id)
        .bind(it.plan_id)
        .bind(&it.service_ref)
        .bind(it.note.as_deref().unwrap_or(""))
        .execute(&mut *tx)
        .await?;
        match kind {
            "vps" => {
                sqlx::query(
                    "insert into node_outbound (machine_id, protocol, host, port, method, secret_enc) values ($1, 'shadowsocks', $2, $3, 'aes-256-gcm', $4)",
                )
                .bind(id)
                .bind(c.ip.to_string())
                .bind(it.ss_port.unwrap_or(8388) as i32)
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
        // 账单：显式给了就用；否则有套餐时按套餐价、到期日先空着（不建）
        if let Some(s) = &it.subscription {
            let price = match s.price_cents {
                Some(p) => p,
                None => match it.plan_id {
                    Some(p) => sqlx::query_scalar("select price_cents from plan where id = $1").bind(p).fetch_one(&mut *tx).await?,
                    None => 0,
                },
            };
            sqlx::query(
                "insert into subscription (machine_id, price_cents, cycle, due_date, billing_tz) values ($1, $2, $3::billing_cycle, $4, $5)",
            )
            .bind(id)
            .bind(price)
            .bind(s.cycle.as_deref().unwrap_or("monthly"))
            .bind(s.due_date)
            .bind(s.billing_tz.as_deref().unwrap_or("Asia/Shanghai"))
            .execute(&mut *tx)
            .await?;
        }
        let mut params = json!({"relay_id": it.relay_id});
        if it.reset_host_key == Some(true) {
            params["reset_host_key"] = json!(true);
        }
        if let Some(pw) = it.login.as_ref().and_then(|l| l.password.as_deref()).filter(|p| !p.is_empty()) {
            let enc = base64::engine::general_purpose::STANDARD.encode(st.secrets.seal_str(pw));
            params["login"] = json!({"password_enc": enc});
        }
        let job = jobs::enqueue(&mut tx, &format!("provision_{kind}"), Some(id), params, &a.name).await?;
        audit(&mut tx, &a.name, "machine.provision", "machine", Some(id), json!({"name": c.name, "ip": c.ip.to_string(), "kind": kind, "job_id": job})).await?;
        created.push(json!({"machine_id": id, "job_id": job, "name": c.name}));
    }
    tx.commit().await?;
    st.emit(Event::Machines);
    Ok(ok(json!({"jobs": created})))
}
