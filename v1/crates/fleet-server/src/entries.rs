//! 入口（线路机上的一条 VLESS REALITY 入站 → 节点）的领域逻辑：端口分配、期望状态、应用回报。
//!
//! 直出入口：节点就是线路机自己（`relay_id = node_id`），出站是 freedom，从线路机自己的公网 IP 出去。
//! 不需要 node_outbound 行，「直出」完全由入口的形状决定；(node_id, relay_id) 的唯一索引保证每台线路机至多一条在用的直出入口。

use crate::crypto::{short_id, SecretBox};
use crate::db::{bump_relay, new_id, Tx};
use crate::error::{ApiError, ApiResult};
use crate::names::entry_name;
use fleet_proto::{ApplyReport, DesiredEntry, DesiredState, Outbound, RelayParams};
use sqlx::PgPool;
use std::net::IpAddr;
use uuid::Uuid;

/// 策略锁的共享锁：发布在锁内读在用节点，建入口的事务拿着它就不会被漏掉。
/// 必须在事务开头、任何行锁之前拿（和发布的加锁顺序一致，否则会和发布互相等）。
pub async fn lock_policy_shared(tx: &mut Tx<'_>) -> ApiResult<()> {
    sqlx::query("select pg_advisory_xact_lock_shared($1)").bind(crate::policy::LOCK_KEY).execute(&mut **tx).await?;
    Ok(())
}

