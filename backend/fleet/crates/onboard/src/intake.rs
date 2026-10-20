//! 导入：CSV → 行规格；套餐按目录（architecture/plan-catalog/catalog.json）解析；静态校验。
//!
//! CSV 表头（顺序随意，前 7 列必填）：
//!   provider, ip, ssh_port, auth, plan, team, number [, relay, ss_port, dd, mac, service_id, note]
//! - auth：`key`（已能用上架密钥登录）或 `password:<文件路径>`（文件里只有 root 密码，0600）。
//! - plan：套餐 id（attvps.resi-vps.basic）、规范名（ATTVPS 基础版）或目录里登记过的别名（基础版）。
//! - relay：线路机名（与 onboard.toml [[relays]].name 一致）；空 = 自动放置（名下入口最少且未满的线路机）。
//! - ss_port：NAT 机（套餐 nat=true）必填，是供应商映射到节点 8388 的公网端口。
//! - dd：`none`（默认）或 `manual`（人工闸门：在供应商面板重装后继续）。

use crate::config::Config;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::net::Ipv4Addr;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RowSpec {
    pub seq: usize,
    pub provider: String,
    pub provider_code: String,
    pub ip: String,
    pub ssh_port: u16,
    /// 节点上 SS 实际监听端口（总是 8388）。
    pub ss_port: u16,
    /// 线路机连过来的端口：普通机 = 8388；NAT 机 = 映射端口。
    pub ss_public_port: u16,
    pub nat: bool,
    pub auth: String,
    pub password_file: Option<String>,
    pub plan_id: String,
    pub plan_name: String,
    pub price: String,
    pub currency: String,
    pub bandwidth_mbps: Option<u64>,
    pub team: String,
    pub number: u32,
    pub name: String,
    pub short_name: String,
    pub relay: Option<String>,
    pub dd: String,
    pub mac: Option<String>,
    pub service_id: Option<String>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Issue {
    pub seq: usize,
    pub level: &'static str, // error | warn
    pub msg: String,
}

pub struct Catalog {
    pub raw: Value,
}

pub fn norm_alias(s: &str) -> String {
    let s = s.trim().to_lowercase().replace('（', "(").replace('）', ")");
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl Catalog {
    pub fn load(path: &std::path::Path) -> Result<Catalog, String> {
        let t = std::fs::read_to_string(path).map_err(|e| format!("读套餐目录 {} 失败：{e}", path.display()))?;
        let raw: Value = serde_json::from_str(&t).map_err(|e| format!("套餐目录不是合法 JSON：{e}"))?;
        if !raw["plans"].is_array() {
            return Err("套餐目录缺少 plans".into());
        }
        Ok(Catalog { raw })
    }

    /// 按 id / 规范名 / 别名解析；只在同一供应商内匹配别名。
    pub fn resolve(&self, provider: &str, plan: &str) -> Option<&Value> {
        let plans = self.raw["plans"].as_array()?;
        let pv = provider.to_ascii_lowercase();
        if let Some(p) = plans.iter().find(|p| p["id"].as_str() == Some(plan.trim())) {
            return Some(p);
        }
        let n = norm_alias(plan);
        if let Some(p) = plans.iter().find(|p| p["canonical_name"].as_str().map(norm_alias) == Some(n.clone())) {
            return Some(p);
        }
        let id = self.raw["alias_index"].as_array()?.iter().find(|a| {
            a["provider"].as_str() == Some(pv.as_str()) && a["alias_norm"].as_str() == Some(n.as_str())
        })?["plan_id"]
            .as_str()?
            .to_string();
        plans.iter().find(|p| p["id"].as_str() == Some(id.as_str()))
    }

    /// 目录里已存在的 (团队, 编号)。
    pub fn taken_numbers(&self) -> HashSet<(String, u32)> {
        let mut out = HashSet::new();
        for a in self.raw["assets"].as_array().into_iter().flatten() {
            if let Some(s) = a["short_name"].as_str() {
                if let Some((t, n)) = s.rsplit_once('-') {
                    if let Ok(n) = n.parse::<u32>() {
                        out.insert((t.to_string(), n));
                    }
                }
            }
        }
        out
    }
    pub fn known_ips(&self) -> HashSet<String> {
        let mut out = HashSet::new();
        for a in self.raw["assets"].as_array().into_iter().flatten() {
            if let Some(s) = a["asset_name"].as_str() {
                if let Some(ip) = s.rsplit(" · ").next() {
                    out.insert(ip.trim().to_string());
                }
            }
        }
        out
    }
}

/// 极简 CSV：逗号分隔，支持双引号包裹与 "" 转义；# 开头的行是注释。
pub fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let mut rows = vec![];
    for line in text.lines() {
        let l = line.trim_end_matches('\r');
        if l.trim().is_empty() || l.trim_start().starts_with('#') {
            continue;
        }
        let mut cells = vec![];
        let mut cur = String::new();
        let mut q = false;
        let mut it = l.chars().peekable();
        while let Some(c) = it.next() {
            match (c, q) {
                ('"', true) if it.peek() == Some(&'"') => {
                    cur.push('"');
                    it.next();
                }
                ('"', _) => q = !q,
                (',', false) => cells.push(std::mem::take(&mut cur).trim().to_string()),
                (c, _) => cur.push(c),
            }
        }
        cells.push(cur.trim().to_string());
        rows.push(cells);
    }
    rows
}

pub fn norm_team(t: &str) -> String {
    let t = t.trim();
    if t.chars().all(|c| c.is_ascii_digit()) && !t.is_empty() {
        format!("团队{t}")
    } else {
        t.to_string()
    }
}

/// 解析 + 静态校验。返回行（即使有错也尽量解析出来，便于展示）与问题清单。
pub fn load_rows(cfg: &Config, cat: &Catalog, csv_text: &str, csv_dir: &std::path::Path) -> (Vec<RowSpec>, Vec<Issue>) {
    let mut issues = vec![];
    let table = parse_csv(csv_text);
    let Some((head, body)) = table.split_first() else {
        issues.push(Issue { seq: 0, level: "error", msg: "CSV 是空的".into() });
        return (vec![], issues);
    };
    let head: Vec<String> = head.iter().map(|h| h.trim().to_ascii_lowercase()).collect();
    for need in ["provider", "ip", "ssh_port", "auth", "plan", "team", "number"] {
        if !head.iter().any(|h| h == need) {
            issues.push(Issue { seq: 0, level: "error", msg: format!("表头缺少必填列 {need}") });
        }
    }
    if issues.iter().any(|i| i.level == "error") {
        return (vec![], issues);
    }
    let taken = cat.taken_numbers();
    let known_ips = cat.known_ips();
    let mut rows = vec![];
    let mut seen_key = HashSet::new();
    let mut seen_num = HashSet::new();
    for (i, cells) in body.iter().enumerate() {
        let seq = i + 1;
        let get = |k: &str| -> String {
            head.iter().position(|h| h == k).and_then(|p| cells.get(p)).cloned().unwrap_or_default()
        };
        let mut bad = |m: String| issues.push(Issue { seq, level: "error", msg: m });
        let provider = get("provider");
        let ip = get("ip");
        if ip.parse::<Ipv4Addr>().is_err() {
            bad(format!("IP「{ip}」不是 IPv4 地址"));
        }
        let ssh_port: u16 = get("ssh_port").parse().unwrap_or_else(|_| {
            if get("ssh_port").is_empty() { 22 } else { 0 }
        });
        if ssh_port == 0 {
            bad(format!("ssh_port「{}」不是端口号", get("ssh_port")));
        }
        let auth_raw = get("auth");
        let (auth, password_file) = if auth_raw == "key" {
            ("key".to_string(), None)
        } else if let Some(f) = auth_raw.strip_prefix("password:").or_else(|| auth_raw.strip_prefix("password=")) {
            let p = std::path::Path::new(f.trim());
            let p = if p.is_absolute() { p.to_path_buf() } else { csv_dir.join(p) };
            if !p.is_file() {
                bad(format!("密码文件 {} 不存在", p.display()));
            }
            ("password".to_string(), Some(p.display().to_string()))
        } else {
            bad(format!("auth「{auth_raw}」只能是 key 或 password:<文件>"));
            ("key".to_string(), None)
        };
        let plan_in = get("plan");
        let plan = cat.resolve(&provider, &plan_in);
        let (plan_id, plan_name, nat, host_access, price, currency, bw) = match plan {
            Some(p) => (
                p["id"].as_str().unwrap_or("").to_string(),
                p["canonical_name"].as_str().unwrap_or("").to_string(),
                p["specs"]["nat"].as_bool().unwrap_or(false),
                p["host_access"].as_bool().unwrap_or(true),
                p["price"]["amount"].as_str().unwrap_or("0").to_string(),
                p["price"]["currency"].as_str().unwrap_or("USD").to_string(),
                p["specs"]["bandwidth_mbps"].as_u64(),
            ),
            None => {
                bad(format!("套餐「{plan_in}」不在目录里（按 {provider} 的 id / 规范名 / 别名都没匹配上）：请从目录选一个，或先登记别名"));
                (String::new(), String::new(), false, true, "0".into(), "USD".into(), None)
            }
        };
        if plan.is_some() && !host_access {
            bad(format!("套餐 {plan_name} 没有主机权限（代理产品），不走这条流水线"));
        }
        if let Some(p) = plan {
            if let Some(pp) = p["provider_name"].as_str() {
                if !pp.eq_ignore_ascii_case(&provider) {
                    bad(format!("套餐 {plan_name} 属于 {pp}，不是 {provider}"));
                }
            }
        }
        let ss_port_raw = get("ss_port");
        let ss_public_port: u16 = if ss_port_raw.is_empty() { cfg.ss.port } else { ss_port_raw.parse().unwrap_or(0) };
        if nat && ss_port_raw.is_empty() {
            bad("NAT 套餐必须填 ss_port（供应商映射到 8388 的公网端口）".into());
        }
        if ss_public_port == 0 {
            bad(format!("ss_port「{ss_port_raw}」不是端口号"));
        }
        let team = norm_team(&get("team"));
        let number: u32 = get("number").parse().unwrap_or(0);
        if number == 0 || team.is_empty() {
            bad("team / number 必填，number 是正整数".into());
        }
        let relay = Some(get("relay")).filter(|s| !s.is_empty());
        if let Some(r) = &relay {
            if cfg.relay(r).is_none() {
                bad(format!("线路机「{r}」不在 onboard.toml 的 [[relays]] 里"));
            }
        }
        let dd = match get("dd").as_str() {
            "" | "none" => "none".to_string(),
            "manual" => "manual".to_string(),
            o => {
                bad(format!("dd「{o}」只能是 none 或 manual"));
                "none".to_string()
            }
        };
        let key = (ip.clone(), ssh_port);
        if !seen_key.insert(key) {
            bad(format!("{ip}:{ssh_port} 在本批里重复"));
        }
        if !seen_num.insert((team.clone(), number)) {
            bad(format!("{team}-{number:02} 在本批里重复"));
        }
        if taken.contains(&(team.clone(), number)) {
            bad(format!("{team}-{number:02} 已是现有资产（目录里有），换一个编号"));
        }
        if !nat && known_ips.contains(&ip) {
            bad(format!("{ip} 已是现有资产的 IP"));
        }
        let provider_code = cfg.provider_code(&provider);
        let short_name = format!("{team}-{number:02}");
        let name = format!("{short_name} · {provider_code} · {ip}");
        let mac = Some(get("mac").to_ascii_lowercase()).filter(|s| !s.is_empty());
        rows.push(RowSpec {
            seq,
            provider,
            provider_code,
            ip,
            ssh_port,
            ss_port: cfg.ss.port,
            ss_public_port,
            nat,
            auth,
            password_file,
            plan_id,
            plan_name,
            price,
            currency,
            bandwidth_mbps: bw,
            team,
            number,
            name,
            short_name,
            relay,
            dd,
            mac,
            service_id: Some(get("service_id")).filter(|s| !s.is_empty()),
            note: Some(get("note")).filter(|s| !s.is_empty()),
        });
    }
    (rows, issues)
}

/// 计划哈希：规范化的行 + 影响执行的选项。CSV 改了（哪怕一个字）哈希就变。
pub fn plan_hash(rows: &[RowSpec], options: &Value) -> String {
    let mut h = Sha256::new();
    h.update(serde_json::to_vec(rows).unwrap());
    h.update(serde_json::to_vec(options).unwrap());
    hex::encode(h.finalize())[..16].to_string()
}

pub fn batch_id_for(csv_text: &str) -> String {
    let d = Sha256::digest(csv_text.as_bytes());
    format!("b-{}", &hex::encode(d)[..10])
}

/// 自动放置：没有指定线路机的行放到「现有入口 + 本批已分配」最少、且未满的线路机。
pub fn place(cfg: &Config, rows: &mut [RowSpec], existing: &BTreeMap<String, usize>) -> Vec<Issue> {
    let mut load: BTreeMap<String, usize> = cfg.relays.iter().map(|r| (r.name.clone(), *existing.get(&r.name).unwrap_or(&0))).collect();
    for r in rows.iter() {
        if let Some(n) = &r.relay {
            *load.entry(n.clone()).or_default() += 1;
        }
    }
    let mut issues = vec![];
    for r in rows.iter_mut().filter(|r| r.relay.is_none()) {
        let best = cfg
            .relays
            .iter()
            .filter(|c| load[&c.name] < c.max_nodes)
            .min_by_key(|c| load[&c.name]);
        match best {
            Some(c) => {
                r.relay = Some(c.name.clone());
                *load.get_mut(&c.name).unwrap() += 1;
            }
            None => issues.push(Issue { seq: r.seq, level: "error", msg: "所有线路机都满了（每台最多 25 个节点）：请加线路机".into() }),
        }
    }
    for c in &cfg.relays {
        if load[&c.name] > c.max_nodes {
            issues.push(Issue { seq: 0, level: "error", msg: format!("{} 名下将有 {} 个节点，超过上限 {}", c.name, load[&c.name], c.max_nodes) });
        }
    }
    issues
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_quotes_and_comments() {
        let t = "# c\na,b\n\"x,1\",\"he said \"\"hi\"\"\"\n";
        let r = parse_csv(t);
        assert_eq!(r, vec![vec!["a".to_string(), "b".into()], vec!["x,1".into(), "he said \"hi\"".into()]]);
    }

    #[test]
    fn alias_norm() {
        assert_eq!(norm_alias(" 基础版（美国住宅IP VPS主机） "), "基础版(美国住宅ip vps主机)");
    }
}
