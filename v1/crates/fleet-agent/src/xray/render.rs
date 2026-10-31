//! DesiredState → 完整 xray.json（固定骨架 + 每个入口一组入站/出站/规则）。
//! 纯函数，无副作用；apply 与 `render` 子命令共用。

use crate::config::Config;
use anyhow::{bail, ensure, Result};
use fleet_proto::{tags, DesiredEntry, DesiredState, EntryPolicy, Outbound, PolicyMode, RelayParams};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

pub const API_TAG: &str = "api";
/// Xray API 监听地址：只在本机回环，agent 经 `xray api --server=` 连它。没有需要改它的场景，不做成配置项
pub const API_HOST: &str = "127.0.0.1";
pub const API_PORT: u16 = fleet_proto::XRAY_API_PORT;
pub const STATIC_OUTBOUNDS: [&str; 3] = ["deny", "direct", "blocked"];
/// mihomo 兼容：minClientVer 不得高于此值
pub const MAX_MIN_CLIENT_VER: &str = "1.8.2";

pub struct RenderCtx {
    pub access_log: String,
}

impl RenderCtx {
    pub fn of(cfg: &Config) -> Self {
        Self { access_log: cfg.access_log.to_string_lossy().into() }
    }
}

/// 渲染结果：整份配置 + 按类别切开的对象（用于按 tag 做差异）
#[derive(Debug, Clone, PartialEq)]
pub struct Rendered {
    pub full: Value,
    pub inbounds: Vec<Value>,
    pub outbounds: Vec<Value>,
    pub rules: Vec<Value>,
    /// 不随入口变化的部分（log/api/stats/policy + 静态出站/规则），变了就必须重启 Xray
    pub static_part: Value,
}

pub fn tag_of(v: &Value) -> &str {
    v.get("tag").and_then(|t| t.as_str()).unwrap_or("")
}
pub fn rule_tag_of(v: &Value) -> &str {
    v.get("ruleTag").and_then(|t| t.as_str()).unwrap_or("")
}
pub fn is_static_outbound(tag: &str) -> bool {
    STATIC_OUTBOUNDS.contains(&tag)
}
fn is_entry_rule(v: &Value) -> bool {
    let t = rule_tag_of(v);
    t.starts_with("rule-") || t.starts_with("probe-") || t.starts_with("policy-") || t.starts_with("guard-")
}

impl Rendered {
    /// 从整份配置切分（render 与读取磁盘上的现有配置共用）
    pub fn from_full(full: Value) -> Rendered {
        let arr = |p: &[&str]| -> Vec<Value> {
            let mut cur = &full;
            for k in p {
                match cur.get(*k) {
                    Some(n) => cur = n,
                    None => return vec![],
                }
            }
            cur.as_array().cloned().unwrap_or_default()
        };
        let inbounds = arr(&["inbounds"]);
        let outbounds = arr(&["outbounds"]);
        let rules = arr(&["routing", "rules"]);
        let mut sp = Map::new();
        for k in ["log", "api", "stats", "policy"] {
            if let Some(v) = full.get(k) {
                sp.insert(k.into(), v.clone());
            }
        }
        sp.insert("outbounds".into(), Value::Array(outbounds.iter().filter(|v| is_static_outbound(tag_of(v))).cloned().collect()));
        sp.insert("rules".into(), Value::Array(rules.iter().filter(|v| !is_entry_rule(v)).cloned().collect()));
        sp.insert("domainStrategy".into(), full.pointer("/routing/domainStrategy").cloned().unwrap_or(Value::Null));
        Rendered { full, inbounds, outbounds, rules, static_part: Value::Object(sp) }
    }
}

fn parse_ver(s: &str) -> Option<(u32, u32, u32)> {
    let mut it = s.trim().split('.');
    let a = it.next()?.parse().ok()?;
    let b = it.next().map(|x| x.parse().ok()).unwrap_or(Some(0))?;
    let c = it.next().map(|x| x.parse().ok()).unwrap_or(Some(0))?;
    if it.next().is_some() {
        return None;
    }
    Some((a, b, c))
}

/// 每个启用入口的回环探测入站端口 = 入口端口 + `fleet_proto::PROBE_PORT_OFFSET`。超出 65535 或与其它端口冲突则没有探测入站。
pub fn probe_ports(entries: &[DesiredEntry]) -> HashMap<Uuid, u16> {
    let taken: HashSet<u16> = entries.iter().filter(|e| e.enabled).map(|e| e.port).chain([API_PORT]).collect();
    entries
        .iter()
        .filter(|e| e.enabled)
        .filter_map(|e| {
            let p = e.port.checked_add(fleet_proto::PROBE_PORT_OFFSET)?;
            (!taken.contains(&p)).then_some((e.id, p))
        })
        .collect()
}

/// 有 AnyTLS 的启用入口：入口 → (公网端口, 回环桥接端口)。中控下发了 `anytls_port`、且和 `fleet_proto::anytls_ports` 按这份期望状态算出的一致才算
/// （同一条规则，算不出来或对不上就不开，不会监听一个分享链接里没有的端口）
pub fn anytls_ports(entries: &[DesiredEntry]) -> HashMap<Uuid, (u16, u16)> {
    let enabled: Vec<(Uuid, u16)> = entries.iter().filter(|e| e.enabled).map(|e| (e.id, e.port)).collect();
    let calc = fleet_proto::anytls_ports(&enabled);
    entries
        .iter()
        .filter(|e| e.enabled)
        .filter_map(|e| {
            let p = e.anytls_port.filter(|p| calc.get(&e.id) == Some(p))?;
            Some((e.id, (p, e.port.checked_add(fleet_proto::ANYTLS_BRIDGE_OFFSET)?)))
        })
        .collect()
}

pub fn probe_tag(id: Uuid) -> String {
    format!("probe-{}", id.simple())
}

