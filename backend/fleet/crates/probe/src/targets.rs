//! 探测对象：每个落地节点一条 [`NodeTarget`]。
//!
//! 来源（都只读）：
//! - Xray JSON（运行中的配置文件，或 relayd 自己生成的配置）：`outbounds[].protocol == "shadowsocks"`；
//! - 3x-ui 数据库 `settings.xrayTemplateConfig`（现网 3x-ui 热加的出站只在这里，config.json 可能落后）；
//! - 旧 `komari-relay-probe` 的 config.json：只取 uuid / name / ip / outbound_tag，用于把出站 tag 对上节点身份
//!   和期望出口 IP（迁移期；新架构里这些来自 RelaySpec 的 ProbeTarget）。
use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;

use serde_json::Value;

use crate::types::TargetInfo;

/// 一个落地节点的探测参数。密码不实现 Debug/Serialize。
#[derive(Clone)]
pub struct NodeTarget {
    /// 节点 uid（旧系统 uuid）；没有映射时用出站 tag
    pub node: String,
    pub name: String,
    /// Xray 出站 tag（`ss-a-b-c-d`）
    pub tag: String,
    /// SS 服务端地址（IP 或域名）
    pub addr: String,
    pub port: u16,
    /// Shadowsocks 加密方式；SOCKS5 出站（NodeMach 高速代理）为 "socks5"
    pub method: String,
    /// SOCKS5 用户名（Shadowsocks 为空）
    pub username: String,
    pub password: String,
    /// 期望出口 IP
    pub expected_ip: String,
    /// 是否做 UDP STUN（节点开了 UDP relay 才有意义）
    pub udp: bool,
}

impl std::fmt::Debug for NodeTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeTarget")
            .field("node", &self.node)
            .field("tag", &self.tag)
            .field("addr", &self.addr)
            .field("port", &self.port)
            .field("method", &self.method)
            .field("expected_ip", &self.expected_ip)
            .finish_non_exhaustive()
    }
}

impl NodeTarget {
    pub fn info(&self) -> TargetInfo {
        TargetInfo {
            node: self.node.clone(),
            name: self.name.clone(),
            tag: self.tag.clone(),
            addr: self.addr.clone(),
            port: self.port,
            method: self.method.clone(),
            expected_ip: self.expected_ip.clone(),
            udp: self.udp,
        }
    }

    /// 解析 SS 服务端地址（域名走系统解析）
    pub async fn socket_addr(&self) -> std::io::Result<SocketAddr> {
        if let Ok(ip) = self.addr.parse::<IpAddr>() {
            return Ok(SocketAddr::new(ip, self.port));
        }
        tokio::net::lookup_host((self.addr.as_str(), self.port))
            .await?
            .next()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "域名无解析结果"))
    }
}

/// `ss-75-37-237-38` → `75.37.237.38`
pub fn ip_from_tag(tag: &str) -> Option<String> {
    let rest = tag.strip_prefix("ss-")?;
    let ip = rest.replace('-', ".");
    ip.parse::<std::net::Ipv4Addr>().ok().map(|a| a.to_string())
}

/// 从一份 Xray 配置（JSON 值）里取全部 SS 出站与 SOCKS5 出站（NodeMach）；兼容 `settings.servers[0]` 与扁平 `settings{address,port,…}` 两种写法
pub fn from_xray_value(cfg: &Value) -> Vec<NodeTarget> {
    let mut out = Vec::new();
    for o in cfg["outbounds"].as_array().into_iter().flatten() {
        if o["protocol"].as_str() == Some("socks") {
            let tag = o["tag"].as_str().unwrap_or("").to_string();
            let s = &o["settings"];
            let srv = s["servers"].as_array().and_then(|a| a.first()).unwrap_or(s);
            let (Some(addr), Some(port)) = (srv["address"].as_str(), srv["port"].as_u64()) else { continue };
            let u = srv["users"].as_array().and_then(|a| a.first());
            let expected = ip_from_tag(&tag).or_else(|| addr.parse::<IpAddr>().ok().map(|a| a.to_string())).unwrap_or_default();
            out.push(NodeTarget {
                node: tag.clone(),
                name: tag.clone(),
                tag,
                addr: addr.to_string(),
                port: port as u16,
                method: "socks5".into(),
                username: u.and_then(|u| u["user"].as_str()).unwrap_or("").to_string(),
                password: u.and_then(|u| u["pass"].as_str()).unwrap_or("").to_string(),
                expected_ip: expected,
                udp: false,
            });
            continue;
        }
        if o["protocol"].as_str() != Some("shadowsocks") {
            continue;
        }
        let tag = o["tag"].as_str().unwrap_or("").to_string();
        let s = &o["settings"];
        let srv = s["servers"].as_array().and_then(|a| a.first()).unwrap_or(s);
        let (Some(addr), Some(port), Some(method), Some(password)) =
            (srv["address"].as_str(), srv["port"].as_u64(), srv["method"].as_str(), srv["password"].as_str())
        else {
            continue;
        };
        let expected = ip_from_tag(&tag).or_else(|| addr.parse::<IpAddr>().ok().map(|a| a.to_string())).unwrap_or_default();
        out.push(NodeTarget {
            node: tag.clone(),
            name: tag.clone(),
            tag,
            addr: addr.to_string(),
            port: port as u16,
            method: method.to_string(),
            username: String::new(),
            password: password.to_string(),
            expected_ip: expected,
            udp: false,
        });
    }
    out
}

