//! 穿透入口（DESIGN.md「穿透入口」）：入口的模式、新入口的默认模式、能不能开、节点上 bridge 的配置、隧道挂不上时叫节点重启 bridge。
//!
//! 普通模式下每条新客户连接都要线路机向节点新建一次 TCP（连 ssserver）；有的供应商（2026-10 的 DeerVPS / AT&T 段）丢外面发起的
//! 入站 SYN（10–30%），客户隔一会转圈 1–3 秒。穿透 = 节点上的 bridge（Xray 26 的 VLESS 反向代理）用这个入口专用的客户端 UUID
//! 主动连入口自己的 REALITY 入站、建长连接，线路机把这个入口的客户流量经它交给节点，从节点自己的 IP 出去：线路机不再向节点发起连接，
//! 分享链接、订阅、软路由出口都不变（同一个入口端口与凭据）。节点上的 ssserver 照旧留着，切回普通模式即用。
//!
//! 第一版只支持有自己 SSH 与 agent 的 VPS 节点：家宽代理装不了 bridge；同机 IP 节点的 bridge 要从它自己的 IP 出站，没做。
//! 新入口（上架、加入口）不支持时自动按普通模式建并写明原因；换线路机的新入口沿用旧入口的模式（同样不支持就退回普通）。

use crate::db::{audit, Tx};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use fleet_proto::CAP_REVERSE;
use serde_json::{json, Value};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

pub const NORMAL: &str = "normal";
pub const REVERSE: &str = "reverse";

/// 模式的中文（网页、fleetctl、作业日志）
pub fn mode_zh(m: &str) -> &'static str { if m == REVERSE { "穿透" } else { "普通" } }

/// 「新入口默认模式」存在 app_setting 的这一项（值是 "normal" / "reverse"）。没设过 = 穿透：
/// 新上架的多是 AT&T 系、入站质量差的机器（用户 2026-10-27 定）
const SETTING_KEY: &str = "entry_mode_default";
pub const DEFAULT_MODE: &str = REVERSE;

/// 校验模式字符串（API、上架参数）
pub fn parse_mode(s: &str) -> Result<&'static str, String> {
    match s.trim() {
        NORMAL => Ok(NORMAL),
        REVERSE => Ok(REVERSE),
        x => Err(format!("入口模式只能是 normal（普通）或 reverse（穿透），给的是「{x}」")),
    }
}

/// 新入口的默认模式与是否设过（没设过时按 `DEFAULT_MODE`）
pub async fn default_mode(db: &PgPool) -> sqlx::Result<(&'static str, bool)> {
    let v: Option<Value> = sqlx::query_scalar("select value from app_setting where key = $1").bind(SETTING_KEY).fetch_optional(db).await?;
    Ok(match v.as_ref().and_then(Value::as_str).map(parse_mode) {
        Some(Ok(m)) => (m, true),
        // 手工改坏了：按没设过处理，日志里提醒
        Some(Err(e)) => {
            tracing::warn!("app_setting 里的 {SETTING_KEY} 不对（{e}），按默认的「{}」", mode_zh(DEFAULT_MODE));
            (DEFAULT_MODE, false)
        }
        None => (DEFAULT_MODE, false),
    })
}