fn outbound_json(id: Uuid, o: &Outbound) -> Value {
    let tag = tags::outbound(id);
    let server = |host: &str, port: u16, user: &Option<String>, pass: &Option<String>| {
        let mut s = json!({"address": host, "port": port});
        if let Some(u) = user.as_deref().filter(|u| !u.is_empty()) {
            s["users"] = json!([{"user": u, "pass": pass.clone().unwrap_or_default()}]);
        }
        s
    };
    match o {
        Outbound::Shadowsocks { host, port, method, password } => json!({
            "tag": tag, "protocol": "shadowsocks",
            "settings": {"servers": [{"address": host, "port": port, "method": method, "password": password}]}
        }),
        Outbound::Socks5 { host, port, username, password } => json!({
            "tag": tag, "protocol": "socks",
            "settings": {"servers": [server(host, *port, username, password)]}
        }),
        Outbound::Http { host, port, username, password } => json!({
            "tag": tag, "protocol": "http",
            "settings": {"servers": [server(host, *port, username, password)]}
        }),
        // 直出也用入口自己的出站 tag（不用共享的 direct）：探测入站、访问策略的放行规则都指向 out-<入口>，
        // 增删、回滚、核对和节点入口走同一条路
        Outbound::Direct => json!({"tag": tag, "protocol": "freedom", "settings": {"domainStrategy": "AsIs"}}),
    }
}

/// VLESS REALITY 入站，与 mihomo / OpenClash 兼容的固定形状，不要改（客户是用 OpenClash 软路由的团队）：
/// - 只有 VLESS + REALITY，传输 tcp、`header.type` 为 none，flow 用 `xtls-rprx-vision`，sniffing 的 `destOverride` 含 `quic`；
/// - `minClientVer` 不得高于 `MAX_MIN_CLIENT_VER`（1.8.2）：更高的值会让 mihomo 被 Xray 26.7+ 拒绝连接；
/// - 分享链接里 `fp=chrome`、`flow=xtls-rprx-vision`、`type=tcp`
fn inbound_json(e: &DesiredEntry, rp: &RelayParams, min_ver: &str) -> Value {
    json!({
        "listen": "0.0.0.0",
        "port": e.port,
        "protocol": "vless",
        "tag": tags::inbound(e.id),
        "settings": {
            "clients": [{"id": e.client_uuid.to_string(), "email": tags::email(e.id), "flow": "xtls-rprx-vision"}],
            "decryption": "none",
            "fallbacks": []
        },
        "streamSettings": {
            "network": "tcp",
            "security": "reality",
            "tcpSettings": {"acceptProxyProtocol": false, "header": {"type": "none"}},
            "realitySettings": {
                "show": false,
                "xver": 0,
                "target": dest_with_port(&rp.reality_dest),
                "serverNames": [rp.reality_sni],
                "privateKey": rp.reality_private_key,
                "minClientVer": min_ver,
                "maxClientVer": "",
                "maxTimeDiff": 0,
                "shortIds": [e.short_id]
            }
        },
        "sniffing": {"enabled": true, "destOverride": ["http", "tls", "quic"], "metadataOnly": false, "routeOnly": false}
    })
}

/// AnyTLS 的回环桥接入站：sing-box 卸掉 AnyTLS 之后经它把连接交给 Xray，入口的路由、访问策略、守卫规则照常生效。
/// 只听 127.0.0.1、VLESS 不加密（回环上没有第三方）、不用 flow（vision 要 TLS）；client 与 email 同主入站：
/// 流量计数按 email 合并到入口，访问日志按入站 tag 认入口（来源是回环，不当真实来源，见 access.rs）。sniffing 同主入站
fn bridge_inbound_json(e: &DesiredEntry, port: u16) -> Value {
    json!({
        "listen": "127.0.0.1",
        "port": port,
        "protocol": "vless",
        "tag": tags::bridge(e.id),
        "settings": {
            "clients": [{"id": e.client_uuid.to_string(), "email": tags::email(e.id)}],
            "decryption": "none"
        },
        "streamSettings": {"network": "tcp", "security": "none"},
        "sniffing": {"enabled": true, "destOverride": ["http", "tls", "quic"], "metadataOnly": false, "routeOnly": false}
    })
}

/// 入口自己的入站 tag：主入站，有 AnyTLS 时加上桥接入站。入口的路由、访问策略、守卫规则都匹配这一组
fn entry_inbounds(e: &DesiredEntry, anytls: bool) -> Vec<String> {
    let mut v = vec![tags::inbound(e.id)];
    if anytls {
        v.push(tags::bridge(e.id));
    }
    v
}

fn dest_with_port(d: &str) -> String {
    if d.starts_with('[') && d.contains("]:") || (!d.starts_with('[') && d.matches(':').count() == 1) {
        d.to_string()
    } else {
        format!("{d}:443")
    }
}

pub fn policy_tag(id: Uuid, part: &str) -> String {
    format!("policy-{}-{part}", id.simple())
}

/// 直出入口的守卫规则：目标端口是本机 Xray API、任一入口的探测入站或 AnyTLS 桥接入站（`ports`）的连接一律 blocked，排在该入口其它规则之前。
/// 路由是 AsIs，block-private 只拦 IP 目标；客户写一个解析到回环的域名（localhost、127.0.0.1.nip.io），
/// freedom 会在线路机本机替他解析并连上 API（可增删入站出站）或探测入站。按端口拦和怎么解析无关。
/// 不改成 IPIfNonMatch 让域名先解析再过 block-private：解析结果落在 198.18.0.0/15 的 fake-ip DNS 环境（实验室）里，
/// 所有域名都会被当成内网拦掉
fn guard_rule(e: &DesiredEntry, inb: &[String], ports: &str) -> Value {
    json!({"type": "field", "ruleTag": format!("guard-{}-api", e.id.simple()), "inboundTag": inb, "port": ports, "outboundTag": "blocked"})
}

