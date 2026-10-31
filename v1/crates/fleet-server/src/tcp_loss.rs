//! 持续握手丢包（告警规则 `tcp_loss_high`，warning，节点健康算「降级」）：线路机 → 节点的入口 TCP 探测在近 30 分钟里
//! 失败率或带 SYN 重传的比例持续偏高。
//!
//! 为什么另立一条：`tcp_fail` 要连续失败才开（去抖 90 秒、恢复只要连续 2 次成功），供应商只丢一部分入站新建连接时
//! 失败是零散的——2026-10-27 DeerVPS 4 台从 09:30 起连续几个小时 10–20% 的探测「超时 10 秒」、22–40% 的成功建连带 SYN 重传，
//! `tcp_fail` 只零星开过几次又很快恢复，健康一直「正常」，是人看网页卡片上的红块才发现的。这里按窗口统计比例，不看连续性。
//!
//! 每 60 秒算一次（`run_forever`）：在用的节点（VPS、家宽代理、同机 IP 节点；不含维护中的）汇总它在用入口近 `WINDOW_MIN` 分钟的
//! TCP 探测（probe_result kind = tcp，每条入口每 30 秒一次，30 分钟约 60 次）。经离线、维护中、Xray 停了的线路机的入口不算（同 tcp_fail：
//! 那时的失败不是节点的问题）。穿透入口没有这项探测，不算；
//! 节点的入口都切成穿透后开着的直接撤掉（入站丢包已经绕开了）。没有复用 probe_hourly：它按整点小时、也不记重传；网页心跳条（api/misc.rs probe_beats）是 3 分钟一格的展示查询。
//!
//! - 开：样本 ≥ `MIN_SAMPLES`，且失败 ≥ `FAIL_PCT`%（并且至少 `FAIL_MIN` 次）或成功建连里带 SYN 重传的 ≥ `RETRANS_PCT`%（并且至少 `RETRANS_MIN` 次）。
//! - 关（滞回）：失败率与重传率都低于各自阈值的一半，连续 `CLEAR_HOLD_S` 秒才关；样本不够时判不出，开着的保持。
//! - 结果放进 `LiveCache`（`set_tcp_loss`），视图补指标时带上（`MachineView::tcp_loss`），health.rs 给原因，告警引擎照常去抖、开关、静默；
//!   闲置备用照常算健康、不开告警（alerts.rs）。中控重启后先从在告警的 `tcp_loss_high` 恢复「已触发」状态，第一轮算完之前判不出（不恢复）。
//! - 供应商 / 网段关联：同一轮里同供应商（machine.provider_id）或同 /24 的其他在用节点也在触发时，摘要追加「同供应商 X 共 N 台同时（可能是供应商入站丢包）」，
//!   通知合并成一条后一眼能看出是供应商的问题，先找供应商（`fleetctl diag` 给原始数据）而不是逐台查。

use crate::reverse;
use crate::state::AppState;
use crate::views::{self, MachineView};
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::Arc;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

pub const RULE: &str = "tcp_loss_high";
/// 统计窗口（分钟）：够长才能把零散的失败攒成比例，又不至于供应商恢复后还要等半天
pub const WINDOW_MIN: i64 = 30;
/// 样本下限：一条入口 30 分钟约 60 次，至少一半（刚上架、线路机刚回来的不判）
pub const MIN_SAMPLES: i64 = 30;
/// 失败率阈值（%）：正常机器 0%，2026-10-27 异常时 5–20%。60 次里 3 次就到 5%
pub const FAIL_PCT: f64 = 5.0;
/// 失败至少这么多次：小样本时 2 次偶发失败也可能过 5%，偶发 1–2 次不开
pub const FAIL_MIN: i64 = 3;
/// 带 SYN 重传的成功建连占比阈值（%）：正常 0%，异常时 22–40%；取 15% 留出余量
pub const RETRANS_PCT: f64 = 15.0;
/// 重传至少这么多次：偶发拥塞一两次重传不算
pub const RETRANS_MIN: i64 = 5;
/// 恢复要两个比例都低于阈值的一半、连续这么久（秒）：开 5% / 15%，关 2.5% / 7.5%，免得在阈值上下来回开关
pub const CLEAR_HOLD_S: i64 = 10 * 60;
/// 多久算一次
const EVERY: std::time::Duration = std::time::Duration::from_secs(60);