/// 改默认模式：写库、写审计。返回改之前的
pub async fn save_default(st: &AppState, actor: &str, mode: &'static str) -> ApiResult<&'static str> {
    let (old, set) = default_mode(&st.db).await?;
    if old == mode && set {
        return Ok(old);
    }
    let mut tx = st.db.begin().await?;
    sqlx::query(
        "insert into app_setting (key, value, updated_by) values ($1, $2, $3)
         on conflict (key) do update set value = excluded.value, updated_at = now(), updated_by = excluded.updated_by",
    )
    .bind(SETTING_KEY)
    .bind(json!(mode))
    .bind(actor)
    .execute(&mut *tx)
    .await?;
    audit(&mut tx, actor, "settings.entry_mode", "setting", None, json!({"from": old, "to": mode})).await?;
    tx.commit().await?;
    Ok(old)
}

/// agent 报过的能力里有没有穿透（`Report.caps`，中控存在 agent_state.info.caps；旧 agent 是空数组或没有）
async fn agent_can(conn: &mut PgConnection, machine: Uuid) -> sqlx::Result<bool> {
    let v: Option<bool> = sqlx::query_scalar("select coalesce(info->'caps' ? $2, false) from agent_state where machine_id = $1")
        .bind(machine)
        .bind(CAP_REVERSE)
        .fetch_optional(&mut *conn)
        .await?;
    Ok(v.unwrap_or(false))
}

/// 节点挂在这台线路机上的入口能不能用穿透；不能时返回原因（中文，带下一步怎么办）。
/// - 家宽代理没有 SSH 和 agent，装不了 bridge；直出入口没有节点；
/// - 同机 IP 节点：bridge 要从它自己的 IP 出站（Xray 的 sendThrough），第一版没做；
/// - 线路机 agent 太旧：不认识期望状态里的穿透出站，整份解析失败（整台线路机的配置停在旧代数）；
/// - 节点 agent 太旧：不会按中控的要求重启 bridge（隧道挂不上时没法自动恢复，见 `kick_for`）
pub async fn unsupported(conn: &mut PgConnection, node: Uuid, relay: Uuid) -> sqlx::Result<Option<String>> {
    let n: Option<(String, Option<Uuid>, String)> = sqlx::query_as(
        "select m.kind::text, m.host_id, display_name(m.kind::text, m.label, t.name, m.number) from machine m left join team t on t.id = m.team_id where m.id = $1",
    )
    .bind(node)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((kind, host, name)) = n else { return Ok(Some("节点不存在".into())) };
    match kind.as_str() {
        "proxy" => return Ok(Some("家宽代理没有 SSH 和 agent，装不了穿透要的 bridge".into())),
        "relay" => return Ok(Some("线路机直出入口没有落地节点，用不着穿透".into())),
        _ => {}
    }
    if host.is_some() {
        return Ok(Some("同机 IP 节点暂不支持穿透（bridge 要从它自己的 IP 出站，第一版没做）".into()));
    }
    let relay_name: String = sqlx::query_scalar("select coalesce(label, '') from machine where id = $1").bind(relay).fetch_one(&mut *conn).await?;
    if !agent_can(conn, relay).await? {
        return Ok(Some(format!("线路机 {relay_name} 的 agent 版本过旧，不认识穿透入口（下发了会整份配置解析失败）：先 fleetctl upgrade-agent {relay_name}")));
    }
    if !agent_can(conn, node).await? {
        return Ok(Some(format!("节点 {name} 的 agent 版本过旧（或还没上报过），隧道挂不上时不会自动重启 bridge：先 fleetctl upgrade-agent {name}")));
    }
    Ok(None)
}

/// 新入口用哪种模式：`requested`（上架参数、加入口参数；None = 面板设置的默认模式）要穿透但用不了时退回普通，返回原因
pub async fn resolve_new(conn: &mut PgConnection, node: Uuid, relay: Uuid, requested: &str) -> sqlx::Result<(&'static str, Option<String>)> {
    if requested != REVERSE {
        return Ok((NORMAL, None));
    }
    Ok(match unsupported(conn, node, relay).await? {
        None => (REVERSE, None),
        Some(why) => (NORMAL, Some(format!("按普通模式建入口：{why}"))),
    })
}

/// 只看机器类型就知道不支持的（上架预览、作业建立时还没有 agent 上报可看）：家宽代理、同机 IP 节点
pub fn kind_unsupported(kind: &str, host_id: Option<Uuid>) -> Option<&'static str> {
    match (kind, host_id) {
        ("proxy", _) => Some("家宽代理没有 SSH 和 agent，装不了穿透要的 bridge"),
        (_, Some(_)) => Some("同机 IP 节点暂不支持穿透（bridge 要从它自己的 IP 出站，第一版没做）"),
        _ => None,
    }
}

// ── 节点上的 bridge ─────────────────────────────────────────────────────────

/// bridge 的一条隧道：连哪个入口（线路机 IP + 入口端口 + 该入口的 REALITY 参数），用哪个客户端
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct Tunnel {
    pub entry_id: Uuid,
    pub relay_ip: String,
    pub port: i32,
    pub bridge_uuid: Uuid,
    pub sni: String,
    pub public_key: String,
    pub short_id: String,
}

/// bridge 收到的反向流量在它自己路由里的入站标签（所有隧道共用：都交给 direct 出去）
pub const BRIDGE_INBOUND: &str = "r-in";

/// 同 ssserver 的出站黑名单（jobs/scripts.rs `OUTBOUND_ACL`）：客户把域名解析到回环、私网、链路本地、CGNAT 也到不了节点本机服务和供应商内网
const PRIVATE: &[&str] = &[
    "0.0.0.0/8", "127.0.0.0/8", "10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "169.254.0.0/16", "100.64.0.0/10", "::/128", "::1/128", "fc00::/7", "fe80::/10",
];

