//! 差异与顺序：当前（磁盘配置 + 运行态 tag）→ 候选。纯函数。
//!
//! 顺序（spec/edge.md §5.3 第 8 步）：
//!   1. 出站：新增 ado；变化 rmo+ado
//!   2. 入站：先全部 rmi（删除的 + 变化的），再全部 adi（变化的 + 新增的）——端口互换也不冲突
//!   3. 规则：不一致时整表替换 adrules（原子，保证位置）
//!   4. 出站删除 rmo（此时规则已不再引用）

use super::cli::{Op, Runtime};
use super::render::{is_static_outbound, rule_tag_of, tag_of, Rendered, API_TAG};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub op: Op,
    /// 回滚时执行的反向操作；`None` = 无法回滚（旧对象内容未知）
    pub undo: Option<Op>,
    pub tag: String,
}

fn by_tag(v: &[Value]) -> HashMap<&str, &Value> {
    v.iter().map(|x| (tag_of(x), x)).collect()
}

fn rule_pairs(rules: &[Value]) -> Vec<(String, String)> {
    rules
        .iter()
        .map(|r| (rule_tag_of(r).to_string(), r.get("outboundTag").and_then(|t| t.as_str()).unwrap_or("").to_string()))
        .collect()
}

/// `prev` = 磁盘上上一份配置（运行态对象的内容以它为准）；`rt` = 运行态存在哪些 tag。
/// 运行态有而候选没有的动态对象一律删除（孤儿清理）；候选有而运行态没有的一律新增。
pub fn plan(rt: &Runtime, prev: &Rendered, cand: &Rendered) -> Vec<Step> {
    let mut steps = Vec::new();
    let prev_o = by_tag(&prev.outbounds);
    let prev_i = by_tag(&prev.inbounds);
    let rt_o: HashSet<&str> = rt.outbounds.iter().map(|s| s.as_str()).collect();
    let rt_i: HashSet<&str> = rt.inbounds.iter().map(|s| s.as_str()).collect();

    let cand_o: Vec<&Value> = cand.outbounds.iter().filter(|v| !is_static_outbound(tag_of(v))).collect();
    let cand_i: Vec<&Value> = cand.inbounds.iter().filter(|v| tag_of(v) != API_TAG).collect();
    let cand_o_tags: HashSet<&str> = cand_o.iter().map(|v| tag_of(v)).collect();
    let cand_i_tags: HashSet<&str> = cand_i.iter().map(|v| tag_of(v)).collect();

    // 1. 出站新增 / 变化
    for v in &cand_o {
        let t = tag_of(v);
        if rt_o.contains(t) {
            if prev_o.get(t) == Some(v) {
                continue;
            }
            steps.push(Step {
                op: Op::RemoveOutbound(t.into()),
                undo: prev_o.get(t).map(|o| Op::AddOutbound((*o).clone())),
                tag: t.into(),
            });
        }
        steps.push(Step { op: Op::AddOutbound((*v).clone()), undo: Some(Op::RemoveOutbound(t.into())), tag: t.into() });
    }

    // 2a. 入站先删：运行态多余的 + 内容变化的
    let mut inbound_adds: Vec<&Value> = Vec::new();
    let mut seen = HashSet::new();
    for t in rt.inbounds.iter().map(|s| s.as_str()).filter(|t| *t != API_TAG && !cand_i_tags.contains(t)) {
        if seen.insert(t) {
            steps.push(Step {
                op: Op::RemoveInbound(t.into()),
                undo: prev_i.get(t).map(|o| Op::AddInbound((*o).clone())),
                tag: t.into(),
            });
        }
    }
    for v in &cand_i {
        let t = tag_of(v);
        if rt_i.contains(t) && prev_i.get(t) == Some(v) {
            continue;
        }
        if rt_i.contains(t) {
            steps.push(Step {
                op: Op::RemoveInbound(t.into()),
                undo: prev_i.get(t).map(|o| Op::AddInbound((*o).clone())),
                tag: t.into(),
            });
        }
        inbound_adds.push(v);
    }
    // 2b. 再加
    for v in inbound_adds {
        let t = tag_of(v);
        steps.push(Step { op: Op::AddInbound((*v).clone()), undo: Some(Op::RemoveInbound(t.into())), tag: t.into() });
    }

    // 3. 规则
    if cand.rules != prev.rules || rule_pairs(&cand.rules) != rt.rules {
        steps.push(Step { op: Op::SetRules(cand.rules.clone()), undo: Some(Op::SetRules(prev.rules.clone())), tag: "rules".into() });
    }

    // 4. 出站删除
    for t in rt.outbounds.iter().map(|s| s.as_str()) {
        if is_static_outbound(t) || cand_o_tags.contains(t) {
            continue;
        }
        steps.push(Step { op: Op::RemoveOutbound(t.into()), undo: prev_o.get(t).map(|o| Op::AddOutbound((*o).clone())), tag: t.into() });
    }
    steps
}

