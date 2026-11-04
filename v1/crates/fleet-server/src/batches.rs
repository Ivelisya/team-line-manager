//! 计费台账的口径：每个节点（计费名额 billing_slot，迁移 0097）从开始收费日起每满一个月是一期，每一期收一次钱，只有已支付 / 待支付两种状态（billing_paid，迁移 0100）。
//! 第 k 期 = [开始收费日加 k 个月, 加 k+1 个月)（meta::add_months，月底对齐：1-31 开始的是 2-28、3-31…）；停止收费的名额只收起点早于结束日的期。
//! 按月看 = 起点落在这个月的那一期：每个节点每个月正好一期（月底对齐也不会一个月两期）。
//! 「今天」按组织时区（默认北京时间）的日期。收钱与供应商订阅无关；只有新节点的开始收费日取订阅推出的购买日（ledger::purchase_date）。

use crate::meta;
use time::{Date, OffsetDateTime};

/// 今天（组织时区的日期）
pub fn today() -> Date { OffsetDateTime::now_utc().to_offset(meta::org_offset()).date() }

/// 这天所在月的 1 日
pub fn month_of(d: Date) -> Date { d.replace_day(1).unwrap_or(d) }

/// 本月 1 日
pub fn this_month() -> Date { month_of(today()) }

/// `2026-10-31`
pub fn fmt_date(d: Date) -> String { format!("{:04}-{:02}-{:02}", d.year(), u8::from(d.month()), d.day()) }

/// `10 月 31 日`
pub fn fmt_cn(d: Date) -> String { format!("{} 月 {} 日", u8::from(d.month()), d.day()) }

/// `2026-11`
pub fn fmt_month(m: Date) -> String { format!("{:04}-{:02}", m.year(), u8::from(m.month())) }

/// `2026-10-31` → 日期；格式不对给 None
pub fn parse_date(s: &str) -> Option<Date> {
    Date::parse(s.trim(), time::macros::format_description!("[year]-[month]-[day]")).ok()
}

/// `2026-11` → 该月 1 号；格式不对给 None
pub fn parse_month(s: &str) -> Option<Date> { parse_date(&format!("{}-01", s.trim())) }

/// 第 k 期的 [起点, 终点)；日期超出范围为 None
pub fn period(start: Date, k: i64) -> Option<(Date, Date)> { Some((meta::add_months(start, k)?, meta::add_months(start, k + 1)?)) }

fn month_no(d: Date) -> i64 { i64::from(d.year()) * 12 + i64::from(u8::from(d.month())) }

/// 起点落在 month（该月 1 日）那个月、要收费的那一期 [起点, 终点)：已经开始收费，且没停止或这期起点早于结束日；没有为 None
pub fn period_in(start: Date, end: Option<Date>, month: Date) -> Option<(Date, Date)> {
    let k = month_no(month) - month_no(start);
    if k < 0 {
        return None;
    }
    let (ps, pe) = period(start, k)?;
    end.is_none_or(|e| ps < e).then_some((ps, pe))
}

/// 停止收费的名额最后收费的那一期的最后一天（「收费到 11 月 29 日」）；结束日不晚于开始收费日（没有产生收费期次）为 None
pub fn billed_until(start: Date, end: Date) -> Option<Date> {
    if end <= start {
        return None;
    }
    let mut k = 0;
    while meta::add_months(start, k + 1).is_some_and(|s| s < end) {
        k += 1;
    }
    period(start, k).and_then(|(_, e)| e.previous_day())
}

/// 第 period_start 是 start 的第几期（k，从 0 起）；不在 start 的期序列上为 None
pub fn period_no(start: Date, period_start: Date) -> Option<i64> {
    let k = month_no(period_start) - month_no(start);
    (k >= 0 && period(start, k)?.0 == period_start).then_some(k)
}

/// 开始收费日最晚填到今天加一个月（再晚的期超出能翻看的月份）
pub fn latest_start(today: Date) -> Date { meta::add_months(today, 1).unwrap_or(today) }

