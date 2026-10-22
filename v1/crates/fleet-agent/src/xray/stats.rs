//! 流量增量：Xray 计数器是"开机以来累计"，Xray 重启会清零。
//! 状态（core_id + 上次读数）落盘，Agent 重启不丢也不重复计。

use fleet_proto::{tags, EntrySample};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StatsState {
    /// 有过基线（首次运行只记基线、增量为 0）
    pub initialized: bool,
    /// Xray 进程"纪元"标识（boot_id:pid:starttime）；变了 = Xray 重启过
    pub core_id: String,
    pub prev: HashMap<String, u64>,
}

/// 入口 → (上行, 下行) 字节：离线队列淘汰掉的报告里的流量，由下一份报告补报
pub type Carry = HashMap<Uuid, (u64, u64)>;

/// stats.json 的完整内容：流量基线（stats_task 算，经 `Buffers::stats_commit` 交给上报器）+ 结转表（上报器维护）。
/// 只有上报器写这个文件；stats_task 启动时按 `StatsState` 读，多出的 carry 键被忽略
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct StatsFile {
    #[serde(flatten)]
    pub base: StatsState,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub carry: Carry,
}

/// 流量样本累加进结转表
pub fn add_carry(carry: &mut Carry, samples: &[EntrySample]) {
    for s in samples {
        let c = carry.entry(s.entry_id).or_default();
        c.0 = c.0.saturating_add(s.up_bytes);
        c.1 = c.1.saturating_add(s.down_bytes);
    }
}

/// 结转表 → 补报样本。样本本来就是"一个间隔内的增量"、中控按求和汇总，多一条同入口的样本只是把这段流量记在补报时刻；
/// 连接数记 0（中控对连接数取 max，不受影响）
pub fn carry_samples(carry: &Carry, ts_ms: i64) -> Vec<EntrySample> {
    carry.iter().map(|(&entry_id, &(up_bytes, down_bytes))| EntrySample { entry_id, ts_ms, up_bytes, down_bytes, conns: 0 }).collect()
}

/// 计数器名 `user>>>e-<入口>>>>traffic>>>uplink|downlink` 里的入口
fn counter_entry(name: &str) -> Option<Uuid> {
    let email = name.strip_prefix("user>>>")?.split(">>>").next()?;
    Uuid::try_parse(email.strip_prefix("e-")?).ok()
}

/// 只留 `ids` 里入口的计数器，再交给 `compute`。stats_task 传「本轮启用 ∪ 上轮启用」：
/// - 刚停用 / 删除的入口上轮还启用：这一轮照样求差、出样本，停用前最后一段流量不丢
/// - 热添加的入口：Xray 里已有计数器、applied 还没换上时不建基线，等它进了启用列表，首批字节按「新出现的计数器」整个计入
/// - 两轮都不启用的：不参与计算，基线原样留着（同纪元内加回来时按原基线求差）
pub fn entry_counters(list: Vec<(String, u64)>, ids: &HashSet<Uuid>) -> HashMap<String, u64> {
    list.into_iter().filter(|(k, _)| counter_entry(k).is_some_and(|id| ids.contains(&id))).collect()
}

/// 计算每个计数器的增量并更新状态。
/// - 首次（无基线）：只记基线，增量 0
/// - 纪元变了（Xray 重启）：计数从 0 重来，增量 = 当前值
/// - 同纪元回退：视为被重置，增量 = 当前值
/// - 同纪元新出现的计数器：增量 = 当前值
/// - 否则：当前值 − 上次值
///
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

/// Xray 进程纪元标识。pid 或启动时间有一个取不到就返回 None：
/// 调用方必须跳过本轮 `compute`（保留上一轮的 core_id 和基线），否则会被误判成"Xray 重启"而把累计值全量当增量。
pub fn core_id_of(boot: &str, pid: Option<u32>, start_ticks: Option<u64>) -> Option<String> {
    Some(format!("{boot}:{}:{}", pid?, start_ticks?))
}