/// 一台节点在视图里的判断（只给健康判定用，不对外输出）
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Loss {
    /// 判不出：中控刚启动还没算过，或已在触发、这一轮样本不够（开着的不恢复）
    #[default]
    Unknown,
    /// 没有持续丢包
    Clear,
    /// 在触发：原因文字（含供应商 / 网段关联）
    Lossy(String),
}

/// 机器 id → 判断（只放在触发、判不出的；不在里面的是 `Clear`）；中控启动后还没算过时整个为 None（`LiveCache::tcp_loss`）
pub type Map = Arc<HashMap<Uuid, Loss>>;

/// 一台节点近 `WINDOW_MIN` 分钟的入口 TCP 探测
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// 探测次数
    pub n: i64,
    /// 失败次数（多是「超时 10 秒」：SYN 连发 4 次都没回）
    pub fail: i64,
    /// 成功、并且报了重传数的次数（旧 agent 不报，不进分母）
    pub ok_known: i64,
    /// 其中建连时 SYN 重传过的次数
    pub retrans: i64,
}

impl Stats {
    fn add(&mut self, o: &Stats) {
        self.n += o.n;
        self.fail += o.fail;
        self.ok_known += o.ok_known;
        self.retrans += o.retrans;
    }
    pub fn fail_pct(&self) -> f64 { if self.n == 0 { 0.0 } else { self.fail as f64 * 100.0 / self.n as f64 } }
    pub fn retrans_pct(&self) -> f64 { if self.ok_known == 0 { 0.0 } else { self.retrans as f64 * 100.0 / self.ok_known as f64 } }
    pub fn enough(&self) -> bool { self.n >= MIN_SAMPLES }
    /// 满足开的条件
    pub fn bad(&self) -> bool {
        self.enough() && ((self.fail >= FAIL_MIN && self.fail_pct() >= FAIL_PCT) || (self.retrans >= RETRANS_MIN && self.retrans_pct() >= RETRANS_PCT))
    }
    /// 满足关的条件（两个比例都低于阈值的一半）
    pub fn clear(&self) -> bool { self.enough() && self.fail_pct() < FAIL_PCT / 2.0 && self.retrans_pct() < RETRANS_PCT / 2.0 }
    /// 摘要：次数与比例都写上（供应商要的是数字）
    pub fn text(&self) -> String {
        let mut s = format!("近 {WINDOW_MIN} 分钟线路机到节点 TCP 探测 {} 次，失败 {} 次（{:.0}%）", self.n, self.fail, self.fail_pct());
        if self.ok_known > 0 {
            s += &format!("，成功的 {} 次里 {} 次 SYN 重传（{:.0}%）", self.ok_known, self.retrans, self.retrans_pct());
        }
        s
    }
}

/// 已触发的一台：最近一次样本够时的摘要（中控重启后从告警恢复的为 None，下一次样本够时补上）、两个比例从什么时候起一直低于一半阈值
#[derive(Debug, Clone, Default)]
struct Latch {
    text: Option<String>,
    clear_since: Option<OffsetDateTime>,
}

/// 两次计算之间要记住的：哪些节点在触发（滞回要用）
#[derive(Default)]
pub struct LossState {
    seeded: bool,
    latched: HashMap<Uuid, Latch>,
}

/// 关联用的网段：IPv4 的 /24（「203.0.113.0/24」）；IPv6、域名不关联
fn net24(ip: &str) -> Option<String> {
    let a: Ipv4Addr = ip.parse().ok()?;
    let o = a.octets();
    Some(format!("{}.{}.{}.0/24", o[0], o[1], o[2]))
}

/// 一台节点用来关联的信息
struct Peer {
    id: Uuid,
    provider: Option<(Uuid, String)>,
    net: Option<String>,
    idle_spare: bool,
}

/// 关联后缀：同供应商（优先）或同 /24 里此刻也在触发的在用节点（含它自己）≥ 2 台时写出来。闲置备用不算进别人的台数
fn correlate(me: &Peer, firing: &[&Peer]) -> String {
    let mates = |same: &dyn Fn(&Peer) -> bool| firing.iter().filter(|p| (p.id == me.id || !p.idle_spare) && same(p)).count();
    if let Some((pid, pname)) = &me.provider {
        let n = mates(&|p| p.provider.as_ref().is_some_and(|x| x.0 == *pid));
        if n >= 2 {
            return format!("；同供应商 {pname} 共 {n} 台同时（可能是供应商入站丢包）");
        }
    }
    if let Some(net) = &me.net {
        let n = mates(&|p| p.net.as_deref() == Some(net));
        if n >= 2 {
            return format!("；同网段 {net} 共 {n} 台同时（可能是供应商入站丢包）");
        }
    }
    String::new()
}

