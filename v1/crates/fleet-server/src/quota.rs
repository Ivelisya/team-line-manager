//! 流量额度的预测与搬迁建议：按近 7 天日均算「预计哪天用满」，再算搬走哪些入口能撑到重置日。
//! 纯计算放在这里（单元测试不起库）；取数与接口在 `api/lifecycle.rs` 的 `rebalance`。

use time::{Duration, OffsetDateTime};
use uuid::Uuid;

/// 日均取近多少天（周期开头不足的按周期内已有的天数）
pub const WINDOW_DAYS: i64 = 7;
/// 目标线路机接收入口后，到重置日预计的用量不超过上限的这个比例
pub const TARGET_SAFE_PCT: f64 = 80.0;
/// 已超出额度的线路机：搬迁目标是搬走后日均不超过原来的这个比例（到重置日前不再大量增长；超额部分可能限速或另收费）
pub const OVER_LIMIT_KEEP_PCT: f64 = 20.0;
/// 预测至少要有这么久的数据（周期刚重置、刚上架的不预测）
const MIN_WINDOW_S: i64 = 24 * 3600;

const DAY_S: f64 = 86400.0;

/// 近 7 天日均（字节 / 天）与按它预计用满的时刻。
/// `window_used`：窗口 `[window_start, now]` 内的用量；窗口不足 1 天返回 (None, None)。
/// 预计用满时刻只在早于 `period_end`（重置时刻）时给出；已经用满、日均为 0 都没有
pub fn forecast(used: i64, limit_bytes: Option<i64>, window_used: i64, window_start: OffsetDateTime, now: OffsetDateTime, period_end: Option<OffsetDateTime>) -> (Option<i64>, Option<OffsetDateTime>) {
    let span = (now - window_start).whole_seconds();
    if span < MIN_WINDOW_S {
        return (None, None);
    }
    let daily = (window_used.max(0) as f64 * DAY_S / span as f64).round() as i64;
    let full = (|| {
        let (limit, end) = (limit_bytes?, period_end?);
        if daily <= 0 || used >= limit {
            return None;
        }
        let secs = (limit - used) as f64 / daily as f64 * DAY_S;
        // 远远超出重置日的不必算成时刻（也避免 Duration 溢出）
        if secs > (end - now).as_seconds_f64() {
            return None;
        }
        // 精确到小时：同一份缓存的用量下结果稳定，不随每次刷新抖动
        let at = OffsetDateTime::from_unix_timestamp((now + Duration::seconds_f64(secs)).unix_timestamp() / 3600 * 3600).ok()?;
        (at < end).then_some(at)
    })();
    (Some(daily), full)
}

/// 滚动 30 天的窗口：整 30 × 24 小时（同供应商面板的 last 30 days），不按日历日、和时区无关
pub const ROLLING_HOURS: i64 = 720;
/// 滚动 30 天的面板读数覆盖录入时刻往前多少秒（同窗口长度）：过了这么久读数里的量全部滚出窗口
const CALIBRATE_ROLLING_S: f64 = (ROLLING_HOURS * 3600) as f64;

/// 滚动 30 天的窗口起点：此刻所在整点往前 `ROLLING_HOURS` 小时。计入小时汇总表里 `hour >= 起点` 的桶：最近 720 个完整的整点桶，
/// 加上当前这个没过完的小时（小时汇总每 5 分钟把当前小时也汇总一遍，表里一般有它的部分数据）。覆盖的是 [起点, now]，
/// 比整 30 天多出 now 在本小时里过去的那几分钟（不少于 30 天，多算不超过 1 小时）。同一个整点内起点不变（视图的缓存键用它）
pub fn rolling_start(now: OffsetDateTime) -> OffsetDateTime {
    let h = OffsetDateTime::from_unix_timestamp(now.unix_timestamp().div_euclid(3600) * 3600).unwrap_or(now);
    h - Duration::hours(ROLLING_HOURS)
}

/// 滚动 30 天的校准读数还没滚出窗口（录入不满 30 天）；满 30 天起就不再算，也不再按录入时刻起算实测（views.rs 的缓存键随之去掉它）
pub fn rolling_calibration_live(base_at: OffsetDateTime, now: OffsetDateTime) -> bool { (now - base_at).as_seconds_f64() < CALIBRATE_ROLLING_S }

