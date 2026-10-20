//! 流量增量：Xray 计数器是"开机以来累计"，Xray 重启会清零。
//! 状态（core_id + 上次读数）落盘，Agent 重启不丢也不重复计。

use fleet_proto::{tags, EntrySample};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StatsState {
    /// 有过基线（首次运行只记基线、增量为 0）
    pub initialized: bool,
    /// Xray 进程"纪元"标识（boot_id:pid:starttime）；变了 = Xray 重启过
    pub core_id: String,
    pub prev: HashMap<String, u64>,
}

/// 计算每个计数器的增量并更新状态。
/// - 首次（无基线）：只记基线，增量 0
/// - 纪元变了（Xray 重启）：计数从 0 重来，增量 = 当前值
/// - 同纪元回退：视为被重置，增量 = 当前值
/// - 同纪元新出现的计数器：增量 = 当前值
/// - 否则：当前值 − 上次值
/// 状态里的 prev 是"合并"而不是替换：暂时消失的计数器重新出现时仍按原基线求差。
pub fn compute(st: &mut StatsState, core_id: &str, cur: &HashMap<String, u64>) -> HashMap<String, u64> {
    let mut out = HashMap::new();
    let epoch_changed = st.core_id != core_id;
    if epoch_changed {
        // 新纪元：旧纪元里有、现在没有的计数器不会再回来，清掉基线避免状态无限增长。
        // （同纪元内不清：入口被移除后计数器仍留在 Xray 里，再加回来时必须按原基线求差）
        st.prev.retain(|k, _| cur.contains_key(k));
    }
    for (name, &v) in cur {
        let d = if !st.initialized {
            0
        } else if epoch_changed {
            v
        } else {
            match st.prev.get(name) {
                None => v,
                Some(&p) if v < p => v,
                Some(&p) => v - p,
            }
        };
        out.insert(name.clone(), d);
        st.prev.insert(name.clone(), v);
    }
    st.initialized = true;
    st.core_id = core_id.to_string();
    out
}

/// 把增量按入口汇总成 EntrySample（只为已知的启用入口输出，零增量也输出以保持连续性）
pub fn to_samples(deltas: &HashMap<String, u64>, entries: &[(Uuid, u32)], ts_ms: i64) -> Vec<EntrySample> {
    entries
        .iter()
        .map(|(id, conns)| {
            let email = tags::email(*id);
            let g = |dir: &str| deltas.get(&format!("user>>>{email}>>>traffic>>>{dir}")).copied().unwrap_or(0);
            EntrySample { entry_id: *id, ts_ms, up_bytes: g("uplink"), down_bytes: g("downlink"), conns: *conns }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(v: &[(&str, u64)]) -> HashMap<String, u64> {
        v.iter().map(|(k, x)| (k.to_string(), *x)).collect()
    }

    #[test]
    fn baseline_then_growth() {
        let mut st = StatsState::default();
        let d = compute(&mut st, "c1", &m(&[("a", 100)]));
        assert_eq!(d["a"], 0); // 首次只记基线
        let d = compute(&mut st, "c1", &m(&[("a", 130)]));
        assert_eq!(d["a"], 30);
        let d = compute(&mut st, "c1", &m(&[("a", 130)]));
        assert_eq!(d["a"], 0);
    }

    #[test]
    fn counter_reset_same_epoch() {
        let mut st = StatsState::default();
        compute(&mut st, "c1", &m(&[("a", 500)]));
        let d = compute(&mut st, "c1", &m(&[("a", 20)]));
        assert_eq!(d["a"], 20);
        let d = compute(&mut st, "c1", &m(&[("a", 25)]));
        assert_eq!(d["a"], 5);
    }

    #[test]
    fn xray_restart_new_epoch_counts_from_zero() {
        let mut st = StatsState::default();
        compute(&mut st, "c1", &m(&[("a", 500)]));
        // 重启后增长到 900（比旧值还大）：纪元变了，增量 = 当前值，而不是 400
        let d = compute(&mut st, "c2", &m(&[("a", 900)]));
        assert_eq!(d["a"], 900);
        let d = compute(&mut st, "c2", &m(&[("a", 950)]));
        assert_eq!(d["a"], 50);
    }

    #[test]
    fn new_counter_in_running_epoch_and_reappearing() {
        let mut st = StatsState::default();
        compute(&mut st, "c1", &m(&[("a", 10)]));
        // 新入口出现
        let d = compute(&mut st, "c1", &m(&[("a", 10), ("b", 7)]));
        assert_eq!(d["b"], 7);
        // b 暂时消失再出现：仍按原基线求差
        compute(&mut st, "c1", &m(&[("a", 10)]));
        let d = compute(&mut st, "c1", &m(&[("a", 10), ("b", 9)]));
        assert_eq!(d["b"], 2);
    }

    #[test]
    fn state_survives_agent_restart() {
        let mut st = StatsState::default();
        compute(&mut st, "c1", &m(&[("a", 100)]));
        let json = serde_json::to_string(&st).unwrap();
        let mut st2: StatsState = serde_json::from_str(&json).unwrap();
        let d = compute(&mut st2, "c1", &m(&[("a", 160)]));
        assert_eq!(d["a"], 60);
    }

    #[test]
    fn samples_and_prune() {
        let id = Uuid::from_u128(5);
        let e = tags::email(id);
        let up = format!("user>>>{e}>>>traffic>>>uplink");
        let down = format!("user>>>{e}>>>traffic>>>downlink");
        let deltas = m(&[(&up, 11), (&down, 22)]);
        let s = to_samples(&deltas, &[(id, 3), (Uuid::from_u128(6), 0)], 1000);
        assert_eq!((s[0].up_bytes, s[0].down_bytes, s[0].conns), (11, 22, 3));
        assert_eq!((s[1].up_bytes, s[1].down_bytes), (0, 0));
        // 入口被移除后计数器仍留在 Xray 里：加回同一入口时按原基线求差，不能把累计值再算一遍
        let mut st = StatsState::default();
        compute(&mut st, "c1", &m(&[(&up, 100)]));
        compute(&mut st, "c1", &m(&[(&up, 150)]));
        let d = compute(&mut st, "c1", &m(&[(&up, 160)]));
        assert_eq!(d[&up], 10);
        // 新纪元后旧计数器的基线被清掉
        compute(&mut st, "c2", &m(&[("other", 5)]));
        assert!(!st.prev.contains_key(&up));
    }
}