pub fn from_xray_file(path: &Path) -> Result<Vec<NodeTarget>, String> {
    let s = std::fs::read_to_string(path).map_err(|e| format!("读 {}：{e}", path.display()))?;
    let v: Value = serde_json::from_str(&s).map_err(|e| format!("解析 {}：{e}", path.display()))?;
    Ok(from_xray_value(&v))
}

/// 只读打开 3x-ui 数据库，取 `xrayTemplateConfig`
pub fn from_xui_db(path: &Path) -> Result<Vec<NodeTarget>, String> {
    use rusqlite::{Connection, OpenFlags};
    let c = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(|e| format!("打开 {}：{e}", path.display()))?;
    let s: String = c
        .query_row("SELECT value FROM settings WHERE key='xrayTemplateConfig'", [], |r| r.get(0))
        .map_err(|e| format!("读 xrayTemplateConfig：{e}"))?;
    let v: Value = serde_json::from_str(&s).map_err(|e| format!("解析 xrayTemplateConfig：{e}"))?;
    Ok(from_xray_value(&v))
}

/// 节点身份映射（按出站 tag）：来自旧 komari-relay-probe config.json 的 assets[]
#[derive(Clone, Debug, Default)]
pub struct Identity {
    pub node: String,
    pub name: String,
    pub expected_ip: String,
}

pub fn identities_from_legacy_assets(path: &Path) -> Result<BTreeMap<String, Identity>, String> {
    let s = std::fs::read_to_string(path).map_err(|e| format!("读 {}：{e}", path.display()))?;
    let v: Value = serde_json::from_str(&s).map_err(|e| format!("解析 {}：{e}", path.display()))?;
    let mut m = BTreeMap::new();
    for a in v["assets"].as_array().into_iter().flatten() {
        let Some(tag) = a["outbound_tag"].as_str() else { continue };
        m.insert(
            tag.to_string(),
            Identity {
                node: a["uuid"].as_str().unwrap_or(tag).to_string(),
                name: a["name"].as_str().unwrap_or("").to_string(),
                expected_ip: a["ip"].as_str().unwrap_or("").to_string(),
            },
        );
    }
    Ok(m)
}

/// 按 tag 合并多个来源（后来的覆盖先来的），再套上身份映射
pub fn merge(sources: Vec<Vec<NodeTarget>>, ids: &BTreeMap<String, Identity>) -> Vec<NodeTarget> {
    let mut by_tag: BTreeMap<String, NodeTarget> = BTreeMap::new();
    for list in sources {
        for t in list {
            by_tag.insert(t.tag.clone(), t);
        }
    }
    by_tag
        .into_values()
        .map(|mut t| {
            if let Some(id) = ids.get(&t.tag) {
                t.node = id.node.clone();
                if !id.name.is_empty() {
                    t.name = id.name.clone();
                }
                if !id.expected_ip.is_empty() {
                    t.expected_ip = id.expected_ip.clone();
                }
            }
            t
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_outbound_shapes() {
        let v: Value = serde_json::json!({"outbounds":[
            {"tag":"direct","protocol":"freedom"},
            {"tag":"ss-75-37-237-38","protocol":"shadowsocks","settings":{"servers":[{"address":"75.37.237.38","port":8388,"method":"aes-256-gcm","password":"p"}]}},
            {"tag":"x","protocol":"shadowsocks","settings":{"address":"10.0.0.1","port":1,"method":"2022-blake3-aes-256-gcm","password":"q"}}
        ]});
        let t = from_xray_value(&v);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].expected_ip, "75.37.237.38");
        assert_eq!(t[1].expected_ip, "10.0.0.1");
        assert!(!format!("{:?}", t[0]).contains("password"));
        let v: Value = serde_json::json!({"outbounds":[
            {"tag":"relay-32667","protocol":"socks","settings":{"servers":[{"address":"u.a0t.net","port":31485,"users":[{"user":"u1","pass":"p1"}]}]}}]});
        let t = from_xray_value(&v);
        assert_eq!((t[0].method.as_str(), t[0].username.as_str(), t[0].password.as_str(), t[0].expected_ip.as_str()), ("socks5", "u1", "p1", ""));
        assert!(!format!("{:?}", t[0]).contains("p1"));
    }
}