/// 新建入口（普通模式），见 [`create_with`]
pub async fn create(tx: &mut Tx<'_>, relay_id: Uuid, node_id: Uuid) -> ApiResult<(Uuid, i32, i64)> { create_with(tx, relay_id, node_id, crate::reverse::NORMAL).await }

/// 新建入口：锁线路机 → 从 `relay.port_next` 分端口（不低于现有入口的最大端口 + 1；分完 +1）→ 代数 +1。返回 (entry_id, port, generation)。
/// `mode` 是 normal / reverse（穿透，调用方已按 `reverse::resolve_new` 确认能用）。调用方须已在事务开头调过 [`lock_policy_shared`]。
pub async fn create_with(tx: &mut Tx<'_>, relay_id: Uuid, node_id: Uuid, mode: &str) -> ApiResult<(Uuid, i32, i64)> {
    let (pmin, pmax, next, top): (i32, i32, i32, Option<i32>) = sqlx::query_as(
        "select r.port_min, r.port_max, r.port_next, (select max(e.port) from entry e where e.relay_id = r.machine_id)
         from relay r join machine m on m.id = r.machine_id
         where r.machine_id = $1 and m.lifecycle = 'active' for update of r for share of m",
    )
    .bind(relay_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| ApiError::conflict("线路机不存在或不在用"))?;
    // 节点机器行也加共享锁：和退役（对机器行 for update）互斥，退役开始后就不会再有新入口挂上来
    let node: Option<(String, String)> = sqlx::query_as("select lifecycle::text, kind::text from machine where id = $1 for share")
        .bind(node_id)
        .fetch_optional(&mut **tx)
        .await?;
    match node {
        None => return Err(ApiError::not_found("没有这台节点")),
        Some((lc, _)) if lc == "retiring" || lc == "retired" => return Err(ApiError::conflict("节点正在退役或已退役，不能再加入口")),
        // 线路机只能当自己的「节点」（直出入口），不能挂到别的线路机上
        Some((_, kind)) if kind == "relay" && node_id != relay_id => return Err(ApiError::conflict("线路机不能当别的线路机的节点：线路机上只能加它自己的直出入口")),
        Some(_) => {}
    }
    // 端口永不复用（含已删除、已随机器物理删除的入口）：分过的端口记在 relay.port_next（迁移 0050），不看现有入口的 max(port)。
    // 旧分享链接不会误连到新节点，客户路由上残留的旧出口也不会被认成新入口（routers/clash.rs）。
    // 兜底不低于现有入口的最大端口 + 1：port_next 推算偏低（迁移按标签对回收站快照，标签改过的对不上），或中控回退到旧版本
    // （按 max(port) + 1 建入口、不推进 port_next）后再升回来时，不撞 entry_port_live 唯一索引
    let port = next.max(pmin).max(top.map_or(0, |p| p + 1));
    if port > pmax {
        return Err(ApiError::conflict("线路机端口段已用完：请在线路机参数里扩大端口范围"));
    }
    sqlx::query("update relay set port_next = $2 where machine_id = $1").bind(relay_id).bind(port + 1).execute(&mut **tx).await?;
    let gen = bump_relay(tx, relay_id).await?;
    let id = new_id();
    sqlx::query(
        "insert into entry (id, relay_id, node_id, port, client_uuid, short_id, lifecycle, changed_gen, mode)
         values ($1, $2, $3, $4, $5, $6, 'pending', $7, $8)",
    )
    .bind(id)
    .bind(relay_id)
    .bind(node_id)
    .bind(port)
    .bind(Uuid::new_v4())
    .bind(short_id())
    .bind(gen)
    .bind(mode)
    .execute(&mut **tx)
    .await?;
    Ok((id, port, gen))
}

/// 入口状态的中文（同网页 ENTRY_LABEL）
pub fn state_zh(s: &str) -> &str {
    match s {
        "pending" => "应用中",
        "active" => "运行中",
        "disabled" => "已停用",
        "removing" => "移除中",
        "removed" => "已移除",
        s => s,
    }
}

/// 改入口状态（enable/disable/removing/rotate）并 +1 代数。返回 (relay_id, generation)。
pub async fn transition(tx: &mut Tx<'_>, entry_id: Uuid, to: &str, rotate: bool) -> ApiResult<(Uuid, i64)> {
    // 先锁线路机再锁入口，与 on_applied 同序（入口所属的线路机不会变，先不加锁读出来）
    let relay_id: Uuid = sqlx::query_scalar("select relay_id from entry where id = $1")
        .bind(entry_id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| ApiError::not_found("没有这个入口"))?;
    sqlx::query("select 1 from relay where machine_id = $1 for update").bind(relay_id).execute(&mut **tx).await?;
    let cur: String = sqlx::query_scalar("select lifecycle::text from entry where id = $1 for update").bind(entry_id).fetch_one(&mut **tx).await?;
    let allowed = matches!(
        (cur.as_str(), to),
        ("active", "disabled") | ("disabled", "active") | ("pending" | "active" | "disabled", "removing") | ("removing", "removing")
    ) || (rotate && cur == to);
    if !allowed {
        return Err(ApiError::conflict(format!("入口当前是「{}」，不能变成「{}」", state_zh(&cur), state_zh(to))));
    }
    let gen = bump_relay(tx, relay_id).await?;
    sqlx::query(
        "update entry set lifecycle = $2::entry_lifecycle, changed_gen = $3, apply_error = null, updated_at = now(),
            client_uuid = case when $4 then gen_random_uuid() else client_uuid end
         where id = $1",
    )
    .bind(entry_id)
    .bind(to)
    .bind(gen)
    .bind(rotate)
    .execute(&mut **tx)
    .await?;
    Ok((relay_id, gen))
}

/// 线路机的期望状态。改了这里生成的内容（数据没变、结果会变的那种）要把 `db::DESIRED_FORMAT` +1，否则线路机拿不到新的
pub async fn desired_state(db: &PgPool, secrets: &SecretBox, relay_id: Uuid, echo_url: &str) -> anyhow::Result<DesiredState> {
    let (gen, dest, sni, sk_enc, mcv): (i64, String, String, Vec<u8>, Option<String>) = sqlx::query_as(
        "select generation, reality_dest, reality_sni, reality_private_enc, min_client_ver from relay where machine_id = $1",
    )
    .bind(relay_id)
    .fetch_one(db)
    .await?;

    #[derive(sqlx::FromRow)]
    struct Row {
        id: Uuid,
        node_id: Uuid,
        team_id: Option<Uuid>,
        port: i32,
        lifecycle: String,
        client_uuid: Uuid,
        short_id: String,
        kind: String,
        team: Option<String>,
        number: Option<i32>,
        label: Option<String>,
        ip: String,
        direct: bool,
        protocol: Option<String>,
        host: Option<String>,
        oport: Option<i32>,
        method: Option<String>,
        username: Option<String>,
        secret_enc: Option<Vec<u8>>,
        mode: String,
        bridge_uuid: Uuid,
    }
    // 直出入口没有 node_outbound 行（节点就是线路机自己）；节点入口缺出站参数（或秘密已在退役时清掉）的照旧不下发
    let rows: Vec<Row> = sqlx::query_as(
        "select e.id, e.node_id, m.team_id, e.port, e.lifecycle::text lifecycle, e.client_uuid, e.short_id, m.kind::text kind, t.name team, m.number,
                m.label, host(m.primary_ip) ip, e.node_id = e.relay_id direct, o.protocol, o.host, o.port oport, o.method, o.username, o.secret_enc,
                e.mode, e.bridge_uuid
         from entry e join machine m on m.id = e.node_id left join team t on t.id = m.team_id
         left join node_outbound o on o.machine_id = e.node_id
         where e.relay_id = $1 and e.lifecycle in ('pending', 'active', 'disabled') and (e.node_id = e.relay_id or o.secret_enc is not null)
         order by e.port",
    )
    .bind(relay_id)
    .fetch_all(db)
    .await?;
    let policies = crate::policy::load_current(db).await?;
    let mut entries = Vec::with_capacity(rows.len());
    for r in rows {
        // 穿透：线路机不连节点，出站参数（节点的 ssserver）留在库里不下发，切回普通模式时照旧用
        let outbound = match (r.direct, r.protocol.as_deref(), r.host, r.oport, r.secret_enc) {
            (true, ..) => Outbound::Direct,
            (false, ..) if r.mode == crate::reverse::REVERSE => Outbound::Reverse { bridge_uuid: r.bridge_uuid },
            (false, Some(p), Some(host), Some(oport), Some(enc)) => {
                let (secret, port) = (secrets.open_str(&enc)?, oport as u16);
                match p {
                    "shadowsocks" => Outbound::Shadowsocks { host, port, method: r.method.unwrap_or_else(|| crate::meta::SS_METHOD.into()), password: secret },
                    "socks5" => Outbound::Socks5 { host, port, username: r.username, password: Some(secret).filter(|s| !s.is_empty()) },
                    _ => Outbound::Http { host, port, username: r.username, password: Some(secret).filter(|s| !s.is_empty()) },
                }
            }
            _ => anyhow::bail!("入口 {} 的节点出站参数不全", r.id),
        };
        entries.push(DesiredEntry {
            id: r.id,
            port: r.port as u16,
            enabled: r.lifecycle != "disabled",
            remark: entry_name(r.direct, &r.kind, r.label.as_deref(), r.team.as_deref(), r.number),
            client_uuid: r.client_uuid,
            short_id: r.short_id,
            outbound,
            expect_egress_ip: r.ip.parse::<IpAddr>()?,
            policy: crate::policy::compile(&policies.effective(r.node_id, r.team_id).0),
        });
    }
    Ok(DesiredState {
        generation: gen as u64,
        relay: RelayParams {
            reality_dest: dest,
            reality_sni: sni,
            reality_private_key: secrets.open_str(&sk_enc)?,
            min_client_ver: mcv,
            echo_url: echo_url.to_string(),
            probe_interval_s: 30,
        },
        entries,
        // 三网 PING 的目标：全局 + 这台的覆盖（ping.rs）
        ping: crate::ping::config_for(db, relay_id).await?,
    })
}

/// 线路机回报：记录已应用代数，推进入口状态（pending→active，removing→removed）。
/// 返回是否有改动：同一份回报重发（已应用的代数、同样的错误）什么都不写，调用方也就不推送
pub async fn on_applied(db: &PgPool, relay_id: Uuid, rep: &ApplyReport) -> anyhow::Result<bool> {
    let mut tx = db.begin().await?;
    let gen = rep.generation as i64;
    // 回报的代数比中控记录的还新：数据库回滚过或 agent 带着旧机器的状态，不能信，忽略
    let cur: Option<(i64,)> = sqlx::query_as("select generation from relay where machine_id = $1 for update").bind(relay_id).fetch_optional(&mut *tx).await?;
    if let Some((g,)) = cur {
        if gen > g {
            tracing::warn!(%relay_id, reported = gen, current = g, "忽略超前的应用回报");
            return Ok(false);
        }
    }
    let mut n = sqlx::query(
        "update entry set apply_error = u.err from unnest($2::uuid[], $3::text[]) u(id, err)
         where entry.id = u.id and entry.relay_id = $1 and entry.apply_error is distinct from u.err",
    )
    .bind(relay_id)
    .bind(rep.entries.iter().map(|e| e.id).collect::<Vec<_>>())
    .bind(rep.entries.iter().map(|e| if e.ok { None } else { e.error.clone().or(Some("失败".into())) }).collect::<Vec<Option<String>>>())
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if rep.ok {
        // 上次应用时间照旧每份都刷新；只有代数前进或清掉了错误才算改动（行已在上面锁住，o 读到的是改之前的值）
        let moved: Option<bool> = sqlx::query_scalar(
            "with o as (select applied_generation g, apply_error e from relay where machine_id = $1)
             update relay set applied_generation = greatest(applied_generation, $2), applied_at = now(), apply_error = null
             from o where machine_id = $1 returning o.g < $2 or o.e is not null",
        )
        .bind(relay_id)
        .bind(gen)
        .fetch_optional(&mut *tx)
        .await?;
        n += moved.unwrap_or(false) as u64;
        let failed: Vec<Uuid> = rep.entries.iter().filter(|e| !e.ok).map(|e| e.id).collect();
        n += sqlx::query(
            "update entry set lifecycle = 'active', updated_at = now()
             where relay_id = $1 and lifecycle = 'pending' and changed_gen <= $2 and not (id = any($3))",
        )
        .bind(relay_id)
        .bind(gen)
        .bind(&failed)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        n += sqlx::query(
            "update entry set lifecycle = 'removed', removed_at = now(), updated_at = now()
             where relay_id = $1 and lifecycle = 'removing' and changed_gen <= $2",
        )
        .bind(relay_id)
        .bind(gen)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    } else {
        n += sqlx::query("update relay set apply_error = $2 where machine_id = $1 and apply_error is distinct from $2")
            .bind(relay_id)
            .bind(rep.error.clone().unwrap_or_else(|| "未知错误".into()))
            .execute(&mut *tx)
            .await?
            .rows_affected();
    }
    tx.commit().await?;
    Ok(n > 0)
}

/// 百分号编码（只留 RFC 3986 的非保留字符）
pub(crate) fn pct(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => o.push(b as char),
            _ => o.push_str(&format!("%{b:02X}")),
        }
    }
    o
}

/// vless:// 分享链接；fragment 只放显示名，不放任何 IP。
pub fn share_url(uuid: Uuid, host: &str, port: i32, sni: &str, pbk: &str, sid: &str, remark: &str) -> String {
    format!(
        "vless://{uuid}@{host}:{port}?encryption=none&flow=xtls-rprx-vision&security=reality&sni={}&fp=chrome&pbk={}&sid={}&type=tcp#{}",
        pct(sni),
        pct(pbk),
        pct(sid),
        pct(remark)
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn share_url_encodes_remark() {
        let u = super::share_url(uuid::Uuid::nil(), "relay.example", 20001, "www.cloudflare.com", "pk", "ab", "团队1-06");
        assert!(u.starts_with("vless://00000000-0000-0000-0000-000000000000@relay.example:20001?"));
        assert!(u.ends_with("#%E5%9B%A2%E9%98%9F1-06"));
    }
}