/// 按面板校准的读数（`base` 字节，`base_at` 录入）此刻还算在已用里多少；None = 已失效、已用回到纯实测。
/// 只管衰减，「录入时刻在不在当前周期里」由调用方先判断（views.rs，月度周期进入下一个周期就失效）。
/// 月度周期（`rolling` = false）原样全算：读数是本周期开头到录入时刻的量，到重置前一直在周期里。
/// 滚动 30 天：读数是录入时刻往前 30 天的量，按均匀分布，过了 d 天（小数天）还留在窗口里的是 base × (1 − d/30)，满 30 天为 None
/// （窗口也正好 30 天：录入时刻滚出窗口与读数衰减到 0 是同一刻）。
/// 实测从录入时刻往后另算（不衰减），所以 used = 这部分 + 录入之后的实测。录入时刻晚于 now（时钟差）按刚录入算
pub fn calibrated(base: i64, base_at: OffsetDateTime, now: OffsetDateTime, rolling: bool) -> Option<i64> {
    if !rolling {
        return Some(base);
    }
    let elapsed = (now - base_at).as_seconds_f64().max(0.0);
    rolling_calibration_live(base_at, now).then(|| (base as f64 * (1.0 - elapsed / CALIBRATE_ROLLING_S)).round() as i64)
}

/// 搬走一部分入口后，该线路机每天少多少：从 `rates`（各入口每天的流量，字节）里选出最少的入口，让 `daily - 搬走的` ≤ `allowed`。
/// 先拿最大的，直到剩下的缺口某一个入口就能补上，再换成能补上的最小的那个（少搬一点）。
/// 返回 (选中的下标, 搬走的合计, 是否达到目标)
pub fn pick_moves(daily: f64, allowed: f64, rates: &[f64]) -> (Vec<usize>, f64, bool) {
    let mut need = daily - allowed;
    let mut left: Vec<usize> = (0..rates.len()).filter(|&i| rates[i] > 0.0).collect();
    left.sort_by(|&a, &b| rates[b].total_cmp(&rates[a]));
    let (mut picked, mut cut) = (vec![], 0.0);
    while need > 0.0 && !left.is_empty() {
        // left 按流量从大到小：能单独补上缺口的里面取最小的，没有就拿最大的
        let at = left.iter().rposition(|&i| rates[i] >= need).unwrap_or(0);
        let i = left.remove(at);
        need -= rates[i];
        cut += rates[i];
        picked.push(i);
    }
    (picked, cut, need <= 0.0)
}

/// 一台线路机的额度读数（取自视图的 quota）
#[derive(Debug, Clone, Copy)]
pub struct Q {
    pub limit: Option<f64>,
    pub used: f64,
    pub daily: f64,
    /// 距重置还有多少天；按近 30 天滚动计的用 `Q::rolling`（`ROLLING_DAYS`）
    pub days_left: f64,
    /// 近 30 天滚动计、没有重置日：预计用量按稳态算（见 `Q::rolling`）
    pub rolling: bool,
}

/// 按近 30 天滚动计流量的线路机没有重置日，用量永远是「近 30 天合计」：看稳态，日均 × 30 天
pub const ROLLING_DAYS: f64 = 30.0;

impl Q {
    /// 滚动 30 天的线路机：用量是近 30 天合计，预计用量 = max(已用, 日均 × `ROLLING_DAYS`)——日均已经降下来的（刚搬走了入口）看稳态，
    /// 近 30 天合计还高的（日均刚升起来、窗口里的量还没滚出去）不能低估成稳态；
    /// 承接 `extra` 的日均后在这个基础上加 extra × `ROLLING_DAYS`，即 (日均 + extra) × 30 与 已用 + extra × 30 都要在安全线内；余量按预计用量算
    pub fn rolling(limit: Option<f64>, used: f64, daily: f64) -> Q {
        Q { limit, used, daily, days_left: ROLLING_DAYS, rolling: true }
    }
    /// 按当前速度到重置日预计的用量（滚动 30 天的见 `Q::rolling`）
    pub fn projected(&self) -> f64 {
        if self.rolling { self.used.max(self.daily * self.days_left) } else { self.used + self.daily * self.days_left }
    }
    /// 到重置日还能再放多少（字节），没有上限为 None
    pub fn headroom(&self) -> Option<f64> {
        self.limit.map(|l| l - self.projected())
    }
    /// 再每天多承接 `extra` 字节后，到重置日预计的用量是否仍在安全线内
    pub fn fits(&self, extra: f64) -> bool {
        self.limit.is_none_or(|l| self.projected() + extra * self.days_left <= l * TARGET_SAFE_PCT / 100.0)
    }
}