/// 探测入站只放行到回显服务（echo_url 的主机与端口）和出口探测的备用目标（probe_matchers）：探测入站是回环上不要密码的 SOCKS，守卫规则之外它也不该是跳板——
/// 不限目标的话，连上它就能经它连本机 Xray API（增删全部入口）、借别的入口的出站出去。别的目标不匹配任何规则，
/// 落到默认出站（第一个，deny 黑洞）。探测走 socks5h，SOCKS 请求里就是 URL 里的主机名（或 IP）与端口
fn echo_target(url: &str) -> Option<Value> {
    let (host, port, ip) = url_target(url)?;
    Some(if ip { json!({"ip": [host], "port": port}) } else { json!({"domain": [format!("full:{host}")], "port": port}) })
}

/// URL → (主机, 端口, 主机是不是 IP)
fn url_target(url: &str) -> Option<(String, String, bool)> {
    let u = reqwest::Url::parse(url).ok()?;
    let host = u.host_str()?;
    let port = u.port_or_known_default()?.to_string();
    Some(match host.trim_start_matches('[').trim_end_matches(']').parse::<std::net::IpAddr>() {
        Ok(ip) => (ip.to_string(), port, true),
        Err(_) => (host.to_string(), port, false),
    })
}

/// 探测入站放行的目标：回显服务，加上出口探测的备用目标（probe::EGRESS_ALT_URL，回显服务经入口不通时核对用）。
/// 两个都是域名时合成一条（域名、端口各取并集，入口的规则条数不变）；有一个是 IP 时分开（Xray 规则里 domain 与 ip 要同时满足）。
/// 没有回显地址时什么都不放行
fn probe_matchers(echo_url: &str) -> Vec<Value> {
    let Some(echo) = echo_target(echo_url) else { return vec![] };
    let alt = crate::probe::EGRESS_ALT_URL;
    match (url_target(echo_url), url_target(alt)) {
        (Some((h1, p1, false)), Some((h2, p2, false))) => {
            let mut domains = vec![format!("full:{h1}")];
            if h1 != h2 {
                domains.push(format!("full:{h2}"));
            }
            let port = if p1 == p2 { p1 } else { format!("{p1},{p2}") };
            vec![json!({"domain": domains, "port": port})]
        }
        (_, Some(_)) => vec![echo, echo_target(alt).expect("常量是合法 URL")],
        _ => vec![echo],
    }
}

/// 第一条用 `probe-<入口>`，其余（备用目标）`probe-<入口>-<n>`
fn probe_rule(id: Uuid, n: usize, m: &Value) -> Value {
    let mut r = m.clone();
    r["type"] = json!("field");
    r["ruleTag"] = json!(if n == 0 { probe_tag(id) } else { format!("{}-{n}", probe_tag(id)) });
    r["inboundTag"] = json!([probe_tag(id)]);
    r["outboundTag"] = json!(tags::outbound(id));
    r
}

/// 访问策略规则：排在该入口默认路由规则之前，只匹配该入口自己的入站（主入站与 AnyTLS 桥接入站；探测入站有独立规则，不受影响）。
/// - allowlist：名单内走原出站，其余 blocked
/// - denylist：名单内 blocked，其余落到默认规则
/// - blocked：该入口全部 blocked
///
/// observe（没有 policy）不产生规则。
fn policy_rules(e: &DesiredEntry, inb: &[String]) -> Vec<Value> {
    let Some(EntryPolicy { mode, domains }) = &e.policy else { return vec![] };
    let rule = |part: &str, domains: Option<&Vec<String>>, out: String| {
        let mut r = json!({"type": "field", "ruleTag": policy_tag(e.id, part), "inboundTag": inb, "outboundTag": out});
        if let Some(d) = domains {
            r["domain"] = json!(d);
        }
        r
    };
    match mode {
        PolicyMode::Allowlist => vec![
            rule("match", Some(domains), tags::outbound(e.id)),
            rule("default", None, "blocked".into()),
        ],
        PolicyMode::Denylist => vec![rule("match", Some(domains), "blocked".into())],
        PolicyMode::Blocked => vec![rule("default", None, "blocked".into())],
    }
}

/// 域名匹配串只接受 Xray 的 `domain:` / `full:` 形式 + 保守字符集，避免把中控传来的任意串写进配置
fn valid_matcher(m: &str) -> bool {
    let Some(rest) = m.strip_prefix("domain:").or_else(|| m.strip_prefix("full:")) else { return false };
    !rest.is_empty()
        && rest.len() <= 253
        && rest.contains('.')
        && rest.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
}

fn probe_inbound_json(id: Uuid, port: u16) -> Value {
    json!({
        "listen": "127.0.0.1",
        "port": port,
        "protocol": "socks",
        "tag": probe_tag(id),
        "settings": {"auth": "noauth", "udp": false}
    })
}

