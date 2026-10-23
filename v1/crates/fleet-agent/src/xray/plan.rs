//! 差异与顺序：当前（磁盘配置 + 运行态 tag）→ 候选。纯函数。
//!
//! 顺序（spec/edge.md §5.3 第 8 步）：
//!   1. 出站：新增 ado；变化 rmo+ado
//!   2. 入站：先 rmi 运行态多余的；变化的彼此没有端口冲突时同一入站 rmi、adi 相邻，
//!      有冲突（端口互换等）就先全部 rmi 再全部 adi；最后 adi 新增的
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
    let cand_i: Vec<&Value> = cand.inbounds.iter().collect();
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

    // 2. 入站
    let rmi = |t: &str| Step { op: Op::RemoveInbound(t.into()), undo: prev_i.get(t).map(|o| Op::AddInbound((*o).clone())), tag: t.into() };
    let adi = |v: &Value| Step { op: Op::AddInbound(v.clone()), undo: Some(Op::RemoveInbound(tag_of(v).into())), tag: tag_of(v).into() };
    // 2a. 先删运行态多余的（孤儿的内容、端口都未知，必须最先腾出来）。旧骨架的 api 入站不删：读不到旧配置（prev 按新骨架算）时
    // 正在运行的 Xray 可能还是旧骨架，经 API 删掉它等于把 API 自己删了
    let mut seen = HashSet::new();
    for t in rt.inbounds.iter().map(|s| s.as_str()).filter(|t| *t != API_TAG && !cand_i_tags.contains(t)) {
        if seen.insert(t) {
            steps.push(rmi(t));
        }
    }
    // 2b. 内容变化的（运行态有）要 rmi + adi，新增的只 adi
    let (changed, added): (Vec<&Value>, Vec<&Value>) = cand_i
        .iter()
        .copied()
        .filter(|v| !(rt_i.contains(tag_of(v)) && prev_i.get(tag_of(v)) == Some(v)))
        .partition(|v| rt_i.contains(tag_of(v)));
    // 变化的入站彼此没有端口冲突时，同一入站删、加相邻：每个入口只断开一步（整体替换时不必让第一个入口等 N 步）。
    // 某个入站的新端口正被另一个待替换的入站占着（端口互换），或另一个的旧内容未知（不知道它占着哪个端口），就先全部删再全部加
    let port_of = |v: &Value| v.get("port").and_then(Value::as_u64);
    let conflict = changed
        .iter()
        .any(|&v| changed.iter().any(|&u| tag_of(u) != tag_of(v) && prev_i.get(tag_of(u)).is_none_or(|&o| port_of(o) == port_of(v))));
    if conflict {
        steps.extend(changed.iter().map(|&v| rmi(tag_of(v))));
        steps.extend(changed.iter().copied().map(adi));
    } else {
        for &v in &changed {
            steps.push(rmi(tag_of(v)));
            steps.push(adi(v));
        }
    }
    // 2c. 新增的最后加：它们的端口在候选里唯一，多余的、变化的此时都已腾出旧端口
    steps.extend(added.into_iter().map(adi));

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

    /// 计划里的撤销步骤，反向排成一份计划
    fn undo_of(steps: &[Step]) -> Vec<Step> {
        steps.iter().rev().filter_map(|s| s.undo.clone().map(|u| Step { op: u, undo: None, tag: s.tag.clone() })).collect()
    }

    /// 从 `from` 的运行态出发模拟执行 `steps`，结果要和 `to` 一致
    fn assert_reaches(from: &Rendered, steps: &[Step], to: &Rendered) {
        let mut rt = rt_of(from);
        simulate(&mut rt, steps).unwrap();
        assert!(verify(&rt, to).is_ok(), "{rt:?}");
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
                "adrules (4 条)".to_string()
            ]
        );
        assert_reaches(&prev, &steps, &cand);
    }

    #[test]
    fn old_skeleton_api_inbound_never_removed() {
        // 读不到 xray.json（prev 按新骨架算），运行中的 Xray 还是带 api 入站的旧骨架
        let prev = skeleton(&ctx());
        let cand = render(&desired(vec![entry(1, 20001, true)]), &ctx()).unwrap();
        let mut rt = rt_of(&prev);
        rt.inbounds.push(API_TAG.into());
        let steps = plan(&rt, &prev, &cand);
        assert!(steps.iter().all(|s| s.op != Op::RemoveInbound(API_TAG.into())), "{:?}", kinds(&steps));
        assert!(kinds(&steps).iter().any(|k| k.starts_with("adi ")));
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
        assert!(rt.inbounds.is_empty());
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
        assert_reaches(&prev, &steps, &cand);
        // 只改出站：不碰入站
        let mut e = entry(1, 20001, true);
        e.outbound = fleet_proto::Outbound::Shadowsocks { host: "1.1.1.1".into(), port: 1, method: "aes-256-gcm".into(), password: "x".into() };
        let c2 = render(&desired(vec![e, entry(2, 20002, true)]), &ctx()).unwrap();
        let k2 = kinds(&plan(&rt_of(&prev), &prev, &c2));
        assert_eq!(k2.len(), 2);
        assert!(k2[0].starts_with("rmo") && k2[1].starts_with("ado"));
    }

    #[test]
    fn whole_replacement_interleaves_per_inbound() {
        // 整体替换（例如换 REALITY 私钥）且没有端口冲突：同一入站删、加相邻，每个入口只断开一步
        let es = || vec![entry(1, 20001, true), entry(2, 20002, true)];
        let prev = render(&desired(es()), &ctx()).unwrap();
        let mut d = desired(es());
        d.relay.reality_private_key = "NEWKEY".into();
        let cand = render(&d, &ctx()).unwrap();
        let steps = plan(&rt_of(&prev), &prev, &cand);
        let (a, b) = (tags::inbound(Uuid::from_u128(1)), tags::inbound(Uuid::from_u128(2)));
        assert_eq!(kinds(&steps), vec![format!("rmi {a}"), format!("adi {a}"), format!("rmi {b}"), format!("adi {b}")]);
        assert_reaches(&prev, &steps, &cand);
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
        let undo = undo_of(&steps);
        assert_reaches(&cand, &undo, &prev);
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
        assert_eq!(kinds(&steps), vec!["adrules (6 条)".to_string()]);
        assert_reaches(&prev, &steps, &a);
        // 只改域名列表（规则的 ruleTag/出站对没变）也必须替换
        let b = render(&desired(vec![with_policy(1, 20001, Allowlist, &["domain:a.com", "full:b.com"])]), &ctx()).unwrap();
        assert_eq!(kinds(&plan(&rt_of(&a), &a, &b)), vec!["adrules (6 条)".to_string()]);
        // 模式 allowlist → denylist
        let c = render(&desired(vec![with_policy(1, 20001, Denylist, &["domain:a.com"])]), &ctx()).unwrap();
        assert_eq!(kinds(&plan(&rt_of(&b), &b, &c)), vec!["adrules (5 条)".to_string()]);
        // 恢复 observe：规则表回到原样
        let steps = plan(&rt_of(&c), &c, &prev);
        assert_eq!(kinds(&steps), vec!["adrules (4 条)".to_string()]);
        assert_reaches(&c, &steps, &prev);
        // 没变化：空计划
        assert!(plan(&rt_of(&b), &b, &b).is_empty());
        // 回滚：undo 回到上一份规则表
        let steps = plan(&rt_of(&a), &a, &b);
        let undo = undo_of(&steps);
        assert_reaches(&b, &undo, &a);
    }

    #[test]
    fn direct_entry_is_hot_added_and_removed() {
        use super::super::render::tests::direct;
        let base = render(&desired(vec![entry(1, 20001, true)]), &ctx()).unwrap();
        let with = render(&desired(vec![entry(1, 20001, true), direct(2, 20002)]), &ctx()).unwrap();
        let id = Uuid::from_u128(2);
        // 新增：和节点入口一样热增（不重启），出站是入口自己的 out-<id>
        let steps = plan(&rt_of(&base), &base, &with);
        assert_eq!(
            kinds(&steps),
            vec![format!("ado {}", tags::outbound(id)), format!("adi {}", tags::inbound(id)), format!("adi probe-{}", id.simple()), "adrules (7 条)".to_string()]
        );
        assert_reaches(&base, &steps, &with);
        // 移除：规则先换、出站最后删，共享的 direct 不动
        let steps = plan(&rt_of(&with), &with, &base);
        let k = kinds(&steps);
        assert_eq!(k.last().unwrap(), &format!("rmo {}", tags::outbound(id)));
        let mut rt = rt_of(&with);
        simulate(&mut rt, &steps).unwrap();
        assert!(verify(&rt, &base).is_ok());
        assert!(rt.outbounds.contains(&"direct".to_string()));
        // 回滚：撤回全部步骤回到加之前
        let steps = plan(&rt_of(&base), &base, &with);
        let undo = undo_of(&steps);
        assert_reaches(&with, &undo, &base);
        // 停用：入站、出站、规则都摘掉
        let mut d = direct(2, 20002);
        d.enabled = false;
        let off = render(&desired(vec![entry(1, 20001, true), d]), &ctx()).unwrap();
        assert_eq!(off.full, base.full);
    }

    #[test]
    fn verify_detects_mismatch() {
        let r = render(&desired(vec![entry(1, 20001, true)]), &ctx()).unwrap();
        let mut rt = rt_of(&r);
        rt.rules.swap(0, 1);
        assert!(verify(&rt, &r).is_err());
    }
}