/// 线路机上 1 字节业务流量按计费规则折成多少计费字节（实测不出来时用）：转发时入站 ≈ 出站 ≈ 业务量，
/// 双向计费（sum）算两份，单向（max / min / in / out）和按业务流量算的（None）算一份
pub fn rule_factor(rule: Option<&str>) -> f64 {
    if rule == Some("sum") { 2.0 } else { 1.0 }
}

/// 实测折算系数（网卡计费日均 ÷ 业务日均）允许偏离 `rule_factor` 的范围：转发的物理关系决定了系数该贴着 `rule_factor`，
/// 偏出一半以上多半是分母不全（背景流量、窗口里搬走的入口、刚搬空的线路机）或数据缺口，实测值不可信
pub const K_MIN_RATIO: f64 = 0.5;
pub const K_MAX_RATIO: f64 = 1.5;

/// 实测系数 `k` 在 `rule_factor` 的 `K_MIN_RATIO`–`K_MAX_RATIO` 倍之内就用它，超出（或不是有限正数）回退 `rule_factor`
pub fn sane_k(k: f64, rule: Option<&str>) -> f64 {
    let f = rule_factor(rule);
    if k.is_finite() && (f * K_MIN_RATIO..=f * K_MAX_RATIO).contains(&k) { k } else { f }
}