/// 参与判断的节点：在用、不是线路机、不在维护
fn judged(m: &MachineView) -> bool { m.lifecycle == "active" && m.kind != "relay" && !m.maintenance }

/// 有线路机 → 节点 TCP 探测的入口：在用、不是直出、不是穿透（穿透是节点主动连线路机，没有这项探测，入站握手丢包也不影响它）
fn probed(e: &views::EntryView) -> bool { e.lifecycle == "active" && !e.direct && e.mode != reverse::REVERSE }

/// 计入统计的入口：有 TCP 探测的；经离线、维护中、Xray 停了的线路机的不算（同 health.rs probe_reasons）
fn counted(e: &views::EntryView) -> bool { probed(e) && !e.relay_offline && !e.relay_maint && !e.relay_xray_down }

/// 各入口在 (`now` − 窗口, `now`] 里的 TCP 探测
async fn entry_stats(db: &sqlx::PgPool, ids: &[Uuid], now: OffsetDateTime) -> sqlx::Result<HashMap<Uuid, Stats>> {
    let rows: Vec<(Uuid, i64, i64, i64, i64)> = sqlx::query_as(
        "select x.entry_id, count(*), count(*) filter (where not x.ok), count(*) filter (where x.ok and x.retrans is not null),
                count(*) filter (where x.ok and x.retrans > 0)
         from unnest($1::uuid[]) e(id) cross join lateral (
           select entry_id, ok, retrans from probe_result
           where entry_id = e.id and kind = 'tcp' and ts > $2 - make_interval(mins => $3::int) and ts <= $2) x
         group by 1",
    )
    .bind(ids)
    .bind(now)
    .bind(WINDOW_MIN as i32)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|(id, n, fail, ok_known, retrans)| (id, Stats { n, fail, ok_known, retrans })).collect())
}

/// 算一轮，结果放进 `LiveCache`。`now` 由调用方给（测试快进滞回时间）
pub async fn evaluate(st: &AppState, state: &mut LossState, now: OffsetDateTime) -> anyhow::Result<()> {
    if !state.seeded {
        // 中控重启：在告警的那几台先当「已触发」，否则比例落在两个阈值之间时会被当成没问题、告警悄悄恢复
        let ids: Vec<Uuid> = sqlx::query_scalar("select subject_id from incident where state = 'firing' and engine = 'machine' and rule = $1")
            .bind(RULE)
            .fetch_all(&st.db)
            .await?;
        state.latched.extend(ids.into_iter().map(|id| (id, Latch::default())));
        state.seeded = true;
    }
    let ms = views::live(st).await?;
    // 节点上已经没有要探测的入口（都切成穿透、入口都撤了）：再也等不到样本，开着的直接撤掉，否则一直「样本不够、保持」收不回来
    state.latched.retain(|id, _| !ms.iter().any(|m| m.id == *id && judged(m) && !m.entries.iter().any(probed)));
    let ids: Vec<Uuid> = ms.iter().filter(|m| judged(m)).flat_map(|m| m.entries.iter().filter(|e| counted(e)).map(|e| e.id)).collect();
    let per_entry = entry_stats(&st.db, &ids, now).await?;
    let mut stats: HashMap<Uuid, Stats> = HashMap::new();
    for m in ms.iter().filter(|m| judged(m)) {
        let s = stats.entry(m.id).or_default();
        for e in m.entries.iter().filter(|e| counted(e)) {
            if let Some(x) = per_entry.get(&e.id) {
                s.add(x);
            }
        }
    }
    let map = step(state, &ms, &stats, now);
    st.live.set_tcp_loss(map);
    Ok(())
}