fn validate(d: &DesiredState) -> Result<String> {
    let rp = &d.relay;
    ensure!(!rp.reality_private_key.trim().is_empty(), "reality_private_key 为空");
    ensure!(!rp.reality_sni.trim().is_empty(), "reality_sni 为空");
    ensure!(!rp.reality_dest.trim().is_empty(), "reality_dest 为空");
    let min_ver = rp.min_client_ver.clone().unwrap_or_else(|| MAX_MIN_CLIENT_VER.to_string());
    let (Some(v), Some(max)) = (parse_ver(&min_ver), parse_ver(MAX_MIN_CLIENT_VER)) else {
        bail!("min_client_ver 格式无效：{min_ver}");
    };
    ensure!(v <= max, "min_client_ver {min_ver} 高于 {MAX_MIN_CLIENT_VER}，会让 mihomo 客户端被拒");
    let mut ids = HashSet::new();
    let mut ports = HashSet::new();
    for e in d.entries.iter().filter(|e| e.enabled) {
        ensure!(ids.insert(e.id), "入口 id 重复：{}", e.id);
        ensure!(e.port != 0 && e.port != API_PORT, "入口 {} 端口 {} 不可用", e.id, e.port);
        ensure!(ports.insert(e.port), "入口端口重复：{}", e.port);
        let sid = &e.short_id;
        ensure!(
            sid.len() <= 16 && sid.len() % 2 == 0 && sid.bytes().all(|b| b.is_ascii_hexdigit()),
            "入口 {} 的 short_id 必须是不超过 16 位的偶数长度十六进制：{sid:?}", e.id
        );
        if let Some((host, port)) = e.outbound.addr() {
            ensure!(!host.trim().is_empty() && port != 0, "入口 {} 出站地址无效", e.id);
        }
        if let Some(p) = &e.policy {
            match p.mode {
                PolicyMode::Allowlist | PolicyMode::Denylist => {
                    ensure!(!p.domains.is_empty(), "入口 {} 的访问策略没有域名", e.id);
                    ensure!(p.domains.len() <= 1000, "入口 {} 的访问策略域名过多（{}）", e.id, p.domains.len());
                    if let Some(bad) = p.domains.iter().find(|d| !valid_matcher(d)) {
                        bail!("入口 {} 的访问策略域名无效：{bad:?}", e.id);
                    }
                }
                PolicyMode::Blocked => {}
            }
        }
    }
    Ok(min_ver)
}

fn skeleton_parts(ctx: &RenderCtx) -> (Value, Value, Value, Value) {
    let log = json!({"access": ctx.access_log, "error": "", "loglevel": "warning"});
    // API 用 api.listen 直接监听，不经入站与路由：否则每次调 `xray api` 都在访问日志里记一行 [api -> api]
    let api = json!({
        "tag": API_TAG, "listen": format!("{API_HOST}:{API_PORT}"),
        "services": ["HandlerService", "StatsService", "RoutingService"]
    });
    let policy = json!({
        "levels": {"0": {"statsUserUplink": true, "statsUserDownlink": true}},
        "system": {"statsInboundUplink": true, "statsInboundDownlink": true}
    });
    (log, api, json!({}), policy)
}

pub fn render(d: &DesiredState, ctx: &RenderCtx) -> Result<Rendered> {
    let min_ver = validate(d)?;
    let (log, api, stats, policy) = skeleton_parts(ctx);
    let probes = probe_ports(&d.entries);
    let anytls = anytls_ports(&d.entries);
    let enabled: Vec<&DesiredEntry> = d.entries.iter().filter(|e| e.enabled).collect();
    let matchers = probe_matchers(&d.relay.echo_url);
    // 守卫规则拦的端口：本机 Xray API、全部探测入站与 AnyTLS 桥接入站（升序：同样的入口渲染出同样的规则，不会白白整表替换）。
    // 桥接入站要 UUID，但别的入口的 UUID 不该成为直出客户在线路机本机上的跳板，一并拦
    let mut guard: Vec<u16> = probes.values().copied().chain(anytls.values().map(|p| p.1)).chain([API_PORT]).collect();
    guard.sort_unstable();
    let guard = guard.iter().map(u16::to_string).collect::<Vec<_>>().join(",");

    let mut inbounds = vec![];
    let mut outbounds = vec![
        json!({"tag": "deny", "protocol": "blackhole"}),
        json!({"tag": "direct", "protocol": "freedom", "settings": {"domainStrategy": "AsIs"}}),
        json!({"tag": "blocked", "protocol": "blackhole"}),
    ];
    // 探测入站（本机回环）的规则排在内网屏蔽之前：回显服务可能在内网（实验室），而客户流量仍然禁止访问内网
    let mut rules = vec![];
    let mut entry_rules = vec![];
    for e in &enabled {
        inbounds.push(inbound_json(e, &d.relay, &min_ver));
        if let Some(&(_, bridge)) = anytls.get(&e.id) {
            inbounds.push(bridge_inbound_json(e, bridge));
        }
        let inb = entry_inbounds(e, anytls.contains_key(&e.id));
        outbounds.push(outbound_json(e.id, &e.outbound));
        if let Some(p) = probes.get(&e.id) {
            inbounds.push(probe_inbound_json(e.id, *p));
            rules.extend(matchers.iter().enumerate().map(|(n, m)| probe_rule(e.id, n, m)));
        }
        if e.outbound == Outbound::Direct {
            entry_rules.push(guard_rule(e, &inb, &guard));
        }
        entry_rules.extend(policy_rules(e, &inb));
        entry_rules.push(json!({"type": "field", "ruleTag": tags::rule(e.id), "inboundTag": inb, "outboundTag": tags::outbound(e.id)}));
    }
    rules.push(json!({"type": "field", "ruleTag": "block-private", "ip": ["geoip:private"], "outboundTag": "blocked"}));
    rules.push(json!({"type": "field", "ruleTag": "block-bittorrent", "protocol": ["bittorrent"], "outboundTag": "blocked"}));
    rules.extend(entry_rules);
    let full = json!({
        "log": log, "api": api, "stats": stats, "policy": policy,
        "inbounds": inbounds, "outbounds": outbounds,
        "routing": {"domainStrategy": "AsIs", "rules": rules}
    });
    Ok(Rendered::from_full(full))
}