/// 目标线路机默认选余量最多的（不限流量的最多）；余量相同取先出现的
pub fn best_target(cands: &[(Uuid, Q)]) -> Option<Uuid> {
    cands
        .iter()
        .max_by(|a, b| match (a.1.headroom(), b.1.headroom()) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, _) => std::cmp::Ordering::Greater,
            (_, None) => std::cmp::Ordering::Less,
            (Some(x), Some(y)) => x.total_cmp(&y),
        })
        .map(|c| c.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    const GB: i64 = 1_000_000_000;

    #[test]
    fn sane_k_falls_back_outside_half_to_one_and_a_half_of_rule_factor() {
        // 单向计费系数该贴着 1：0.5–1.5 内用实测，之外回退 1；双向（sum）该贴着 2：1–3
        assert_eq!(sane_k(1.2, Some("max")), 1.2);
        assert_eq!(sane_k(10.0, Some("max")), 1.0);
        assert_eq!(sane_k(0.3, None), 1.0);
        assert_eq!(sane_k(2.6, Some("sum")), 2.6);
        assert_eq!(sane_k(0.9, Some("sum")), 2.0);
        assert_eq!(sane_k(f64::NAN, Some("sum")), 2.0);
    }

    #[test]
    fn forecast_full_before_reset() {
        let now = datetime!(2026-10-24 12:00 UTC);
        let reset = datetime!(2026-11-13 00:00 UTC);
        // 近 7 天 672 GB = 96 GB/天，已用 1051 GB / 2000 GB：还剩 949 GB ≈ 9.9 天 → 11-03 前后用满，早于重置日
        let (d, f) = forecast(1051 * GB, Some(2000 * GB), 672 * GB, now - Duration::days(7), now, Some(reset));
        assert_eq!(d, Some(96 * GB));
        let f = f.expect("早于重置日");
        assert!(f > datetime!(2026-11-02 0:00 UTC) && f < datetime!(2026-11-04 0:00 UTC), "{f}");
        // 慢一点：到重置日用不满 → 不给
        assert_eq!(forecast(1051 * GB, Some(2000 * GB), 70 * GB, now - Duration::days(7), now, Some(reset)).1, None);
        // 周期才开始 3 天：日均按 3 天算，不是除以 7
        let (d, _) = forecast(300 * GB, Some(2000 * GB), 300 * GB, now - Duration::days(3), now, Some(reset));
        assert_eq!(d, Some(100 * GB));
        // 周期刚重置不足 1 天：不预测；已用满 / 没上限 / 没有重置日：只有日均或什么都没有
        assert_eq!(forecast(10 * GB, Some(2000 * GB), 10 * GB, now - Duration::hours(20), now, Some(reset)), (None, None));
        assert_eq!(forecast(2100 * GB, Some(2000 * GB), 672 * GB, now - Duration::days(7), now, Some(reset)).1, None);
        assert_eq!(forecast(1051 * GB, None, 672 * GB, now - Duration::days(7), now, Some(reset)), (Some(96 * GB), None));
        assert_eq!(forecast(1051 * GB, Some(2000 * GB), 672 * GB, now - Duration::days(7), now, None).1, None);
    }

    #[test]
    fn calibrated_monthly_stays_rolling30_decays_linearly() {
        let at = datetime!(2026-11-01 08:00 UTC);
        let base = 300 * GB;
        // 月度：到重置前原样全算（周期内外由 views 判断），隔多久都一样
        assert_eq!(calibrated(base, at, at + Duration::days(29), false), Some(base));
        // 滚动 30 天：刚录入全算；15 天剩一半；按小数天算（7.5 天剩 3/4）；29 天剩 1/30
        assert_eq!(calibrated(base, at, at, true), Some(base));
        assert_eq!(calibrated(base, at, at + Duration::days(15), true), Some(150 * GB));
        assert_eq!(calibrated(base, at, at + Duration::hours(180), true), Some(225 * GB));
        assert_eq!(calibrated(base, at, at + Duration::days(29), true), Some(10 * GB));
        // 满 30 天及以后完全失效；30 天差 1 秒还剩一点点
        assert_eq!(calibrated(base, at, at + Duration::days(30), true), None);
        assert_eq!(calibrated(base, at, at + Duration::days(45), true), None);
        assert!(calibrated(base, at, at + Duration::days(30) - Duration::seconds(1), true).is_some_and(|v| v > 0 && v < GB));
        // 录入时刻在 now 之后（时钟差）按刚录入算，不放大
        assert_eq!(calibrated(base, at, at - Duration::hours(1), true), Some(base));
        assert_eq!(calibrated(0, at, at + Duration::days(3), true), Some(0));
        assert!(rolling_calibration_live(at, at + Duration::days(30) - Duration::seconds(1)) && !rolling_calibration_live(at, at + Duration::days(30)));
    }

    #[test]
    fn rolling_window_is_720_hours_back_from_the_current_hour() {
        // 起点 = 此刻所在整点往前 720 小时：同一个整点里不变；和时区、日历日无关（洛杉矶零点刚过时也是整 30 天，不是 29 天）
        let now = datetime!(2026-11-22 08:41:07 UTC);
        assert_eq!(rolling_start(now), datetime!(2026-10-23 08:00 UTC));
        assert_eq!(rolling_start(datetime!(2026-11-22 08:00 UTC)), rolling_start(datetime!(2026-11-22 08:59:59 UTC)));
        assert_eq!(rolling_start(datetime!(2026-11-22 09:00 UTC)), datetime!(2026-10-23 09:00 UTC));
        // 覆盖 [起点, now]：不少于 30 天，多出的不到 1 小时
        let span = now - rolling_start(now);
        assert!(span >= Duration::days(30) && span < Duration::days(30) + Duration::hours(1), "{span}");
        // 以前的口径：洛杉矶今天（11-22，刚过零点）往前 29 天的零点 = 10-24 00:00 PDT = 10-24 07:00 UTC，只有 29 天 1 小时 41 分
        assert!(rolling_start(now) < datetime!(2026-10-24 07:00 UTC));
    }

    #[test]
    fn rebalance_picks_fewest_entries() {
        // 线路机 1：日均 96，已用 1051 / 2000，还剩 20 天 → 到重置日 2971，要把日均降到 ≤ (2000-1051)/20 = 47.45
        let q = Q { limit: Some(2000e9), used: 1051e9, daily: 96e9, days_left: 20.0, rolling: false };
        let allowed = (q.limit.unwrap() - q.used) / q.days_left;
        // 入口日均：40、30、20、5、1：先拿 40，缺口 8.55 → 单个能补上的最小是 20（不拿 30）
        let rates = [30e9, 40e9, 1e9, 20e9, 5e9];
        let (p, cut, ok) = pick_moves(q.daily, allowed, &rates);
        assert_eq!((p, ok, cut), (vec![1, 3], true, 60e9));
        // 全搬也不够：给出所有有流量的并标未达成
        let (p, _, ok) = pick_moves(96e9, 1.0, &[10e9, 0.0, 20e9]);
        assert_eq!((p.len(), ok), (2, false));
        // 目标：余量最多的（线路机 3：12 TB 几乎空着）；承接 60 GB/天 到重置日仍在 80% 内才放得下
        let (a, b) = (Uuid::from_u128(1), Uuid::from_u128(3));
        let t3 = Q { limit: Some(12000e9), used: 100e9, daily: 5e9, days_left: 20.0, rolling: false };
        assert_eq!(best_target(&[(a, Q { limit: Some(2000e9), used: 500e9, daily: 10e9, days_left: 20.0, rolling: false }), (b, t3)]), Some(b));
        assert!(t3.fits(60e9) && !t3.fits(500e9));
    }

    #[test]
    fn rolling30_target_counts_steady_state_daily_times_30() {
        // 上限 2000 GB、日均 40 GB：稳态 1200 GB，余量 800 GB；再承接 10 GB/天 → 1500 GB，在 80%（1600）内；承接 15 GB/天 → 1650 GB，放不下
        let r = Q::rolling(Some(2000e9), 1000e9, 40e9);
        assert_eq!(r.projected(), 1200e9);
        assert_eq!(r.headroom(), Some(800e9));
        assert!(r.fits(10e9) && !r.fits(15e9));
        // 以前没有重置日时剩余天数取 0：日均和承接量都被忽略，什么都放得下
        let old = Q { limit: Some(2000e9), used: 0.0, daily: 40e9, days_left: 0.0, rolling: false };
        assert!(old.fits(500e9));
        // 日均大的线路机余量少，默认目标不会选它（日均 90 GB 稳态 2700 GB 已超上限）
        let busy = Q::rolling(Some(2000e9), 1000e9, 90e9);
        assert_eq!(busy.headroom(), Some(-700e9));
        assert!(!busy.fits(0.0));
        let (a, b) = (Uuid::from_u128(1), Uuid::from_u128(2));
        assert_eq!(best_target(&[(a, busy), (b, r)]), Some(b));
        // 不限流量的永远放得下
        assert!(Q::rolling(None, 0.0, 90e9).fits(1e12));
        // 近 30 天合计比日均 × 30 高（日均刚降下来、窗口里的量还没滚出去）：预计用量不低于已用，余量按已用算；
        // 承接后另要 已用 + 承接日均 × 30 在上限 80% 内：已用 1500 GB、日均 10 GB（稳态 300 GB），承接 10 GB/天 → 稳态 600 GB 放得下，但 1500 + 300 = 1800 GB 超过 1600 GB
        let tail = Q::rolling(Some(2000e9), 1500e9, 10e9);
        assert_eq!(tail.projected(), 1500e9);
        assert_eq!(tail.headroom(), Some(500e9));
        assert!(tail.fits(3e9) && !tail.fits(4e9), "1500 + 3×30 = 1590 在 1600 内；1500 + 4×30 = 1620 超出");
        assert!(!Q::rolling(Some(2000e9), 1700e9, 0.0).fits(0.0), "已用本身就超过 80%");
        // 已用比稳态低（线路机刚上架 / 日均刚升上来）：照旧按稳态
        assert_eq!(Q::rolling(Some(2000e9), 100e9, 40e9).projected(), 1200e9);
    }
}