/// 按这一轮的统计推进触发状态、算出每台的判断（纯函数，单测直接用）
fn step(state: &mut LossState, ms: &[MachineView], stats: &HashMap<Uuid, Stats>, now: OffsetDateTime) -> HashMap<Uuid, Loss> {
    // 不再在用（退役、删了）的不记；维护中的保持原样，维护结束后按那时再判
    state.latched.retain(|id, _| ms.iter().any(|m| m.id == *id && m.lifecycle == "active" && m.kind != "relay"));
    for (id, s) in stats {
        if s.bad() {
            state.latched.insert(*id, Latch { text: Some(s.text()), clear_since: None });
            continue;
        }
        let Some(l) = state.latched.get_mut(id) else { continue };
        // 样本不够：判不出，摘要沿用上次的、恢复计时不动
        if !s.enough() {
            continue;
        }
        l.text = Some(s.text());
        if !s.clear() {
            l.clear_since = None;
            continue;
        }
        let since = *l.clear_since.get_or_insert(now);
        if now - since >= Duration::seconds(CLEAR_HOLD_S) {
            state.latched.remove(id);
        }
    }
    let peers: HashMap<Uuid, Peer> = ms
        .iter()
        .filter(|m| state.latched.contains_key(&m.id))
        .map(|m| (m.id, Peer { id: m.id, provider: m.provider.as_ref().map(|p| (p.id, p.name.clone())), net: net24(&m.ip), idle_spare: m.idle_spare }))
        .collect();
    let firing: Vec<&Peer> = peers.values().collect();
    state
        .latched
        .iter()
        .map(|(id, l)| {
            let v = match (&l.text, peers.get(id)) {
                (Some(t), Some(p)) => Loss::Lossy(format!("{t}{}", correlate(p, &firing))),
                _ => Loss::Unknown,
            };
            (*id, v)
        })
        .collect()
}

/// 视图里这台的判断：还没算过为 Unknown，算过、不在表里的为 Clear
pub fn of(map: Option<&Map>, id: Uuid) -> Loss {
    match map {
        None => Loss::Unknown,
        Some(m) => m.get(&id).cloned().unwrap_or(Loss::Clear),
    }
}