/// 节点上 bridge 的整份 Xray 配置（中控渲染，作业经 SSH 写到 /etc/fleet-bridge/xray.json）。
/// - 每条隧道一个 VLESS 出站，`reverse.tag` 让它成为反向代理的 bridge 端：Xray 自己维护到线路机的连接（断了约 2 秒重连，
///   每条连接平均超过 16 路并发时再开一条），线路机经它送来的连接按入站标签 `r-in` 交给 direct 出去；
/// - REALITY 参数、端口与客户的分享链接相同（连的就是这个入口自己的入站），客户端 UUID 是入口的 bridge_uuid，不带 flow（同线路机侧）；
///   线路机地址用它的 IP（不经域名解析）；
/// - TCP 保活 10 + 5 秒、TCP_USER_TIMEOUT 15 秒：链路断了（线路机侧没有 FIN / RST）时 bridge 自己 20 秒左右发现、重连
///   （实验：断网 20 秒恢复后 2 秒通；不设时同样断 60 秒，恢复后要 47 秒）；
/// - 路由 IPOnDemand：先解析目标域名，落在私网段（`PRIVATE`，中控地址所在的那段除外，同 ssserver 的黑名单）的拦掉；
/// - 第一个出站 blocked 是默认出站：不匹配规则的（不会有）一律丢弃，隧道出站本身不接任何路由（Xray 也不许它当正向代理）
pub fn render(tunnels: &[Tunnel], public_url: &str) -> Value {
    let central = reqwest::Url::parse(public_url).ok().and_then(|u| u.host_str().and_then(|h| h.trim_start_matches('[').trim_end_matches(']').parse::<std::net::IpAddr>().ok()));
    let private: Vec<&str> = PRIVATE.iter().copied().filter(|n| !central.is_some_and(|c| n.parse::<ipnetwork::IpNetwork>().is_ok_and(|n| n.contains(c)))).collect();
    let mut outbounds = vec![
        json!({"tag": "blocked", "protocol": "blackhole"}),
        json!({"tag": "direct", "protocol": "freedom", "settings": {"domainStrategy": "AsIs"}}),
    ];
    let mut ts: Vec<&Tunnel> = tunnels.iter().collect();
    ts.sort_by_key(|t| t.entry_id);
    for t in ts {
        outbounds.push(json!({
            "tag": format!("t-{}", t.entry_id.simple()),
            "protocol": "vless",
            "settings": {
                "address": t.relay_ip, "port": t.port, "id": t.bridge_uuid.to_string(), "encryption": "none",
                "reverse": {"tag": BRIDGE_INBOUND}
            },
            "streamSettings": {
                "network": "tcp", "security": "reality",
                "realitySettings": {"serverName": t.sni, "password": t.public_key, "shortId": t.short_id, "fingerprint": "chrome"},
                "sockopt": {"tcpKeepAliveIdle": 10, "tcpKeepAliveInterval": 5, "tcpUserTimeout": 15000}
            }
        }));
    }
    json!({
        "log": {"access": "none", "loglevel": "warning"},
        "outbounds": outbounds,
        "routing": {"domainStrategy": "IPOnDemand", "rules": [
            {"type": "field", "ruleTag": "block-private", "inboundTag": [BRIDGE_INBOUND], "ip": private, "outboundTag": "blocked"},
            {"type": "field", "ruleTag": "bridge", "inboundTag": [BRIDGE_INBOUND], "outboundTag": "direct"}
        ]}
    })
}

/// 两份 bridge 配置只差在少了几条隧道（其余完全一样）：只删隧道时写配置、不重启 bridge（重启会断开这台节点上全部经隧道的连接）。
/// 运行中的 bridge 对删掉的入口还会每 2 秒试连一次（对面端口已关，不出日志），下次重启时按新配置就没了
pub fn only_removed(old: &Value, new: &Value) -> bool {
    let (Some(oo), Some(no)) = (old["outbounds"].as_array(), new["outbounds"].as_array()) else { return false };
    if no.len() >= oo.len() || !no.iter().all(|o| oo.contains(o)) {
        return false;
    }
    let strip = |v: &Value| {
        let mut v = v.clone();
        v["outbounds"] = Value::Null;
        v
    };
    strip(old) == strip(new)
}

