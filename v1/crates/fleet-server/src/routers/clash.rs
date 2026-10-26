//! OpenClash（mihomo）配置里的出口，和 fleet 入口对上号。
//!
//! 配置文件 sha256 变了才经隧道取回来（顶层 `secret:` 行在路由上先删掉），只在内存里解析 `proxies`：留 name / type / server / port；
//! uuid、flow、servername、reality-opts（public-key、short-id）只用来和入口当前的参数比对、算出 sync，不落库、不输出。
//! server:port 对上「线路机的 public_host 或公网 IP + 入口端口」就是这个入口（端口永不复用，含已删除的入口）：
//! - ok：参数和入口一致；
//! - drift：对上了入口，但入口已删除 / 正在删除 / 已停用，或 uuid / SNI / 公钥 / short-id / flow 和入口现在的不一样（凭据轮换、换线路机之后没同步）；
//! - unmanaged：不是 fleet 的入口（或配置里没有它）。

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 配置文件最多取这么多字节（ssh 输出的上限是 1 MiB）
pub const MAX_CONFIG: usize = 1024 * 1024;

#[derive(Deserialize)]
struct Cfg {
    #[serde(default)]
    proxies: Vec<P>,
    dns: Option<Dns>,
}

/// dns 段里能用 `#组名` 指定走哪个出口 / 策略组的几处：nameserver-policy 的值（字符串或列表）、nameserver、fallback、proxy-server-nameserver
#[derive(Deserialize)]
struct Dns {
    #[serde(rename = "nameserver-policy", default)]
    policy: std::collections::BTreeMap<String, OneOrMany>,
    #[serde(default)]
    nameserver: OneOrMany,
    #[serde(default)]
    fallback: OneOrMany,
    #[serde(rename = "proxy-server-nameserver", default)]
    proxy_server: OneOrMany,
}

#[derive(Deserialize, Default)]
#[serde(untagged)]
enum OneOrMany {
    #[default]
    None,
    One(String),
    Many(Vec<String>),
}

impl OneOrMany {
    fn items(&self) -> &[String] {
        match self {
            OneOrMany::None => &[],
            OneOrMany::One(s) => std::slice::from_ref(s),
            OneOrMany::Many(v) => v,
        }
    }
}

/// DNS 服务器写法里 `#` 后面的组名（`https://dns.example/dns-query#EXIT_1&h3=true` → `EXIT_1`）
fn dns_group(s: &str) -> Option<String> {
    let name = s.split_once('#')?.1.split('&').next()?.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// 全部按字符串收：按目标类型解析，`short-id: 01234567` 不会被当成数字丢掉前导 0，`port: 443` 拿到 "443"
#[derive(Deserialize)]
struct P {
    name: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    server: Option<String>,
    port: Option<String>,
    uuid: Option<String>,
    flow: Option<String>,
    servername: Option<String>,
    sni: Option<String>,
    #[serde(rename = "reality-opts")]
    reality: Option<Reality>,
    #[serde(rename = "dialer-proxy")]
    dialer: Option<String>,
}

#[derive(Deserialize)]
struct Reality {
    #[serde(rename = "public-key")]
    public_key: Option<String>,
    #[serde(rename = "short-id")]
    short_id: Option<String>,
}

/// 只用来比对的凭据：不落库、不输出（没有 Serialize）
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Creds {
    uuid: Option<String>,
    flow: Option<String>,
    sni: Option<String>,
    public_key: Option<String>,
    short_id: Option<String>,
}

impl std::fmt::Debug for Creds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("Creds(…)") }
}

/// 配置里的一个出口
#[derive(Clone, Debug)]
pub struct Proxy {
    pub name: String,
    pub kind: Option<String>,
    pub server: Option<String>,
    pub port: Option<u16>,
    pub creds: Creds,
}

/// 解析配置里的 `proxies`。没有名字的条目跳过；整份不是合法 YAML、或超出资源上限（别名爆炸之类）时报错
pub fn parse(text: &str) -> Result<Vec<Proxy>, String> { parse_all(text).map(|c| c.0) }

