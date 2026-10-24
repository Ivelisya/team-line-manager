//! 流量异常（告警规则 `nonbusiness_traffic_high`、`business_traffic_spike`，warning）：按小时汇总评估。
//! 小时汇总每跑完一轮（lib.rs `rollup_loop`，每 5 分钟）算一次，结果放进 `LiveCache`，视图带上（`MachineView::anomaly`），
//! health.rs 据此给原因，机器告警引擎（alerts.rs）照常开关、通知、静默；开、关的连续小时数已在这里算好，告警引擎不再去抖。
//!
//! 「最近一个完整小时」是结束满 10 分钟的那个整点小时：整点后头 10 分钟汇总还在给上一小时收尾（db::rollup_dirty）。
//!
//! - 非业务流量偏高：只看有自己网卡读数的 VPS 节点（宿主机的业务含它上面的同机 IP 节点）。一小时里网卡合计（下行 + 上行）÷ 业务流量（下载 + 上传）
//!   超过 `NONBIZ_RATIO_MAX`、多出的（网卡 − 业务 × `NIC_PER_BIZ`）超过 `NONBIZ_EXCESS_MIN` 算异常——扫描、灌包、被滥用的流量不经过入口，
//!   照样算进供应商额度。最近 2 个完整小时都异常才开（这一小时要有业务流量），都正常才关（要有网卡读数：agent 离线的小时判不出，开着的保持）。
//! - 业务流量突增：节点（含家宽代理、同机 IP 节点）最近一个完整小时的业务流量超过近 7 天同一小时（每天的同一个 UTC 整点，星期不限）的中位数 ×
//!   `SPIKE_FACTOR`、且超过 `SPIKE_MIN` 就开；最近 2 个完整小时都回到各自中位数的 `SPIKE_CLEAR_FACTOR` 倍以下（或不到 `SPIKE_MIN`）才关。
//!   近 7 天里的数据不满 `SPIKE_MIN_DAYS` 天不判（新上架的节点没有基线），这种开着的也直接关。
//!
//! 被打（灌包、DDoS）时 agent 常常断线、网卡读数也上不去，这两条抓不到：那时靠出口不通的告警（egress_fail，会给出「临时切到备用」）。

use crate::db::floor_hour;
use std::collections::HashMap;
use std::sync::Arc;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

pub const NONBIZ_RULE: &str = "nonbusiness_traffic_high";
pub const SPIKE_RULE: &str = "business_traffic_spike";

/// 网卡合计 ÷ 业务流量的正常比值：业务的每个字节在节点网卡上进出各一次（约 2 倍）再加协议开销。
/// 依据：2026-10 生产 30 台 VPS 节点的本月、今天两个口径都稳定在 2.16～2.41，取上沿
pub const NIC_PER_BIZ: f64 = 2.4;
/// 比值超过这个算异常（正常上沿 2.41 的 1.7 倍，留足余量）
pub const NONBIZ_RATIO_MAX: f64 = 4.0;
/// 多出的部分超过这么多字节（十进制 GB，同额度）才算：业务很少的小时比值本身抖得厉害，多出几百 MB 不值得告警
pub const NONBIZ_EXCESS_MIN: f64 = 2e9;
/// 业务流量突增：超过同一小时中位数的这么多倍
pub const SPIKE_FACTOR: f64 = 5.0;
/// 并且超过这么多字节（平时很闲的节点涨几倍也不值得告警）
pub const SPIKE_MIN: f64 = 5e9;
/// 回到中位数的这么多倍以下算恢复（开 5 倍、关 2 倍：回差，免得在 5 倍上下每小时开关一次）
pub const SPIKE_CLEAR_FACTOR: f64 = 2.0;
/// 近 7 天里至少有这么多天的数据才判突增
pub const SPIKE_MIN_DAYS: i64 = 3;
/// 突增的基线看几天
const BASELINE_DAYS: i64 = 7;

/// 一台机器的流量异常判断（只给告警用，不对外输出）
#[derive(Debug, Clone, Default)]
pub struct Anomaly {
    /// 非业务流量偏高成立时的原因文字（最近 2 个完整小时都异常）
    pub nonbiz: Option<String>,
    /// 最近 2 个完整小时都正常：开着的非业务流量告警可以恢复
    pub nonbiz_clear: bool,
    /// 业务流量突增成立时的原因文字
    pub spike: Option<String>,
    /// 最近 2 个完整小时都回落了（或数据不够判）：开着的突增告警可以恢复
    pub spike_clear: bool,
}

/// 机器 id → 判断；中控启动后汇总还没跑完第一轮时整个为 None（`LiveCache::anomaly`）
pub type Map = Arc<HashMap<Uuid, Anomaly>>;

/// 这两条规则开、关不去抖（连续小时数已经算过）
pub fn hourly(rule: &str) -> bool { rule == NONBIZ_RULE || rule == SPIKE_RULE }

/// 开着的这条告警能不能恢复：不是这两条规则为 None；还没算过（中控刚启动）、这台没有判断为 false（保持原状）
pub fn cleared(a: Option<&Anomaly>, rule: &str) -> Option<bool> {
    match rule {
        NONBIZ_RULE => Some(a.is_some_and(|a| a.nonbiz_clear)),
        SPIKE_RULE => Some(a.is_some_and(|a| a.spike_clear)),
        _ => None,
    }
}

fn gb(b: f64) -> String { format!("{:.1} GB", b.max(0.0) / 1e9) }

/// 中位数（样本已非空）
fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    let n = v.len();
    if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 }
}