/// 没有任何入口的骨架（首次启动、Xray 配置文件缺失时用）
pub fn skeleton(ctx: &RenderCtx) -> Rendered {
    let d = DesiredState {
        generation: 0,
        relay: RelayParams {
            reality_dest: "x:443".into(),
            reality_sni: "x".into(),
            reality_private_key: "x".into(),
            min_client_ver: None,
            echo_url: String::new(),
            probe_interval_s: 30,
        },
        entries: vec![],
        ping: None,
    };
    render(&d, ctx).expect("空骨架必然合法")
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::net::IpAddr;

    pub fn ctx() -> RenderCtx {
        RenderCtx { access_log: "/var/log/fleet-xray/access.log".into() }
    }

    pub fn entry(n: u128, port: u16, enabled: bool) -> DesiredEntry {
        DesiredEntry {
            id: Uuid::from_u128(n),
            port,
            enabled,
            remark: format!("e{n}"),
            client_uuid: Uuid::from_u128(n + 1000),
            short_id: "0123abcd".into(),
            outbound: Outbound::Shadowsocks { host: "10.0.0.9".into(), port: 8388, method: "aes-256-gcm".into(), password: "pw".into() },
            expect_egress_ip: "203.0.113.9".parse::<IpAddr>().unwrap(),
            policy: None,
            anytls_port: None,
        }
    }

    pub fn desired(entries: Vec<DesiredEntry>) -> DesiredState {
        DesiredState {
            generation: 7,
            relay: RelayParams {
                reality_dest: "www.cloudflare.com:443".into(),
                reality_sni: "www.cloudflare.com".into(),
                reality_private_key: "PRIVKEY".into(),
                min_client_ver: Some("1.8.2".into()),
                echo_url: "http://echo.example/ip".into(),
                probe_interval_s: 30,
            },
            entries,
            ping: None,
        }
    }

    #[test]
    fn golden_single_entry() {
        let r = render(&desired(vec![entry(1, 20001, true)]), &ctx()).unwrap();
        let id = Uuid::from_u128(1).simple().to_string();
        let expected = json!({
            "log": {"access": "/var/log/fleet-xray/access.log", "error": "", "loglevel": "warning"},
            "api": {"tag": "api", "listen": "127.0.0.1:62789", "services": ["HandlerService", "StatsService", "RoutingService"]},
            "stats": {},
            "policy": {
                "levels": {"0": {"statsUserUplink": true, "statsUserDownlink": true}},
                "system": {"statsInboundUplink": true, "statsInboundDownlink": true}
            },
            "inbounds": [
                {
                    "listen": "0.0.0.0", "port": 20001, "protocol": "vless", "tag": format!("in-{id}"),
                    "settings": {
                        "clients": [{"id": Uuid::from_u128(1001).to_string(), "email": format!("e-{id}"), "flow": "xtls-rprx-vision"}],
                        "decryption": "none", "fallbacks": []
                    },
                    "streamSettings": {
                        "network": "tcp", "security": "reality",
                        "tcpSettings": {"acceptProxyProtocol": false, "header": {"type": "none"}},
                        "realitySettings": {
                            "show": false, "xver": 0, "target": "www.cloudflare.com:443",
                            "serverNames": ["www.cloudflare.com"], "privateKey": "PRIVKEY",
                            "minClientVer": "1.8.2", "maxClientVer": "", "maxTimeDiff": 0, "shortIds": ["0123abcd"]
                        }
                    },
                    "sniffing": {"enabled": true, "destOverride": ["http", "tls", "quic"], "metadataOnly": false, "routeOnly": false}
                },
                {"listen": "127.0.0.1", "port": 30001, "protocol": "socks", "tag": format!("probe-{id}"), "settings": {"auth": "noauth", "udp": false}}
            ],
            "outbounds": [
                {"tag": "deny", "protocol": "blackhole"},
                {"tag": "direct", "protocol": "freedom", "settings": {"domainStrategy": "AsIs"}},
                {"tag": "blocked", "protocol": "blackhole"},
                {"tag": format!("out-{id}"), "protocol": "shadowsocks",
                 "settings": {"servers": [{"address": "10.0.0.9", "port": 8388, "method": "aes-256-gcm", "password": "pw"}]}}
            ],
            "routing": {"domainStrategy": "AsIs", "rules": [
                {"type": "field", "ruleTag": format!("probe-{id}"), "inboundTag": [format!("probe-{id}")], "domain": ["full:echo.example", "full:www.gstatic.com"], "port": "80,443", "outboundTag": format!("out-{id}")},
                {"type": "field", "ruleTag": "block-private", "ip": ["geoip:private"], "outboundTag": "blocked"},
                {"type": "field", "ruleTag": "block-bittorrent", "protocol": ["bittorrent"], "outboundTag": "blocked"},
                {"type": "field", "ruleTag": format!("rule-{id}"), "inboundTag": [format!("in-{id}")], "outboundTag": format!("out-{id}")}
            ]}
        });
        assert_eq!(r.full, expected);
        // 第一个出站必须是默认拒绝
        assert_eq!(tag_of(&r.outbounds[0]), "deny");
        assert_eq!(r.rules.len(), 4);
    }

    #[test]
    fn disabled_entries_omitted_and_other_outbounds() {
        let mut e2 = entry(2, 20002, true);
        e2.outbound = Outbound::Socks5 { host: "h".into(), port: 1080, username: Some("u".into()), password: Some("p".into()) };
        let mut e3 = entry(3, 20003, true);
        e3.outbound = Outbound::Http { host: "h".into(), port: 3128, username: None, password: None };
        let r = render(&desired(vec![entry(1, 20001, false), e2.clone(), e3.clone()]), &ctx()).unwrap();
        let s = r.full.to_string();
        assert!(!s.contains(&Uuid::from_u128(1).simple().to_string()));
        let o2 = r.outbounds.iter().find(|o| tag_of(o) == tags::outbound(e2.id)).unwrap();
        assert_eq!(o2["protocol"], "socks");
        assert_eq!(o2["settings"]["servers"][0]["users"][0], json!({"user": "u", "pass": "p"}));
        let o3 = r.outbounds.iter().find(|o| tag_of(o) == tags::outbound(e3.id)).unwrap();
        assert_eq!(o3["protocol"], "http");
        assert!(o3["settings"]["servers"][0].get("users").is_none());
    }

    #[test]
    fn validation() {
        // 端口重复
        assert!(render(&desired(vec![entry(1, 20001, true), entry(2, 20001, true)]), &ctx()).is_err());
        // 禁用入口的端口不参与冲突
        assert!(render(&desired(vec![entry(1, 20001, false), entry(2, 20001, true)]), &ctx()).is_ok());
        // minClientVer 超过 1.8.2
        let mut d = desired(vec![]);
        d.relay.min_client_ver = Some("1.9.0".into());
        assert!(render(&d, &ctx()).is_err());
        d.relay.min_client_ver = None;
        assert!(render(&d, &ctx()).is_ok());
        // short_id 非法
        let mut e = entry(1, 20001, true);
        e.short_id = "xyz".into();
        assert!(render(&desired(vec![e]), &ctx()).is_err());
    }

    #[test]
    fn probe_port_rules() {
        let es = vec![entry(1, 20001, true), entry(2, 30001, true), entry(3, 60000, true), entry(4, 20004, false)];
        let p = probe_ports(&es);
        // 20001+10000=30001 与入口 2 冲突；60000+10000 溢出
        assert!(!p.contains_key(&es[0].id));
        assert_eq!(p[&es[1].id], 40001);
        assert!(!p.contains_key(&es[2].id));
        assert!(!p.contains_key(&es[3].id));
    }

    #[test]
    fn dest_port_default() {
        assert_eq!(dest_with_port("a.com"), "a.com:443");
        assert_eq!(dest_with_port("a.com:8443"), "a.com:8443");
        assert_eq!(dest_with_port("[::1]:443"), "[::1]:443");
    }

    fn with_policy(n: u128, port: u16, mode: PolicyMode, domains: &[&str]) -> DesiredEntry {
        let mut e = entry(n, port, true);
        e.policy = Some(EntryPolicy { mode, domains: domains.iter().map(|d| d.to_string()).collect() });
        e
    }

    fn rule_tags(r: &Rendered) -> Vec<&str> {
        r.rules.iter().map(rule_tag_of).collect()
    }

    #[test]
    fn policy_rules_four_modes() {
        let id = Uuid::from_u128(1).simple().to_string();
        let ib = format!("in-{id}");
        let ob = format!("out-{id}");
        let doms = ["domain:a.com", "full:b.com"];

        // observe：没有任何策略规则，和没有 policy 字段完全一致
        let plain = render(&desired(vec![entry(1, 20001, true)]), &ctx()).unwrap();
        assert!(!rule_tags(&plain).iter().any(|t| t.starts_with("policy-")));

        // allowlist：名单内走原出站，其余 blocked，都在入口默认规则之前
        let r = render(&desired(vec![with_policy(1, 20001, PolicyMode::Allowlist, &doms)]), &ctx()).unwrap();
        assert_eq!(
            rule_tags(&r),
            vec![format!("probe-{id}"), "block-private".to_string(), "block-bittorrent".into(),
                 format!("policy-{id}-match"), format!("policy-{id}-default"), format!("rule-{id}")]
        );
        assert_eq!(
            r.rules[3],
            json!({"type": "field", "ruleTag": format!("policy-{id}-match"), "inboundTag": [ib.clone()], "domain": doms, "outboundTag": ob.clone()})
        );
        assert_eq!(
            r.rules[4],
            json!({"type": "field", "ruleTag": format!("policy-{id}-default"), "inboundTag": [ib.clone()], "outboundTag": "blocked"})
        );

        // denylist：只有一条「名单内 → blocked」
        let r = render(&desired(vec![with_policy(1, 20001, PolicyMode::Denylist, &doms)]), &ctx()).unwrap();
        assert_eq!(rule_tags(&r)[3..], [format!("policy-{id}-match"), format!("rule-{id}")]);
        assert_eq!(r.rules[3]["outboundTag"], "blocked");
        assert_eq!(r.rules[3]["domain"], json!(doms));

        // blocked：该入口全部 blocked，没有域名字段
        let r = render(&desired(vec![with_policy(1, 20001, PolicyMode::Blocked, &[])]), &ctx()).unwrap();
        assert_eq!(rule_tags(&r)[3..], [format!("policy-{id}-default"), format!("rule-{id}")]);
        assert!(r.rules[3].get("domain").is_none());
        assert_eq!(r.rules[3]["outboundTag"], "blocked");
    }

    #[test]
    fn policy_does_not_touch_probe_or_other_entries_and_keeps_static_part() {
        let r = render(
            &desired(vec![with_policy(1, 20001, PolicyMode::Blocked, &[]), entry(2, 20002, true)]),
            &ctx(),
        )
        .unwrap();
        let p1 = format!("probe-{}", Uuid::from_u128(1).simple());
        let probe_rule = r.rules.iter().find(|x| rule_tag_of(x) == p1).unwrap();
        // 探测入站的规则原样：只匹配探测入站、走入口自己的出站，且排在策略规则之前
        assert_eq!(probe_rule["inboundTag"], json!([p1.clone()]));
        assert_eq!(probe_rule["outboundTag"], format!("out-{}", Uuid::from_u128(1).simple()));
        let pos = |t: &str| r.rules.iter().position(|x| rule_tag_of(x) == t).unwrap();
        assert!(pos(&p1) < pos(&format!("policy-{}-default", Uuid::from_u128(1).simple())));
        // 其它入口没有策略规则
        let id2 = Uuid::from_u128(2).simple().to_string();
        assert!(!rule_tags(&r).iter().any(|t| t.starts_with("policy-") && t.contains(&id2)));
        // 策略规则属于「入口规则」：不进 static_part，不会因为策略变化而要求重启 Xray
        let base = render(&desired(vec![entry(1, 20001, true), entry(2, 20002, true)]), &ctx()).unwrap();
        assert_eq!(base.static_part, r.static_part);
        assert_eq!(Rendered::from_full(r.full.clone()), r);
    }

    #[test]
    fn policy_validation() {
        let bad = |mode, domains: &[&str]| render(&desired(vec![with_policy(1, 20001, mode, domains)]), &ctx()).is_err();
        assert!(bad(PolicyMode::Allowlist, &[]));
        assert!(bad(PolicyMode::Denylist, &[]));
        assert!(bad(PolicyMode::Allowlist, &["a.com"]));
        assert!(bad(PolicyMode::Allowlist, &["domain:"]));
        assert!(bad(PolicyMode::Denylist, &["domain:a.com
x"]));
        assert!(bad(PolicyMode::Denylist, &["regexp:.*"]));
        assert!(bad(PolicyMode::Denylist, &["domain:A.com"]));
        assert!(!bad(PolicyMode::Blocked, &[]));
        assert!(!bad(PolicyMode::Denylist, &["domain:a-b.example.com", "full:x.y"]));
    }

    pub fn direct(n: u128, port: u16) -> DesiredEntry {
        let mut e = entry(n, port, true);
        e.outbound = Outbound::Direct;
        e.expect_egress_ip = "198.51.100.2".parse().unwrap();
        e
    }

    #[test]
    fn direct_entry_renders_own_freedom_outbound() {
        let r = render(&desired(vec![entry(1, 20001, true), direct(2, 20002)]), &ctx()).unwrap();
        let (id1, id2) = (Uuid::from_u128(1).simple().to_string(), Uuid::from_u128(2).simple().to_string());
        // 入口自己的 freedom 出站，不复用共享的 direct
        let o = r.outbounds.iter().find(|o| tag_of(o) == format!("out-{id2}")).unwrap();
        assert_eq!(o, &json!({"tag": format!("out-{id2}"), "protocol": "freedom", "settings": {"domainStrategy": "AsIs"}}));
        assert_eq!(r.outbounds.iter().filter(|o| o["protocol"] == "freedom").count(), 2, "共享 direct + 直出入口自己的");
        // 入站与探测入站和节点入口同形
        let inb = r.inbounds.iter().find(|i| tag_of(i) == format!("in-{id2}")).unwrap();
        assert_eq!(inb["protocol"], "vless");
        assert_eq!(inb["port"], 20002);
        assert!(r.inbounds.iter().any(|i| tag_of(i) == format!("probe-{id2}") && i["port"] == 30002));
        // 规则顺序：探测 → 内网 / BT 屏蔽（同节点入口）→ 各入口；直出入口先过「目标端口是本机 Xray API → blocked」
        assert_eq!(
            rule_tags(&r),
            vec![
                format!("probe-{id1}"), format!("probe-{id2}"), "block-private".to_string(), "block-bittorrent".into(),
                format!("rule-{id1}"), format!("guard-{id2}-api"), format!("rule-{id2}")
            ]
        );
        // 守卫拦本机 API 与全部探测入站（含节点入口的）
        assert_eq!(
            r.rules[5],
            json!({"type": "field", "ruleTag": format!("guard-{id2}-api"), "inboundTag": [format!("in-{id2}")], "port": "30001,30002,62789", "outboundTag": "blocked"})
        );
        assert_eq!(r.rules[6], json!({"type": "field", "ruleTag": format!("rule-{id2}"), "inboundTag": [format!("in-{id2}")], "outboundTag": format!("out-{id2}")}));
        assert!(!rule_tags(&r).contains(&format!("guard-{id1}-api").as_str()), "节点入口没有守卫规则（出口在节点上）");
        assert_eq!(r.full["routing"]["domainStrategy"], "AsIs");
        // 守卫规则属于入口规则：增删直出入口只走热增删，不重启 Xray
        assert_eq!(r.static_part, render(&desired(vec![entry(1, 20001, true)]), &ctx()).unwrap().static_part);
        assert_eq!(Rendered::from_full(r.full.clone()), r);
    }

    #[test]
    fn probe_rules_only_reach_the_echo_service() {
        // 回显服务的主机按 SOCKS 里的写法匹配：域名用 full:，IP（含 IPv6）用 ip；端口缺省按协议
        assert_eq!(echo_target("https://fleet.example.com/echo"), Some(json!({"domain": ["full:fleet.example.com"], "port": "443"})));
        assert_eq!(echo_target("http://192.168.31.60:8080/echo"), Some(json!({"ip": ["192.168.31.60"], "port": "8080"})));
        assert_eq!(echo_target("http://[2001:db8::1]:8080/echo"), Some(json!({"ip": ["2001:db8::1"], "port": "8080"})));
        assert_eq!(echo_target(""), None);
        // 回显服务与备用目标都是域名：合成一条；同一个主机不重复
        assert_eq!(probe_matchers("https://fleet.example.com/echo"), vec![json!({"domain": ["full:fleet.example.com", "full:www.gstatic.com"], "port": "443"})]);
        assert_eq!(probe_matchers("https://www.gstatic.com/ip"), vec![json!({"domain": ["full:www.gstatic.com"], "port": "443"})]);
        assert_eq!(probe_matchers(""), Vec::<Value>::new());
        // 内网的回显服务（实验室）照样放行：探测规则排在 block-private 之前；别的目标不匹配任何规则，落到默认的 deny。
        // 回显服务是 IP：备用目标单独一条 probe-<入口>-1
        let mut d = desired(vec![entry(1, 20001, true), direct(2, 20002)]);
        d.relay.echo_url = "http://192.168.31.60:8080/echo".into();
        let r = render(&d, &ctx()).unwrap();
        let probe_rules: Vec<&Value> = r.rules.iter().filter(|x| rule_tag_of(x).starts_with("probe-")).collect();
        assert_eq!(probe_rules.len(), 4);
        for (n, rule) in probe_rules.iter().enumerate() {
            if n % 2 == 0 {
                assert_eq!((&rule["ip"], &rule["port"]), (&json!(["192.168.31.60"]), &json!("8080")), "{n}: {rule}");
            } else {
                assert_eq!((&rule["domain"], &rule["port"]), (&json!(["full:www.gstatic.com"]), &json!("443")), "{n}: {rule}");
                assert!(rule_tag_of(rule).ends_with("-1") && rule["outboundTag"] == probe_rules[n - 1]["outboundTag"], "{n}: {rule}");
            }
        }
        assert_eq!(tag_of(&r.outbounds[0]), "deny");
        // 没有回显地址：探测入站没有放行规则，全落到 deny
        d.relay.echo_url = String::new();
        let r = render(&d, &ctx()).unwrap();
        assert!(!rule_tags(&r).iter().any(|t| t.starts_with("probe-")));
    }

    #[test]
    fn direct_entry_guard_precedes_policy_rules() {
        // 线路机目前没有访问策略，但万一下发了：守卫在策略规则之前（白名单里的域名也连不到本机 API），策略照常
        let mut e = direct(1, 20001);
        e.policy = Some(EntryPolicy { mode: PolicyMode::Allowlist, domains: vec!["domain:a.com".into()] });
        let r = render(&desired(vec![e]), &ctx()).unwrap();
        let id = Uuid::from_u128(1).simple().to_string();
        assert_eq!(rule_tags(&r)[3..], [format!("guard-{id}-api"), format!("policy-{id}-match"), format!("policy-{id}-default"), format!("rule-{id}")]);
        assert_eq!(r.rules[4]["outboundTag"], format!("out-{id}"));
        assert_eq!(r.rules[4]["domain"], json!(["domain:a.com"]));
        assert_eq!(r.rules[5]["outboundTag"], "blocked");
    }

    pub fn with_anytls(mut e: DesiredEntry) -> DesiredEntry {
        e.anytls_port = e.port.checked_add(fleet_proto::ANYTLS_PORT_OFFSET);
        e
    }

    #[test]
    fn anytls_bridge_inbound_and_rules_cover_both_inbounds() {
        let mut d2 = with_anytls(direct(2, 20002));
        d2.policy = Some(EntryPolicy { mode: PolicyMode::Denylist, domains: vec!["domain:a.com".into()] });
        let r = render(&desired(vec![with_anytls(entry(1, 20001, true)), d2]), &ctx()).unwrap();
        let (id1, id2) = (Uuid::from_u128(1).simple().to_string(), Uuid::from_u128(2).simple().to_string());
        // 桥接入站：回环、入口端口 + 30000、VLESS 不加密不带 flow，client 与 email 同主入站
        let b = r.inbounds.iter().find(|i| tag_of(i) == format!("at-{id1}")).unwrap();
        assert_eq!(
            b,
            &json!({
                "listen": "127.0.0.1", "port": 50001, "protocol": "vless", "tag": format!("at-{id1}"),
                "settings": {"clients": [{"id": Uuid::from_u128(1001).to_string(), "email": format!("e-{id1}")}], "decryption": "none"},
                "streamSettings": {"network": "tcp", "security": "none"},
                "sniffing": {"enabled": true, "destOverride": ["http", "tls", "quic"], "metadataOnly": false, "routeOnly": false}
            })
        );
        assert!(r.inbounds.iter().any(|i| tag_of(i) == format!("at-{id2}") && i["port"] == 50002));
        // 入口的路由、访问策略、守卫规则都同时匹配主入站与桥接入站；探测规则只匹配探测入站
        let rule = |t: String| r.rules.iter().find(|x| rule_tag_of(x) == t).unwrap().clone();
        for id in [&id1, &id2] {
            assert_eq!(rule(format!("rule-{id}"))["inboundTag"], json!([format!("in-{id}"), format!("at-{id}")]));
            assert_eq!(rule(format!("probe-{id}"))["inboundTag"], json!([format!("probe-{id}")]));
        }
        assert_eq!(rule(format!("policy-{id2}-match"))["inboundTag"], json!([format!("in-{id2}"), format!("at-{id2}")]));
        // 直出入口的守卫：API、探测入站之外，桥接入站的端口也拦
        let g = rule(format!("guard-{id2}-api"));
        assert_eq!(g["inboundTag"], json!([format!("in-{id2}"), format!("at-{id2}")]));
        assert_eq!(g["port"], "30001,30002,50001,50002,62789");
        // 桥接入站与它的规则都属于入口部分：开关 AnyTLS 只走热增删，不重启 Xray
        assert_eq!(r.static_part, render(&desired(vec![entry(1, 20001, true)]), &ctx()).unwrap().static_part);
        assert_eq!(Rendered::from_full(r.full.clone()), r);
    }

    #[test]
    fn anytls_absent_on_overflow_or_mismatch() {
        // 入口端口 40000：公网 60000 没超，桥接 70000 超出 65535 → 没有 AnyTLS（中控同一条规则，本来也不会下发）
        let mut e = entry(1, 40000, true);
        e.anytls_port = Some(60000);
        // 下发的端口和按规则算的不一致：不开
        let mut e2 = entry(2, 20002, true);
        e2.anytls_port = Some(12345);
        // 停用的入口：没有
        let e3 = with_anytls(entry(3, 20003, false));
        let ds = vec![e, e2, e3];
        assert!(anytls_ports(&ds).is_empty());
        let r = render(&desired(ds), &ctx()).unwrap();
        assert!(!r.inbounds.iter().any(|i| tag_of(i).starts_with("at-")));
        let id = Uuid::from_u128(1).simple().to_string();
        let rule = r.rules.iter().find(|x| rule_tag_of(x) == format!("rule-{id}")).unwrap();
        assert_eq!(rule["inboundTag"], json!([format!("in-{id}")]));
        // 正常的：(公网, 桥接)
        assert_eq!(anytls_ports(&[with_anytls(entry(4, 20004, true))])[&Uuid::from_u128(4)], (40004, 50004));
    }

    #[test]
    fn from_full_roundtrip_static_part() {
        let a = render(&desired(vec![entry(1, 20001, true)]), &ctx()).unwrap();
        let b = render(&desired(vec![entry(2, 20002, true), entry(3, 20003, true)]), &ctx()).unwrap();
        assert_eq!(a.static_part, b.static_part);
        assert_eq!(Rendered::from_full(a.full.clone()), a);
        assert_eq!(skeleton(&ctx()).static_part, a.static_part);
    }
}