/// 节点 bridge 该有的隧道：它在用、待应用、停用中的穿透入口（停用的入站已摘，隧道连不上也无妨，启用时不用再推），
/// 外加 `include`（开启穿透作业：先推 bridge、再切期望状态），去掉 `exclude`
pub async fn tunnels(db: &PgPool, node: Uuid, include: Option<Uuid>, exclude: Option<Uuid>) -> sqlx::Result<Vec<Tunnel>> {
    sqlx::query_as(
        "select e.id entry_id, host(rm.primary_ip) relay_ip, e.port, e.bridge_uuid, r.reality_sni sni, r.reality_public_key public_key, e.short_id
         from entry e join relay r on r.machine_id = e.relay_id join machine rm on rm.id = e.relay_id
         where e.node_id = $1 and e.node_id <> e.relay_id and e.lifecycle in ('pending', 'active', 'disabled')
           and (e.mode = 'reverse' or e.id = $2) and e.id is distinct from $3
         order by e.id",
    )
    .bind(node)
    .bind(include)
    .bind(exclude)
    .fetch_all(db)
    .await
}

// ── 隧道没挂上：叫节点重启 bridge ─────────────────────────────────────────────

/// 同一入口两次叫重启至少隔这么久：bridge 本身起不来、线路机那边一直不让连时，不每 10 秒重启一次
pub const KICK_GAP_S: i64 = 180;
/// 叫了之后这么久内节点的回执都带着它（节点 agent 离线一会儿回来也能收到）
pub const KICK_TTL_S: i64 = 600;

/// 线路机报这些入口的隧道没挂上（`Report.tunnels_down`）：属于它、在用的穿透入口记下要重启的时刻（3 分钟内不重复）。返回记下了几个
pub async fn on_tunnels_down(db: &PgPool, relay: Uuid, ids: &[Uuid]) -> sqlx::Result<u64> {
    if ids.is_empty() {
        return Ok(0);
    }
    let r = sqlx::query(
        "update entry set reverse_kick_at = now()
         where id = any($2) and relay_id = $1 and mode = 'reverse' and lifecycle = 'active'
           and (reverse_kick_at is null or reverse_kick_at < now() - make_interval(secs => $3))",
    )
    .bind(relay)
    .bind(ids)
    .bind(KICK_GAP_S as f64)
    .execute(db)
    .await?;
    if r.rows_affected() > 0 {
        tracing::warn!(%relay, n = r.rows_affected(), "穿透隧道没挂上，叫节点重启 bridge");
    }
    Ok(r.rows_affected())
}

/// 节点回执里的 `bridge_kick`：它的穿透入口里最近一次叫重启的时刻（Unix 毫秒），10 分钟以前的不算
pub async fn kick_for(db: &PgPool, node: Uuid) -> sqlx::Result<Option<i64>> {
    sqlx::query_scalar(
        "select (extract(epoch from max(reverse_kick_at)) * 1000)::bigint from entry
         where node_id = $1 and mode = 'reverse' and reverse_kick_at > now() - make_interval(secs => $2)",
    )
    .bind(node)
    .bind(KICK_TTL_S as f64)
    .fetch_one(db)
    .await
}

// ── 切换 ───────────────────────────────────────────────────────────────────

