//! 实时指标推送：在用机器视图里随上报和时间变化的字段（主机读数、最近上报、agent 状态、健康、探测、流量、到期天数、总览），
//! 和上一次推送逐字段比对，只把变了的字段合成一条 `metrics` 消息广播给实时流；前端直接写进查询缓存，不再整表重取。
//!
//! 有新上报时最多每 3 s 比对一次；没有上报也每 30 s 比对一次（离线判定、到期天数、维护窗口随时间变）。没有订阅者时不做。
//! 视图走 `views::live` 的同一份缓存：REST 拿到的快照不会比上一次推送的基线更旧，前端在快照上叠加增量即可。

use crate::state::{AppState, Event};
use crate::views::{self, EntryView, MachineView, Probe};
use serde::Serialize;
use serde_json::value::{to_raw_value, RawValue};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use time::OffsetDateTime;
use uuid::Uuid;

const EVERY: Duration = Duration::from_secs(3);
const SWEEP: Duration = Duration::from_secs(30);

/// 机器级字段（消息里的键名）。host、tcp/egress 用定长数组（每条上报都变，省字节），其余与 REST 同形
const M_FIELDS: [&str; 8] = ["host", "seen", "agent", "health", "maintenance", "due_days", "removed_traffic", "quota"];
const E_FIELDS: [&str; 3] = ["tcp", "egress", "traffic"];

type Fields = Vec<Box<RawValue>>;
type Patch = BTreeMap<&'static str, Box<RawValue>>;

/// 上一次推送出去的各字段（序列化后的 JSON），比对用
#[derive(Default)]
pub struct Published {
    machines: HashMap<Uuid, Fields>,
    entries: HashMap<Uuid, Fields>,
    overview: Option<Value>,
    at: Option<Instant>,
}

#[derive(Serialize)]
struct Msg<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    machines: HashMap<Uuid, Patch>,
    entries: HashMap<Uuid, Patch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    overview: Option<&'a Value>,
}

pub async fn run_forever(st: AppState) {
    let mut p = Published::default();
    let mut t = tokio::time::interval(EVERY);
    t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        t.tick().await;
        if st.events.receiver_count() == 0 {
            continue;
        }
        let dirty = st.metrics_dirty.swap(false, Ordering::Relaxed);
        if !dirty && p.at.is_some_and(|a| a.elapsed() < SWEEP) {
            continue;
        }
        if let Err(e) = publish(&st, &mut p).await {
            tracing::warn!("实时指标比对失败：{e}");
        }
    }
}

/// 比对一次，有变化就广播。返回是否发了消息
pub async fn publish(st: &AppState, p: &mut Published) -> sqlx::Result<bool> {
    // 比对要最新的视图（短缓存 TTL 与比对间隔同为 3 s，不作废会隔一轮才拿到新数据）；新视图存回缓存，REST 接着用
    st.live.invalidate();
    let ms = views::live(st).await?;
    p.at = Some(Instant::now());
    let mut msg = Msg { kind: "metrics", machines: HashMap::new(), entries: HashMap::new(), overview: None };
    let mut seen_e = HashSet::new();
    for m in ms.iter() {
        if let Some(d) = changed(&mut p.machines, m.id, &M_FIELDS, machine_fields(m)) {
            msg.machines.insert(m.id, d);
        }
        // 入口同时挂在节点和线路机上，只比一次
        for e in m.entries.iter().filter(|e| seen_e.insert(e.id)) {
            if let Some(d) = changed(&mut p.entries, e.id, &E_FIELDS, entry_fields(e)) {
                msg.entries.insert(e.id, d);
            }
        }
    }
    let ids: HashSet<Uuid> = ms.iter().map(|m| m.id).collect();
    p.machines.retain(|id, _| ids.contains(id));
    p.entries.retain(|id, _| seen_e.contains(id));
    let ov = views::overview_of(&ms);
    let ov_changed = p.overview.as_ref() != Some(&ov);
    p.overview = Some(ov);
    if ov_changed {
        msg.overview = p.overview.as_ref();
    }
    if msg.machines.is_empty() && msg.entries.is_empty() && msg.overview.is_none() {
        return Ok(false);
    }
    let json = serde_json::to_string(&msg).expect("指标消息可序列化");
    let _ = st.events.send(Event::Metrics(json.into()));
    Ok(true)
}

/// 和上次相比变了的字段；同时把这次的值记为新基线
fn changed(prev: &mut HashMap<Uuid, Fields>, id: Uuid, names: &[&'static str], cur: Fields) -> Option<Patch> {
    let old = prev.get(&id);
    let d: Patch = names
        .iter()
        .zip(&cur)
        .enumerate()
        .filter(|(i, (_, v))| old.is_none_or(|o| o[*i].get() != v.get()))
        .map(|(_, (k, v))| (*k, v.clone()))
        .collect();
    prev.insert(id, cur);
    (!d.is_empty()).then_some(d)
}

fn raw<T: Serialize + ?Sized>(v: &T) -> Box<RawValue> {
    to_raw_value(v).expect("视图字段可序列化")
}

fn secs(t: OffsetDateTime) -> i64 {
    t.unix_timestamp()
}

fn machine_fields(m: &MachineView) -> Fields {
    // [ts, cpu_pct, load1, mem_used, mem_total, disk_used, disk_total, net_rx_bps, net_tx_bps, tcp_conns]，ts 为 Unix 秒、速率取整
    let host = m.host.as_ref().map(|h| {
        (secs(h.ts), h.cpu_pct, h.load1, h.mem_used, h.mem_total, h.disk_used, h.disk_total, h.net_rx_bps.map(|v| v.round() as i64), h.net_tx_bps.map(|v| v.round() as i64), h.tcp_conns)
    });
    #[derive(Serialize)]
    struct AgentState<'a> {
        version: &'a Option<String>,
        online: bool,
        xray_running: Option<bool>,
        cores: Option<i32>,
    }
    let agent = m.agent.as_ref().map(|a| AgentState { version: &a.version, online: a.online, xray_running: a.xray_running, cores: a.cores });
    vec![
        raw(&host),
        raw(&m.agent.as_ref().and_then(|a| a.last_report_at).map(secs)),
        raw(&agent),
        raw(&m.health),
        raw(&m.maintenance),
        raw(&m.subscription.as_ref().map(|s| s.due_days)),
        raw(&m.removed_traffic),
        raw(&m.quota),
    ]
}

/// [ok, latency_ms, ts, detail]，ts 为 Unix 秒
fn probe(p: &Option<Probe>) -> Box<RawValue> {
    raw(&p.as_ref().map(|p| (p.ok, p.latency_ms, secs(p.ts), &p.detail)))
}

fn entry_fields(e: &EntryView) -> Fields {
    vec![probe(&e.tcp), probe(&e.egress), raw(&e.traffic)]
}