/// 同 `parse`，另给配置里规则之外引用出口 / 策略组的名字（去重排序）：出口的 `dialer-proxy`、dns 里 `#组名` 的几处。
/// 规则里看不到这些引用，策略组有没有在用要算上它们（collect.rs mark_used）
pub fn parse_all(text: &str) -> Result<(Vec<Proxy>, Vec<String>), String> {
    let cfg: Cfg = serde_saphyr::from_str(text).map_err(|e| {
        let m = e.to_string();
        format!("配置文件解析不了：{}", m.lines().next().unwrap_or("").chars().take(200).collect::<String>())
    })?;
    let t = |s: Option<String>| s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let mut refs: Vec<String> = cfg.proxies.iter().filter_map(|p| t(p.dialer.clone())).collect();
    if let Some(d) = &cfg.dns {
        refs.extend(d.policy.values().chain([&d.nameserver, &d.fallback, &d.proxy_server]).flat_map(|v| v.items()).filter_map(|s| dns_group(s)));
    }
    refs.sort();
    refs.dedup();
    let proxies = cfg
        .proxies
        .into_iter()
        .filter_map(|p| {
            let name = t(p.name)?;
            let (public_key, short_id) = match p.reality {
                Some(r) => (t(r.public_key), t(r.short_id)),
                None => (None, None),
            };
            Some(Proxy {
                name,
                kind: t(p.kind),
                server: t(p.server),
                port: p.port.and_then(|s| s.trim().parse().ok()),
                creds: Creds { uuid: t(p.uuid), flow: t(p.flow), sni: t(p.servername).or(t(p.sni)), public_key, short_id },
            })
        })
        .collect();
    Ok((proxies, refs))
}

/// fleet 的一个入口（含已删除的）与它现在的参数
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Entry {
    pub id: Uuid,
    pub port: i32,
    pub lifecycle: String,
    pub client_uuid: Uuid,
    pub short_id: String,
    pub node_id: Uuid,
    pub relay_id: Uuid,
    pub public_host: String,
    pub relay_ip: String,
    pub sni: String,
    pub pbk: String,
    pub relay_name: String,
    pub node_name: String,
}

pub const ENTRIES: &str = "select e.id, e.port, e.lifecycle::text lifecycle, e.client_uuid, e.short_id, e.node_id, e.relay_id,
        r.public_host, host(rm.primary_ip) relay_ip, r.reality_sni sni, r.reality_public_key pbk, coalesce(rm.label, '') relay_name,
        case when e.node_id = e.relay_id then coalesce(rm.label, '') || '-直出' else display_name(n.kind::text, n.label, t.name, n.number) end node_name
    from entry e join relay r on r.machine_id = e.relay_id join machine rm on rm.id = e.relay_id join machine n on n.id = e.node_id
    left join team t on t.id = n.team_id";

/// 接口里出口对应的入口
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct EntryRef {
    pub id: Uuid,
    /// 「团队2-03 · 线路机2:20003」
    pub name: String,
    pub node_id: Uuid,
    pub relay_id: Uuid,
    pub node_name: String,
    pub relay_name: String,
    pub port: i32,
    pub lifecycle: String,
}

/// 两个主机名 / IP 是不是同一个（大小写、方括号、末尾的点不算）
pub fn host_eq(a: &str, b: &str) -> bool {
    let n = |s: &str| s.trim().trim_start_matches('[').trim_end_matches(']').trim_end_matches('.').to_ascii_lowercase();
    n(a) == n(b)
}