/// 改入口模式并给线路机代数 +1（入口 changed_gen 跟着记，等线路机应用到它）。返回 (线路机, 代数)；已经是这个模式也照样 +1（重试时让线路机重新应用一次）
pub async fn set_mode(tx: &mut Tx<'_>, entry: Uuid, mode: &str) -> ApiResult<(Uuid, i64)> {
    let relay: Uuid = sqlx::query_scalar("select relay_id from entry where id = $1").bind(entry).fetch_optional(&mut **tx).await?.ok_or_else(|| ApiError::not_found("没有这个入口"))?;
    // 先锁线路机再锁入口（同 entries::transition、on_applied）
    sqlx::query("select 1 from relay where machine_id = $1 for update").bind(relay).execute(&mut **tx).await?;
    let lc: String = sqlx::query_scalar("select lifecycle::text from entry where id = $1 for update").bind(entry).fetch_one(&mut **tx).await?;
    if !matches!(lc.as_str(), "pending" | "active" | "disabled") {
        return Err(ApiError::conflict(format!("入口现在是「{}」，不能切换模式", crate::entries::state_zh(&lc))));
    }
    let gen = crate::db::bump_relay(tx, relay).await?;
    sqlx::query("update entry set mode = $2, changed_gen = $3, apply_error = null, reverse_kick_at = null, updated_at = now() where id = $1")
        .bind(entry)
        .bind(mode)
        .bind(gen)
        .execute(&mut **tx)
        .await?;
    Ok((relay, gen))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(n: u128, port: i32) -> Tunnel {
        Tunnel {
            entry_id: Uuid::from_u128(n),
            relay_ip: "198.51.100.2".into(),
            port,
            bridge_uuid: Uuid::from_u128(n + 100),
            sni: "www.cloudflare.com".into(),
            public_key: "PBK".into(),
            short_id: "0123abcd".into(),
        }
    }

    #[test]
    fn bridge_config_shape() {
        let v = render(&[t(2, 20002), t(1, 20001)], "https://fleet.example.com");
        let id = Uuid::from_u128(1).simple().to_string();
        assert_eq!(v["outbounds"][0], json!({"tag": "blocked", "protocol": "blackhole"}), "第一个是默认出站：丢弃");
        // 按入口排序，每条隧道一个 VLESS 出站：连入口端口、带 reverse、不带 flow
        assert_eq!(
            v["outbounds"][2],
            json!({
                "tag": format!("t-{id}"), "protocol": "vless",
                "settings": {"address": "198.51.100.2", "port": 20001, "id": Uuid::from_u128(101).to_string(), "encryption": "none", "reverse": {"tag": "r-in"}},
                "streamSettings": {
                    "network": "tcp", "security": "reality",
                    "realitySettings": {"serverName": "www.cloudflare.com", "password": "PBK", "shortId": "0123abcd", "fingerprint": "chrome"},
                    "sockopt": {"tcpKeepAliveIdle": 10, "tcpKeepAliveInterval": 5, "tcpUserTimeout": 15000}
                }
            })
        );
        assert_eq!(v["outbounds"].as_array().unwrap().len(), 4);
        assert_eq!(v["routing"]["domainStrategy"], "IPOnDemand");
        let rules = v["routing"]["rules"].as_array().unwrap();
        assert_eq!(rules[0]["outboundTag"], "blocked");
        assert!(rules[0]["ip"].as_array().unwrap().iter().any(|x| x == "127.0.0.0/8") && rules[0]["ip"].as_array().unwrap().iter().any(|x| x == "192.168.0.0/16"));
        assert_eq!(rules[1], json!({"type": "field", "ruleTag": "bridge", "inboundTag": ["r-in"], "outboundTag": "direct"}));
        // 中控是内网 IP（实验室）：出口探测要经节点连它的 /echo，那一段不拦
        let lab = render(&[t(1, 20001)], "http://192.168.31.60:8080");
        assert!(!lab["routing"]["rules"][0]["ip"].as_array().unwrap().iter().any(|x| x == "192.168.0.0/16"));
        assert!(lab["routing"]["rules"][0]["ip"].as_array().unwrap().iter().any(|x| x == "10.0.0.0/8"));
    }

    #[test]
    fn only_removed_detects_pure_removals() {
        let (a, b, c) = (t(1, 20001), t(2, 20002), t(3, 20003));
        let u = "https://fleet.example.com";
        let two = render(&[a.clone(), b.clone()], u);
        assert!(only_removed(&two, &render(std::slice::from_ref(&a), u)));
        assert!(only_removed(&two, &render(&[], u)));
        assert!(!only_removed(&two, &two), "没变不算");
        assert!(!only_removed(&two, &render(&[a.clone(), c.clone()], u)), "换了一条：要重启");
        assert!(!only_removed(&render(std::slice::from_ref(&a), u), &two), "加了一条：要重启");
        let mut a2 = a.clone();
        a2.sni = "other".into();
        assert!(!only_removed(&two, &render(&[a2], u)), "留下的那条参数变了：要重启");
        assert!(!only_removed(&json!({}), &two));
    }

    #[test]
    fn modes() {
        assert_eq!((parse_mode(" reverse "), parse_mode("normal")), (Ok(REVERSE), Ok(NORMAL)));
        assert!(parse_mode("穿透").unwrap_err().contains("normal"));
        assert_eq!((mode_zh(REVERSE), mode_zh(NORMAL)), ("穿透", "普通"));
        assert_eq!(kind_unsupported("proxy", None).map(|s| s.contains("家宽代理")), Some(true));
        assert_eq!(kind_unsupported("vps", Some(Uuid::nil())).map(|s| s.contains("同机 IP")), Some(true));
        assert!(kind_unsupported("vps", None).is_none());
    }
}