/// 每 60 秒一轮
pub async fn run_forever(st: AppState) {
    let mut state = LossState::default();
    let mut t = tokio::time::interval(EVERY);
    t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        t.tick().await;
        if let Err(e) = evaluate(&st, &mut state, OffsetDateTime::now_utc()).await {
            tracing::error!("持续握手丢包评估失败：{e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::Ref;

    fn s(n: i64, fail: i64, ok_known: i64, retrans: i64) -> Stats { Stats { n, fail, ok_known, retrans } }

    #[test]
    fn thresholds() {
        // 正常机器：0 失败 0 重传
        assert!(!s(60, 0, 60, 0).bad() && s(60, 0, 60, 0).clear());
        // 偶发 1–2 次失败不开：60 次里 2 次（3%），小样本 30 次里 2 次（7%）也不开（至少 3 次）
        assert!(!s(60, 2, 58, 0).bad());
        assert!(!s(30, 2, 28, 0).bad());
        // 2026-10-27 的样子：13% 失败、35% 重传
        assert!(s(60, 8, 52, 18).bad());
        // 只有重传高（入站 SYN 被丢一部分、重发后连上）
        assert!(s(60, 0, 60, 12).bad());
        // 重传 4 次（7%）：不到 5 次、也不到 15%
        assert!(!s(60, 0, 60, 4).bad());
        // 样本不够不判
        assert!(!s(20, 10, 10, 5).bad() && !s(20, 0, 20, 0).clear());
        // 旧 agent 不报重传：只按失败率
        assert!(s(60, 4, 0, 0).bad() && s(60, 1, 0, 0).clear());
        // 滞回：3%（低于 5%、不低于 2.5%）不开也不关
        assert!(!s(60, 2, 58, 0).bad() && !s(60, 2, 58, 0).clear());
    }

    #[test]
    fn summary_text() {
        assert_eq!(s(62, 8, 54, 21).text(), "近 30 分钟线路机到节点 TCP 探测 62 次，失败 8 次（13%），成功的 54 次里 21 次 SYN 重传（39%）");
        assert_eq!(s(60, 4, 0, 0).text(), "近 30 分钟线路机到节点 TCP 探测 60 次，失败 4 次（7%）");
    }

    fn node(ip: &str, provider: Option<(Uuid, &str)>) -> MachineView {
        let mut m = crate::health::tests::base("vps");
        m.ip = ip.into();
        m.provider = provider.map(|(id, n)| Ref { id, name: n.into() });
        m
    }

    #[test]
    fn latch_hysteresis_and_correlation() {
        let deer = Uuid::now_v7();
        let other = Uuid::now_v7();
        let ms = vec![
            node("203.0.113.5", Some((deer, "DeerVPS"))),
            node("203.0.113.6", Some((deer, "DeerVPS"))),
            node("198.51.100.7", Some((other, "GOMAMI"))),
            node("198.51.100.8", Some((Uuid::now_v7(), "ATTVPS"))),
        ];
        let (a, b, c, d) = (ms[0].id, ms[1].id, ms[2].id, ms[3].id);
        let t0 = OffsetDateTime::now_utc();
        let mut st = LossState::default();
        let bad = s(60, 8, 52, 18);
        let ok = s(60, 0, 60, 0);
        let stats = HashMap::from([(a, bad), (b, bad), (c, bad), (d, bad)]);
        let out = step(&mut st, &ms, &stats, t0);
        let text = |id| match &out[&id] {
            Loss::Lossy(t) => t.clone(),
            x => panic!("{x:?}"),
        };
        assert!(text(a).ends_with("；同供应商 DeerVPS 共 2 台同时（可能是供应商入站丢包）"), "{}", text(a));
        // 不同供应商、同一个 /24：按网段关联
        assert!(text(c).ends_with("；同网段 198.51.100.0/24 共 2 台同时（可能是供应商入站丢包）"), "{}", text(c));
        // 落到两个阈值之间（3%）：不关
        let mid = s(60, 2, 58, 0);
        let out = step(&mut st, &ms, &HashMap::from([(a, mid), (b, bad), (c, ok), (d, ok)]), t0 + Duration::minutes(1));
        assert!(matches!(out.get(&a), Some(Loss::Lossy(_))));
        // 只剩 a、b 在触发：c 已经开始恢复计时但还在触发，照样算进网段
        assert!(matches!(out.get(&c), Some(Loss::Lossy(_))));
        // 一半阈值以下满 10 分钟才关；中途样本不够（线路机离线）不打断计时也不关
        let out = step(&mut st, &ms, &HashMap::from([(c, s(10, 0, 10, 0))]), t0 + Duration::minutes(5));
        assert!(matches!(out.get(&c), Some(Loss::Lossy(_))));
        let out = step(&mut st, &ms, &HashMap::from([(c, ok)]), t0 + Duration::minutes(11));
        assert!(!out.contains_key(&c), "c 满 10 分钟，关");
        assert!(out.contains_key(&d), "这一轮没有 d 的统计：保持");
        // d 的计时也从 1 分钟时开始，11 分钟时满 10 分钟：关
        let out = step(&mut st, &ms, &HashMap::from([(d, ok)]), t0 + Duration::minutes(11));
        assert!(!out.contains_key(&d));
        // 只剩 DeerVPS 的两台
        assert_eq!(st.latched.len(), 2);
    }

    #[test]
    fn idle_spare_not_counted_for_others() {
        let deer = Uuid::now_v7();
        let mut ms = vec![node("203.0.113.5", Some((deer, "DeerVPS"))), node("192.0.2.9", Some((deer, "DeerVPS")))];
        ms[1].idle_spare = true;
        let bad = s(60, 8, 52, 18);
        let mut st = LossState::default();
        let out = step(&mut st, &ms, &HashMap::from([(ms[0].id, bad), (ms[1].id, bad)]), OffsetDateTime::now_utc());
        assert_eq!(out[&ms[0].id], Loss::Lossy(bad.text()), "闲置备用不算进同供应商的台数");
    }

    #[test]
    fn seeded_latch_without_samples_is_unknown() {
        let ms = vec![node("203.0.113.5", None)];
        let mut st = LossState { seeded: true, latched: HashMap::from([(ms[0].id, Latch::default())]) };
        let out = step(&mut st, &ms, &HashMap::from([(ms[0].id, s(5, 0, 5, 0))]), OffsetDateTime::now_utc());
        assert_eq!(out[&ms[0].id], Loss::Unknown);
        assert_eq!(of(None, ms[0].id), Loss::Unknown);
        assert_eq!(of(Some(&Arc::new(HashMap::new())), ms[0].id), Loss::Clear);
    }
}