/// 网卡比业务多出的部分不经过入口：比值超过 `NONBIZ_RATIO_MAX` 且多出的超过 `NONBIZ_EXCESS_MIN`（不做除法：业务为 0 时比值无穷大）
fn nonbiz_bad(nic: f64, biz: f64) -> bool { nic - biz * NIC_PER_BIZ > NONBIZ_EXCESS_MIN && nic > biz * NONBIZ_RATIO_MAX }

/// 按 `now` 的最近两个完整小时评估在用的非线路机。只读小时汇总：业务流量按入口的节点归属（含已移除的入口），网卡读 host_metric_hourly
pub async fn evaluate(db: &sqlx::PgPool, now: OffsetDateTime) -> sqlx::Result<HashMap<Uuid, Anomaly>> {
    let h0 = floor_hour(now - Duration::minutes(10)) - Duration::hours(1);
    let h1 = h0 - Duration::hours(1);
    let day = Duration::days(1);
    // 两个小时各自及前 7 天的同一小时
    let hours: Vec<OffsetDateTime> = (0..=BASELINE_DAYS).flat_map(|k| [h0 - day * k as i32, h1 - day * k as i32]).collect();
    let machines: Vec<(Uuid, String, Option<Uuid>)> =
        sqlx::query_as("select id, kind::text, host_id from machine where lifecycle = 'active' and kind <> 'relay'").fetch_all(db).await?;
    let biz: HashMap<(Uuid, OffsetDateTime), f64> = sqlx::query_as::<_, (Uuid, OffsetDateTime, f64)>(
        "select e.node_id, h.hour, sum(h.up_bytes::float8 + h.down_bytes) from entry_traffic_hourly h join entry e on e.id = h.entry_id
         where h.hour = any($1) group by 1, 2",
    )
    .bind(&hours)
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|(n, h, b)| ((n, h), b))
    .collect();
    // 近 7 天里最早有数据的小时：晚于 h0 前 3 天的（数据不满 3 天）没有一行
    let start: HashMap<Uuid, OffsetDateTime> = sqlx::query_as(
        "select e.node_id, min(h.hour) from entry_traffic_hourly h join entry e on e.id = h.entry_id
         where h.hour >= $1 and h.hour <= $2 group by 1",
    )
    .bind(h0 - day * BASELINE_DAYS as i32)
    .bind(h0 - day * SPIKE_MIN_DAYS as i32)
    .fetch_all(db)
    .await?
    .into_iter()
    .collect();
    let nic: HashMap<(Uuid, OffsetDateTime), f64> = sqlx::query_as::<_, (Uuid, OffsetDateTime, f64)>(
        "select machine_id, hour, (net_rx + net_tx)::float8 from host_metric_hourly where hour = any($1)",
    )
    .bind([h0, h1].as_slice())
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|(m, h, b)| ((m, h), b))
    .collect();
    let b = |n: Uuid, h: OffsetDateTime| biz.get(&(n, h)).copied().unwrap_or(0.0);
    // 宿主机网卡上还有同机 IP 节点的业务
    let mut ip_nodes: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for (id, _, host) in &machines {
        if let Some(h) = host {
            ip_nodes.entry(*h).or_default().push(*id);
        }
    }
    let mut out = HashMap::new();
    for (id, kind, host) in &machines {
        let mut a = Anomaly { spike_clear: true, ..Default::default() };
        if kind == "vps" && host.is_none() {
            let total = |h| b(*id, h) + ip_nodes.get(id).map_or(0.0, |v| v.iter().map(|n| b(*n, h)).sum());
            let at = |h| nic.get(&(*id, h)).map(|n| (*n, total(h)));
            let (x0, x1) = (at(h0), at(h1));
            let abnormal = |x: Option<(f64, f64)>| x.is_some_and(|(n, b)| b > 0.0 && nonbiz_bad(n, b));
            if let (true, true, Some((n, b))) = (abnormal(x0), abnormal(x1), x0) {
                a.nonbiz = Some(format!(
                    "最近 1 小时网卡 {}，业务只有 {}（正常约 {NIC_PER_BIZ} 倍），多出的约 {} 不经过入口，可能被扫描或滥用；这部分也算额度",
                    gb(n),
                    gb(b),
                    gb(n - b * NIC_PER_BIZ)
                ));
            }
            a.nonbiz_clear = [x0, x1].iter().all(|x| x.is_some_and(|(n, b)| !nonbiz_bad(n, b)));
        }
        if let Some(s) = start.get(id) {
            // 这一小时的业务流量与近 7 天同一小时的中位数（数据开始之前的天不算样本，之后缺的小时按 0）、样本天数
            let slot = |h: OffsetDateTime| {
                let v: Vec<f64> = (1..=BASELINE_DAYS).map(|k| h - day * k as i32).filter(|x| x >= s).map(|x| b(*id, x)).collect();
                let n = v.len();
                (b(*id, h), if n == 0 { 0.0 } else { median(v) }, n)
            };
            let ((b0, m0, days), (b1, m1, _)) = (slot(h0), slot(h1));
            if b0 > m0 * SPIKE_FACTOR && b0 > SPIKE_MIN {
                let times = if m0 >= 5e7 { format!("（约 {:.0} 倍）", b0 / m0) } else { String::new() };
                a.spike = Some(format!("最近 1 小时业务流量 {}，近 {days} 天同一时段的中位数只有 {}{times}", gb(b0), gb(m0)));
            }
            let calm = |b: f64, m: f64| b < m * SPIKE_CLEAR_FACTOR || b <= SPIKE_MIN;
            a.spike_clear = calm(b0, m0) && calm(b1, m1);
        }
        out.insert(*id, a);
    }
    Ok(out)
}