/// 这个出口对上的入口与同步状态（ok / drift / unmanaged）。同一 server:port 有在用的入口就用它，否则用已删除的
pub fn match_entry(p: &Proxy, entries: &[Entry]) -> (Option<EntryRef>, &'static str) {
    let (Some(server), Some(port)) = (p.server.as_deref(), p.port) else { return (None, "unmanaged") };
    let hits = entries.iter().filter(|e| e.port == port as i32 && (host_eq(server, &e.public_host) || host_eq(server, &e.relay_ip)));
    let mut best: Option<&Entry> = None;
    for e in hits {
        if best.is_none_or(|b| b.lifecycle == "removed" && e.lifecycle != "removed") {
            best = Some(e);
        }
    }
    let Some(e) = best else { return (None, "unmanaged") };
    let same = |a: &Option<String>, b: &str| a.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(b));
    let c = &p.creds;
    let ok = !matches!(e.lifecycle.as_str(), "removed" | "removing" | "disabled")
        && p.kind.as_deref().is_some_and(|k| k.eq_ignore_ascii_case("vless"))
        && same(&c.uuid, &e.client_uuid.to_string())
        && same(&c.sni, &e.sni)
        && c.public_key.as_deref() == Some(e.pbk.as_str())
        && same(&c.short_id, &e.short_id)
        && same(&c.flow, "xtls-rprx-vision");
    let r = EntryRef {
        id: e.id,
        name: format!("{} · {}:{}", e.node_name, e.relay_name, e.port),
        node_id: e.node_id,
        relay_id: e.relay_id,
        node_name: e.node_name.clone(),
        relay_name: e.relay_name.clone(),
        port: e.port,
        lifecycle: e.lifecycle.clone(),
    };
    (Some(r), if ok { "ok" } else { "drift" })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 规则之外引用策略组的地方：出口的 dialer-proxy、dns 里 `#组名`（字符串或列表、带 & 参数）；没有 dns 段、没有 dialer 的配置为空
    #[test]
    fn group_refs_from_dialer_and_dns() {
        let (ps, refs) = parse_all(
            r#"
proxies:
  - {name: 节点01, type: vless, server: 198.51.100.2, port: 20003, dialer-proxy: 跳板}
  - {name: 节点02, type: vless, server: 198.51.100.2, port: 20004}
dns:
  nameserver: [223.5.5.5]
  nameserver-policy:
    "geosite:google": https://dns.google/dns-query#EXIT_1&h3=true
    "+.example.com": ["tls://1.1.1.1#EXIT_2", "system"]
  fallback:
    - "https://1.0.0.1/dns-query#EXIT_1"
"#,
        )
        .unwrap();
        assert_eq!(ps.len(), 2);
        assert_eq!(refs, ["EXIT_1", "EXIT_2", "跳板"]);
        assert_eq!(parse_all("proxies:
  - {name: a, type: vless, server: s, port: 1}
").unwrap().1, Vec::<String>::new());
        assert!(parse_all("dns:
  nameserver-policy: {x: [").is_err());
    }

    const UUID1: &str = "6f1c2c1e-6a9b-4c55-9d3e-8a6f1b2c3d4e";

    /// 现场配置的写法（块格式，reality-opts 两层缩进），外加流式映射、合并键、带引号与不带引号的 short-id
    const YAML: &str = r#"
mixed-port: 7890
secret-not-here: true
base: &base
  type: vless
  udp: true
proxies:
  - name: 节点01
    type: vless
    server: 198.51.100.2
    port: 20003
    uuid: 6f1c2c1e-6a9b-4c55-9d3e-8a6f1b2c3d4e
    tls: true
    udp: true
    flow: xtls-rprx-vision
    network: tcp
    servername: www.cloudflare.com
    client-fingerprint: chrome
    reality-opts:
      public-key: PBK_ABC
      short-id: 01234567
  - {name: 节点02, <<: *base, server: relay.test, port: "20004", uuid: X, servername: www.cloudflare.com, reality-opts: {public-key: PBK_ABC, short-id: 'ab'}}
  - name: 别人的
    type: ss
    server: 203.0.113.9
    port: 8388
  - type: vless
    server: nameless.test
proxy-groups:
  - {name: GLOBAL, type: select, proxies: [节点01, 节点02]}
rules:
  - MATCH,GLOBAL
"#;

    fn entry(port: i32, lifecycle: &str, short_id: &str) -> Entry {
        Entry {
            id: Uuid::now_v7(),
            port,
            lifecycle: lifecycle.into(),
            client_uuid: UUID1.parse().unwrap(),
            short_id: short_id.into(),
            node_id: Uuid::now_v7(),
            relay_id: Uuid::now_v7(),
            public_host: "relay.test".into(),
            relay_ip: "198.51.100.2".into(),
            sni: "www.cloudflare.com".into(),
            pbk: "PBK_ABC".into(),
            relay_name: "线路机2".into(),
            node_name: "团队2-03".into(),
        }
    }

    #[test]
    fn parses_proxies() {
        let ps = parse(YAML).unwrap();
        let names: Vec<&str> = ps.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["节点01", "节点02", "别人的"], "没名字的跳过");
        assert_eq!((ps[0].kind.as_deref(), ps[0].server.as_deref(), ps[0].port), (Some("vless"), Some("198.51.100.2"), Some(20003)));
        assert_eq!(ps[0].creds.short_id.as_deref(), Some("01234567"), "前导 0 保留");
        assert_eq!((ps[1].kind.as_deref(), ps[1].port), (Some("vless"), Some(20004)), "合并键、字符串端口");
        assert!(parse("proxies: [").is_err());
        assert!(parse("mixed-port: 1").unwrap().is_empty(), "没有 proxies 段");
        // 别名爆炸（十亿笑，展开有 10¹² 个节点）：一下子就返回（报错或略过），不会把中控撑爆
        let mut bomb = String::from("a: &a [x, x, x, x, x, x, x, x, x, x]\n");
        for i in 1..12u8 {
            let (c, p) = ((b'a' + i) as char, (b'a' + i - 1) as char);
            bomb.push_str(&format!("{c}: &{c} [*{p}, *{p}, *{p}, *{p}, *{p}, *{p}, *{p}, *{p}, *{p}, *{p}]\n"));
        }
        for tail in ["proxies: []\n", "proxies: *l\n", "proxies: [{name: n, server: *l}]\n"] {
            let t0 = std::time::Instant::now();
            let _ = parse(&format!("{bomb}{tail}"));
            assert!(t0.elapsed() < std::time::Duration::from_secs(2), "{tail}");
        }
    }

    #[test]
    fn matches_entries_and_sync() {
        let ps = parse(YAML).unwrap();
        let live = entry(20003, "active", "01234567");
        let (e, sync) = match_entry(&ps[0], std::slice::from_ref(&live));
        assert_eq!(sync, "ok");
        let e = e.unwrap();
        assert_eq!((e.id, e.name.as_str(), e.port), (live.id, "团队2-03 · 线路机2:20003", 20003));
        // 凭据轮换过：drift
        let rotated = Entry { client_uuid: Uuid::now_v7(), ..live.clone() };
        assert_eq!(match_entry(&ps[0], &[rotated]).1, "drift");
        // 入口已删除（换线路机后）：drift；同端口有在用的优先用在用的
        let gone = entry(20003, "removed", "01234567");
        assert_eq!(match_entry(&ps[0], std::slice::from_ref(&gone)).1, "drift");
        assert_eq!(match_entry(&ps[0], &[gone, live.clone()]).0.unwrap().id, live.id);
        // 入口已停用：线路机上没有这个入站了，也是 drift
        assert_eq!(match_entry(&ps[0], &[entry(20003, "disabled", "01234567")]).1, "drift");
        // 按 public_host 也认；uuid 不对 → drift
        assert_eq!(match_entry(&ps[1], &[entry(20004, "active", "ab")]).1, "drift");
        // 不是 fleet 的入口
        assert_eq!(match_entry(&ps[2], std::slice::from_ref(&live)), (None, "unmanaged"));
        assert_eq!(match_entry(&ps[0], &[entry(20005, "active", "01234567")]), (None, "unmanaged"));
    }

    #[test]
    fn creds_never_printed() {
        let ps = parse(YAML).unwrap();
        let dbg = format!("{:?}", ps[0]);
        assert!(!dbg.contains(UUID1) && !dbg.contains("PBK_ABC") && !dbg.contains("01234567"), "{dbg}");
    }
}