/// 可以翻看的月份：最早的开始收费月（没有或晚于本月就从本月）→ 下个月（开始收费日最晚是今天加一个月，落在下个月）
pub fn months(starts: impl IntoIterator<Item = Date>, this: Date) -> Vec<Date> {
    let first = starts.into_iter().map(month_of).fold(this, Date::min);
    let last = meta::add_months(this, 1).unwrap_or(this);
    let mut out = vec![];
    let mut m = first;
    while m <= last {
        out.push(m);
        match meta::add_months(m, 1) {
            Some(n) => m = n,
            None => break,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::date;

    #[test]
    fn period_starting_in_month() {
        let (oct, nov) = (date!(2026 - 10 - 01), date!(2026 - 11 - 01));
        // 10-01 开始：10 月这期 10-01 – 10-31，11 月这期 11-01 – 11-30；9 月没有
        assert_eq!(period_in(oct, None, oct), Some((oct, nov)));
        assert_eq!(period_in(oct, None, nov), Some((nov, date!(2026 - 12 - 01))));
        assert_eq!(period_in(oct, None, date!(2026 - 09 - 01)), None);
        // 月中加入（10-30）：10 月就有一期 10-30 – 11-29，11 月是 11-30 起的那期
        assert_eq!(period_in(date!(2026 - 10 - 30), None, oct), Some((date!(2026 - 10 - 30), date!(2026 - 11 - 30))));
        assert_eq!(period_in(date!(2026 - 10 - 30), None, nov), Some((date!(2026 - 11 - 30), date!(2026 - 12 - 30))));
        // 月底对齐：1-31 开始的，2 月那期从 2-28 起，3 月那期从 3-31 起
        assert_eq!(period_in(date!(2026 - 01 - 31), None, date!(2026 - 02 - 01)), Some((date!(2026 - 02 - 28), date!(2026 - 03 - 31))));
        assert_eq!(period_in(date!(2026 - 01 - 31), None, date!(2026 - 03 - 01)), Some((date!(2026 - 03 - 31), date!(2026 - 04 - 30))));
        // 停止收费（11-14）：11-01 起的那期照收，12 月没有；结束日正好是期起点（11-01）那期不收
        assert!(period_in(oct, Some(date!(2026 - 11 - 14)), nov).is_some() && period_in(oct, Some(date!(2026 - 11 - 14)), date!(2026 - 12 - 01)).is_none());
        assert_eq!(period_in(oct, Some(nov), nov), None);
        // 结束日不晚于开始收费日：一期都没有
        assert_eq!(period_in(date!(2026 - 10 - 20), Some(date!(2026 - 10 - 15)), oct), None);
    }

    #[test]
    fn period_numbers() {
        let s = date!(2026 - 01 - 31);
        assert_eq!(period_no(s, s), Some(0));
        assert_eq!(period_no(s, date!(2026 - 02 - 28)), Some(1));
        assert_eq!(period_no(s, date!(2026 - 03 - 31)), Some(2));
        // 不在期序列上、或早于开始收费日
        assert_eq!(period_no(s, date!(2026 - 02 - 27)), None);
        assert_eq!(period_no(s, date!(2025 - 12 - 31)), None);
    }

    #[test]
    fn billed_until_last_period() {
        assert_eq!(billed_until(date!(2026 - 08 - 15), date!(2026 - 10 - 15)), Some(date!(2026 - 10 - 14)));
        assert_eq!(billed_until(date!(2026 - 08 - 15), date!(2026 - 10 - 16)), Some(date!(2026 - 11 - 14)));
        assert_eq!(billed_until(date!(2026 - 10 - 01), date!(2026 - 10 - 01)), None);
    }

    #[test]
    fn month_range() {
        let (nov, dec) = (date!(2026 - 11 - 01), date!(2026 - 12 - 01));
        // 本月 11 月：最早 10 月开始 → 10、11、12 月（下个月）；没有名额 / 最早的在下个月之后 → 本月和下个月（一定含本月）
        assert_eq!(months([date!(2026 - 10 - 30), date!(2026 - 10 - 01), date!(2026 - 12 - 05)], nov), [date!(2026 - 10 - 01), nov, dec]);
        assert_eq!(months([], nov), [nov, dec]);
        assert_eq!(months([date!(2026 - 12 - 05)], nov), [nov, dec]);
        assert_eq!(fmt_month(dec), "2026-12");
        assert_eq!(latest_start(date!(2026 - 01 - 31)), date!(2026 - 02 - 28));
        assert_eq!((parse_month("2026-12"), parse_month("2026-13")), (Some(dec), None));
    }
}