/// `compute` 的守卫版：纪元标识未知（None）时什么都不动、返回 None，由调用方跳过本轮。
pub fn compute_if_known(st: &mut StatsState, core_id: Option<&str>, cur: &HashMap<String, u64>) -> Option<HashMap<String, u64>> {
    Some(compute(st, core_id?, cur))
}

/// 把增量按入口汇总成 EntrySample（只为给定的入口输出：本轮启用的与上轮启用、这轮停用的；零增量也输出以保持连续性）
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
    fn unknown_pid_or_start_yields_no_core_id() {
        assert_eq!(core_id_of("b", Some(7), Some(99)).as_deref(), Some("b:7:99"));
        assert_eq!(core_id_of("b", None, Some(99)), None);
        assert_eq!(core_id_of("b", Some(7), None), None);
    }

    /// 回归：pid 偶尔取不到时，调用方跳过 compute，基线和 core_id 原样保留，
    /// 下一轮拿到真 pid 后只算真实增量（旧实现会按"重启"把累计值整个再算一遍）。
    #[test]
    fn skipped_round_keeps_baseline_no_double_count() {
        let boot = "b";
        let mut st = StatsState::default();
        let id = core_id_of(boot, Some(7), Some(99)).unwrap();
        compute(&mut st, &id, &m(&[("a", 1000)]));
        let d = compute(&mut st, &id, &m(&[("a", 1040)]));
        assert_eq!(d["a"], 40);
        // 本轮取不到 pid：跳过，状态一个字节都不变
        let before = st.clone();
        assert!(compute_if_known(&mut st, core_id_of(boot, None, None).as_deref(), &m(&[("a", 1070)])).is_none());
        assert_eq!(st, before);
        // 下一轮拿到同一个 pid：增量 = 1090 − 1040
        let d = compute_if_known(&mut st, Some(&id), &m(&[("a", 1090)])).unwrap();
        assert_eq!(d["a"], 50);
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

    /// 回归：入口刚停用时最后一段流量照样算出来；热添加的入口在 applied 换上之前不建基线，首批字节不被吞掉
    #[test]
    fn disabled_and_hot_added_entries_keep_bytes() {
        let (a, b) = (Uuid::from_u128(1), Uuid::from_u128(2));
        let up = |id: Uuid| format!("user>>>{}>>>traffic>>>uplink", tags::email(id));
        let list = |va: u64, vb: u64| vec![(up(a), va), (up(b), vb), ("user>>>probe>>>traffic>>>uplink".to_string(), 9)];
        let ids = |v: &[Uuid]| v.iter().copied().collect::<HashSet<_>>();
        let mut st = StatsState::default();
        compute(&mut st, "c1", &entry_counters(list(100, 0), &ids(&[a])));
        // b 已加进 Xray、有了 40 字节，但 applied 还没换上：不建基线
        let d = compute(&mut st, "c1", &entry_counters(list(130, 40), &ids(&[a])));
        assert_eq!((d.get(&up(a)), d.get(&up(b))), (Some(&30), None));
        assert!(!st.prev.contains_key(&up(b)) && st.prev.len() == 1);
        // 下一轮 b 启用、a 刚停用（上轮还启用）：b 的 40 + 10 整个计入，a 停用前的 5 字节照样算出来
        let d = compute(&mut st, "c1", &entry_counters(list(135, 50), &ids(&[a, b])));
        let s = to_samples(&d, &[(b, 1), (a, 0)], 1000);
        assert_eq!(s.iter().map(|s| (s.entry_id, s.up_bytes)).collect::<Vec<_>>(), [(b, 50), (a, 5)]);
        // 之后 a 两轮都不启用：不参与计算，基线留着，加回来时按原基线求差
        compute(&mut st, "c1", &entry_counters(list(135, 60), &ids(&[b])));
        let d = compute(&mut st, "c1", &entry_counters(list(137, 60), &ids(&[a, b])));
        assert_eq!(d[&up(a)], 2);
    }
}
