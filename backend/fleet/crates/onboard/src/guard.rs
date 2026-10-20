//! diff 守卫（生产保护）：线路机 Xray 配置只允许「追加」本批的新对象；已有入口/转发/规则一个字节都不能动。
//! 编译候选配置时查一遍，落盘后读回再查一遍（两次都记进线路机批次报告）。

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct AppendDiff {
    pub new_inbounds: Vec<String>,
    pub new_outbounds: Vec<String>,
    pub new_rules: Vec<String>,
}

fn arr<'a>(v: &'a Value, k: &str) -> &'a [Value] {
    v.get(k).and_then(|x| x.as_array()).map(|a| a.as_slice()).unwrap_or(&[])
}
fn rules(v: &Value) -> &[Value] {
    v.get("routing").and_then(|r| r.get("rules")).and_then(|x| x.as_array()).map(|a| a.as_slice()).unwrap_or(&[])
}
fn tag(v: &Value) -> String {
    v.get("tag").and_then(|t| t.as_str()).unwrap_or("").to_string()
}

fn ver_le(a: &str, b: &str) -> bool {
    let p = |s: &str| s.split('.').map(|x| x.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    p(a) <= p(b)
}

/// before → after 必须是纯追加。返回新增对象的 tag。
pub fn append_only(before: &Value, after: &Value) -> Result<AppendDiff, String> {
    let (Some(bo), Some(ao)) = (before.as_object(), after.as_object()) else { return Err("配置不是 JSON 对象".into()) };
    let bk: BTreeSet<_> = bo.keys().collect();
    let ak: BTreeSet<_> = ao.keys().collect();
    if bk != ak {
        return Err(format!("顶层键变了：{bk:?} → {ak:?}"));
    }
    for k in bo.keys() {
        if k == "inbounds" || k == "outbounds" || k == "routing" {
            continue;
        }
        if bo[k] != ao[k] {
            return Err(format!("静态段 {k} 被改动"));
        }
    }
    // routing 除 rules 外不许变
    let (br, ar) = (&before["routing"], &after["routing"]);
    if let (Some(b), Some(a)) = (br.as_object(), ar.as_object()) {
        let bk: BTreeSet<_> = b.keys().filter(|k| *k != "rules").collect();
        let ak: BTreeSet<_> = a.keys().filter(|k| *k != "rules").collect();
        if bk != ak || bk.iter().any(|k| b[k.as_str()] != a[k.as_str()]) {
            return Err("routing 的非规则字段被改动".into());
        }
    } else if br != ar {
        return Err("routing 结构被改动".into());
    }
    let mut d = AppendDiff::default();
    // 入站
    let (bi, ai) = (arr(before, "inbounds"), arr(after, "inbounds"));
    if ai.len() < bi.len() || ai[..bi.len()] != *bi {
        return Err("已有入口被修改、删除或重排".into());
    }
    let mut tags: BTreeSet<String> = bi.iter().map(tag).collect();
    let mut ports: BTreeSet<u64> = bi.iter().filter_map(|i| i["port"].as_u64()).collect();
    for n in &ai[bi.len()..] {
        let t = tag(n);
        if t.is_empty() || !tags.insert(t.clone()) {
            return Err(format!("新入口 tag「{t}」为空或重复"));
        }
        let p = n["port"].as_u64().ok_or("新入口没有端口")?;
        if !ports.insert(p) {
            return Err(format!("新入口端口 {p} 与已有入口冲突"));
        }
        if n["protocol"] != "vless" || n["streamSettings"]["security"] != "reality" {
            return Err(format!("新入口 {t} 只能是 VLESS REALITY"));
        }
        let mcv = n["streamSettings"]["realitySettings"]["minClientVer"].as_str().unwrap_or("");
        if !mcv.is_empty() && !ver_le(mcv, "1.8.2") {
            return Err(format!("新入口 {t} 的 minClientVer {mcv} > 1.8.2（mihomo 客户端会连不上）"));
        }
        d.new_inbounds.push(t);
    }
    // 出站
    let (bo_, ao_) = (arr(before, "outbounds"), arr(after, "outbounds"));
    if ao_.len() < bo_.len() || ao_[..bo_.len()] != *bo_ {
        return Err("已有转发被修改、删除或重排".into());
    }
    let mut otags: BTreeSet<String> = bo_.iter().map(tag).collect();
    for n in &ao_[bo_.len()..] {
        let t = tag(n);
        if t.is_empty() || !otags.insert(t.clone()) {
            return Err(format!("新转发 tag「{t}」为空或重复"));
        }
        if n["protocol"] != "shadowsocks" || n.get("proxySettings").is_some() {
            return Err(format!("新转发 {t} 只能是 shadowsocks（不许 freedom / 链式代理）"));
        }
        d.new_outbounds.push(t);
    }
    // 规则
    let (brs, ars) = (rules(before), rules(after));
    if ars.len() < brs.len() || ars[..brs.len()] != *brs {
        return Err("已有路由规则被修改、删除或重排".into());
    }
    let mut rtags: BTreeSet<String> = brs.iter().filter_map(|r| r["ruleTag"].as_str().map(String::from)).collect();
    for r in &ars[brs.len()..] {
        let o = r.as_object().ok_or("规则不是对象")?;
        let allowed: BTreeSet<&str> = ["type", "ruleTag", "inboundTag", "outboundTag"].into_iter().collect();
        if let Some(k) = o.keys().find(|k| !allowed.contains(k.as_str())) {
            return Err(format!("新规则含不允许的字段 {k}（只允许 inboundTag → outboundTag）"));
        }
        let rt = r["ruleTag"].as_str().unwrap_or("").to_string();
        if rt.is_empty() || !rtags.insert(rt.clone()) {
            return Err(format!("新规则 ruleTag「{rt}」为空或重复"));
        }
        let ins = r["inboundTag"].as_array().ok_or("新规则缺 inboundTag")?;
        if ins.len() != 1 || !d.new_inbounds.iter().any(|t| Some(t.as_str()) == ins[0].as_str()) {
            return Err(format!("新规则 {rt} 只能匹配本批新增的一个入口"));
        }
        let ot = r["outboundTag"].as_str().unwrap_or("");
        if !d.new_outbounds.iter().any(|t| t == ot) {
            return Err(format!("新规则 {rt} 只能指向本批新增的转发（不许 direct / 已有转发）"));
        }
        d.new_rules.push(rt);
    }
    Ok(d)
}

/// before → after 必须恰好删掉 tags 指定的对象（补偿/放弃节点用），其余逐字节不变。
pub fn remove_only(before: &Value, after: &Value, inbound: &str, outbound: &str, rule: &str) -> Result<(), String> {
    let mut expect = before.clone();
    if let Some(a) = expect.get_mut("inbounds").and_then(|x| x.as_array_mut()) {
        a.retain(|i| tag(i) != inbound);
    }
    if let Some(a) = expect.get_mut("outbounds").and_then(|x| x.as_array_mut()) {
        a.retain(|o| tag(o) != outbound);
    }
    if let Some(a) = expect.get_mut("routing").and_then(|r| r.get_mut("rules")).and_then(|x| x.as_array_mut()) {
        a.retain(|r| r["ruleTag"].as_str() != Some(rule));
    }
    if &expect != after {
        return Err("删除之外还有别的改动".into());
    }
    Ok(())
}

/// 每个已有入口的指纹（决定分享链接的全部字段都在入口对象里：端口、UUID、私钥→公钥、shortId、SNI、flow）。
pub fn inbound_fingerprints(cfg: &Value) -> BTreeMap<String, String> {
    arr(cfg, "inbounds")
        .iter()
        .map(|i| (tag(i), hex::encode(Sha256::digest(serde_json::to_vec(i).unwrap()))[..16].to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base() -> Value {
        json!({"log":{"loglevel":"warning"},"api":{"tag":"api","services":["HandlerService","RoutingService"]},
          "inbounds":[{"tag":"api","port":62789,"listen":"127.0.0.1","protocol":"dokodemo-door"},
                      {"tag":"inbound-20001","port":20001,"protocol":"vless","streamSettings":{"security":"reality"}}],
          "outbounds":[{"tag":"direct","protocol":"freedom"},{"tag":"blocked","protocol":"blackhole"}],
          "routing":{"domainStrategy":"AsIs","rules":[{"type":"field","inboundTag":["api"],"outboundTag":"api"}]}})
    }
    fn add(mut c: Value, port: u64) -> Value {
        c["inbounds"].as_array_mut().unwrap().push(json!({"tag":format!("inbound-{port}"),"port":port,"protocol":"vless",
            "streamSettings":{"security":"reality","realitySettings":{"minClientVer":"1.8.2"}}}));
        c["outbounds"].as_array_mut().unwrap().push(json!({"tag":format!("ss-{port}"),"protocol":"shadowsocks","settings":{}}));
        c["routing"]["rules"].as_array_mut().unwrap().push(json!({"type":"field","ruleTag":format!("r-inbound-{port}"),"inboundTag":[format!("inbound-{port}")],"outboundTag":format!("ss-{port}")}));
        c
    }

    #[test]
    fn pure_append_ok() {
        let b = base();
        let a = add(add(b.clone(), 20002), 20003);
        let d = append_only(&b, &a).unwrap();
        assert_eq!(d.new_inbounds, vec!["inbound-20002", "inbound-20003"]);
        assert_eq!(d.new_rules.len(), 2);
    }
    #[test]
    fn modify_existing_rejected() {
        let b = base();
        let mut a = add(b.clone(), 20002);
        a["inbounds"][1]["port"] = json!(20009);
        assert!(append_only(&b, &a).unwrap_err().contains("已有入口"));
    }
    #[test]
    fn rule_to_direct_rejected() {
        let b = base();
        let mut a = add(b.clone(), 20002);
        a["routing"]["rules"][1]["outboundTag"] = json!("direct");
        assert!(append_only(&b, &a).unwrap_err().contains("direct"));
    }
    #[test]
    fn freedom_outbound_rejected() {
        let b = base();
        let mut a = add(b.clone(), 20002);
        a["outbounds"][2]["protocol"] = json!("freedom");
        assert!(append_only(&b, &a).is_err());
    }
    #[test]
    fn balancer_field_rejected() {
        let b = base();
        let mut a = add(b.clone(), 20002);
        a["routing"]["rules"][1]["balancerTag"] = json!("x");
        assert!(append_only(&b, &a).unwrap_err().contains("balancerTag"));
    }
    #[test]
    fn port_clash_rejected() {
        let b = base();
        let mut a = add(b.clone(), 20002);
        a["inbounds"][2]["port"] = json!(20001);
        assert!(append_only(&b, &a).unwrap_err().contains("20001"));
    }
    #[test]
    fn static_section_rejected() {
        let b = base();
        let mut a = add(b.clone(), 20002);
        a["log"]["loglevel"] = json!("debug");
        assert!(append_only(&b, &a).is_err());
    }
    #[test]
    fn remove_exact() {
        let b = add(base(), 20002);
        let a = base();
        remove_only(&b, &a, "inbound-20002", "ss-20002", "r-inbound-20002").unwrap();
        assert!(remove_only(&b, &base(), "inbound-20002", "ss-20002", "nope").is_err());
    }
}