/// 运行态与候选是否一致（tag 集合 + 规则的 (ruleTag, 出站) 序列）。返回不一致的描述。
pub fn verify(rt: &Runtime, cand: &Rendered) -> Result<(), String> {
    let set = |v: &[Value]| -> Vec<String> {
        let mut s: Vec<String> = v.iter().map(|x| tag_of(x).to_string()).collect();
        s.sort();
        s
    };
    let sorted = |v: &[String]| {
        let mut s = v.to_vec();
        s.sort();
        s
    };
    if sorted(&rt.inbounds) != set(&cand.inbounds) {
        return Err(format!("入站不一致：运行态 {:?}，期望 {:?}", sorted(&rt.inbounds), set(&cand.inbounds)));
    }
    if sorted(&rt.outbounds) != set(&cand.outbounds) {
        return Err(format!("出站不一致：运行态 {:?}，期望 {:?}", sorted(&rt.outbounds), set(&cand.outbounds)));
    }
    if rt.rules != rule_pairs(&cand.rules) {
        return Err(format!("规则不一致：运行态 {:?}，期望 {:?}", rt.rules, rule_pairs(&cand.rules)));
    }
    Ok(())
}

/// 把运行态的 Runtime 按步骤"模拟执行"，用于测试 plan 的顺序是否自洽
#[cfg(test)]
pub fn simulate(rt: &mut Runtime, steps: &[Step]) -> Result<(), String> {
    for s in steps {
        match &s.op {
            Op::AddOutbound(v) => {
                if rt.outbounds.iter().any(|t| t == tag_of(v)) {
                    return Err(format!("出站已存在 {}", tag_of(v)));
                }
                rt.outbounds.push(tag_of(v).into());
            }
            Op::RemoveOutbound(t) => {
                let n = rt.outbounds.len();
                rt.outbounds.retain(|x| x != t);
                if n == rt.outbounds.len() {
                    return Err(format!("出站不存在 {t}"));
                }
            }
            Op::AddInbound(v) => {
                if rt.inbounds.iter().any(|t| t == tag_of(v)) {
                    return Err(format!("入站已存在 {}", tag_of(v)));
                }
                rt.inbounds.push(tag_of(v).into());
            }
            Op::RemoveInbound(t) => {
                let n = rt.inbounds.len();
                rt.inbounds.retain(|x| x != t);
                if n == rt.inbounds.len() {
                    return Err(format!("入站不存在 {t}"));
                }
            }
            Op::SetRules(r) => rt.rules = rule_pairs(r),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::render::tests::{ctx, desired, entry};
    use super::super::render::{render, skeleton};
    use super::*;
    use fleet_proto::tags;
    use uuid::Uuid;

    fn rt_of(r: &Rendered) -> Runtime {
        Runtime {
            inbounds: r.inbounds.iter().map(|v| tag_of(v).to_string()).collect(),
            outbounds: r.outbounds.iter().map(|v| tag_of(v).to_string()).collect(),
            rules: rule_pairs(&r.rules),
        }
    }

    fn kinds(steps: &[Step]) -> Vec<String> {
        steps.iter().map(|s| s.op.describe()).collect()
    }

    #[test]
    fn no_change_is_empty() {
        let r = render(&desired(vec![entry(1, 20001, true)]), &ctx()).unwrap();
        assert!(plan(&rt_of(&r), &r, &r).is_empty());
        assert!(verify(&rt_of(&r), &r).is_ok());
    }

    #[test]
    fn add_entry_order() {
        let prev = skeleton(&ctx());
        let cand = render(&desired(vec![entry(1, 20001, true)]), &ctx()).unwrap();
        let steps = plan(&rt_of(&prev), &prev, &cand);
        let id = Uuid::from_u128(1);
        assert_eq!(
            kinds(&steps),
            vec![
                format!("ado {}", tags::outbound(id)),
                format!("adi {}", tags::inbound(id)),
                format!("adi probe-{}", id.simple()),
                "adrules (5 条)".to_string()
            ]
        );
        let mut rt = rt_of(&prev);
        simulate(&mut rt, &steps).unwrap();
        assert!(verify(&rt, &cand).is_ok());
    }

    #[test]
    fn remove_entry_leaves_nothing() {
        let prev = render(&desired(vec![entry(1, 20001, true), entry(2, 20002, true)]), &ctx()).unwrap();
        let cand = skeleton(&ctx());
        let steps = plan(&rt_of(&prev), &prev, &cand);
        let k = kinds(&steps);
        // 规则替换必须在 rmo 之前
        let ri = k.iter().position(|x| x.starts_with("adrules")).unwrap();
        assert!(k.iter().enumerate().filter(|(_, x)| x.starts_with("rmo")).all(|(i, _)| i > ri));
        let mut rt = rt_of(&prev);
        simulate(&mut rt, &steps).unwrap();
        assert!(verify(&rt, &cand).is_ok());
        assert_eq!(rt.inbounds, vec!["api"]);
        assert_eq!(rt.outbounds, vec!["deny", "direct", "blocked"]);
    }

    #[test]
    fn changed_entry_replaced_and_port_swap() {
        let prev = render(&desired(vec![entry(1, 20001, true), entry(2, 20002, true)]), &ctx()).unwrap();
        // 两个入口端口互换：两者都要 rmi+adi，且所有 rmi 在所有 adi 之前
        let cand = render(&desired(vec![entry(1, 20002, true), entry(2, 20001, true)]), &ctx()).unwrap();
        let steps = plan(&rt_of(&prev), &prev, &cand);
        let k = kinds(&steps);
        let last_rmi = k.iter().rposition(|x| x.starts_with("rmi")).unwrap();
        let first_adi = k.iter().position(|x| x.starts_with("adi")).unwrap();
        assert!(last_rmi < first_adi, "{k:?}");
        let mut rt = rt_of(&prev);
        simulate(&mut rt, &steps).unwrap();
        assert!(verify(&rt, &cand).is_ok());
        // 只改出站：不碰入站
        let mut e = entry(1, 20001, true);
        e.outbound = fleet_proto::Outbound::Shadowsocks { host: "1.1.1.1".into(), port: 1, method: "aes-256-gcm".into(), password: "x".into() };
        let c2 = render(&desired(vec![e, entry(2, 20002, true)]), &ctx()).unwrap();
        let k2 = kinds(&plan(&rt_of(&prev), &prev, &c2));
        assert_eq!(k2.len(), 2);
        assert!(k2[0].starts_with("rmo") && k2[1].starts_with("ado"));
    }

    #[test]
    fn heals_runtime_drift_and_orphans() {
        // 磁盘配置与候选相同，但运行态缺入站、多出孤儿
        let cand = render(&desired(vec![entry(1, 20001, true)]), &ctx()).unwrap();
        let mut rt = rt_of(&cand);
        let id = Uuid::from_u128(1);
        rt.inbounds.retain(|t| *t != tags::inbound(id));
        rt.inbounds.push("in-orphan".into());
        rt.outbounds.push("out-orphan".into());
        rt.rules.push(("rule-orphan".into(), "out-orphan".into()));
        let steps = plan(&rt, &cand, &cand);
        let mut sim = rt.clone();
        simulate(&mut sim, &steps).unwrap();
        assert!(verify(&sim, &cand).is_ok(), "{steps:?}");
        // 孤儿内容未知 → 无法回滚
        assert!(steps.iter().any(|s| s.tag == "in-orphan" && s.undo.is_none()));
    }

    #[test]
    fn undo_restores_previous() {
        let prev = render(&desired(vec![entry(1, 20001, true)]), &ctx()).unwrap();
        let cand = skeleton(&ctx());
        let steps = plan(&rt_of(&prev), &prev, &cand);
        // 反向执行全部 undo 应回到 prev
        let undo: Vec<Step> = steps.iter().rev().filter_map(|s| s.undo.clone().map(|u| Step { op: u, undo: None, tag: s.tag.clone() })).collect();
        let mut rt = rt_of(&cand);
        simulate(&mut rt, &undo).unwrap();
        assert!(verify(&rt, &prev).is_ok(), "{rt:?}");
    }

    fn with_policy(n: u128, port: u16, mode: fleet_proto::PolicyMode, domains: &[&str]) -> fleet_proto::DesiredEntry {
        let mut e = entry(n, port, true);
        e.policy = Some(fleet_proto::EntryPolicy { mode, domains: domains.iter().map(|d| d.to_string()).collect() });
        e
    }

    #[test]
    fn policy_changes_are_detected_as_rule_replacement() {
        use fleet_proto::PolicyMode::*;
        let prev = render(&desired(vec![entry(1, 20001, true)]), &ctx()).unwrap();
        // observe → allowlist：只换规则表，入站出站一个不动
        let a = render(&desired(vec![with_policy(1, 20001, Allowlist, &["domain:a.com"])]), &ctx()).unwrap();
        let steps = plan(&rt_of(&prev), &prev, &a);
        assert_eq!(kinds(&steps), vec!["adrules (7 条)".to_string()]);
        let mut rt = rt_of(&prev);
        simulate(&mut rt, &steps).unwrap();
        assert!(verify(&rt, &a).is_ok());
        // 只改域名列表（规则的 ruleTag/出站对没变）也必须替换
        let b = render(&desired(vec![with_policy(1, 20001, Allowlist, &["domain:a.com", "full:b.com"])]), &ctx()).unwrap();
        assert_eq!(kinds(&plan(&rt_of(&a), &a, &b)), vec!["adrules (7 条)".to_string()]);
        // 模式 allowlist → denylist
        let c = render(&desired(vec![with_policy(1, 20001, Denylist, &["domain:a.com"])]), &ctx()).unwrap();
        assert_eq!(kinds(&plan(&rt_of(&b), &b, &c)), vec!["adrules (6 条)".to_string()]);
        // 恢复 observe：规则表回到原样
        let steps = plan(&rt_of(&c), &c, &prev);
        assert_eq!(kinds(&steps), vec!["adrules (5 条)".to_string()]);
        let mut rt = rt_of(&c);
        simulate(&mut rt, &steps).unwrap();
        assert!(verify(&rt, &prev).is_ok());
        // 没变化：空计划
        assert!(plan(&rt_of(&b), &b, &b).is_empty());
        // 回滚：undo 回到上一份规则表
        let steps = plan(&rt_of(&a), &a, &b);
        let undo: Vec<Step> = steps.iter().rev().filter_map(|s| s.undo.clone().map(|u| Step { op: u, undo: None, tag: s.tag.clone() })).collect();
        let mut rt = rt_of(&b);
        simulate(&mut rt, &undo).unwrap();
        assert!(verify(&rt, &a).is_ok());
    }

    #[test]
    fn verify_detects_mismatch() {
        let r = render(&desired(vec![entry(1, 20001, true)]), &ctx()).unwrap();
        let mut rt = rt_of(&r);
        rt.rules.swap(0, 1);
        assert!(verify(&rt, &r).is_err());
    }
}
