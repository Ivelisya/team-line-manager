//! 计费批次的派生口径：管理端的批次视图、月度应收与客户页（公开）都从计费名额（billing_slot，迁移 0097）算，前端只显示。
//! 名额有起算日与结束日（null = 没结束）；同一团队里 (起算日, 地区) 相同的名额是一批，批次编号按 (起算日, 地区) 排序从 1 起。
//! 期 k = [起算日加 k 个月, 起算日加 k+1 个月)（meta::add_months，月底对齐）；名额在期 k 计费 ⇔ 期 k 起点 < 结束日。
//! 「今天」按组织时区（默认北京时间）的日期。与供应商订阅（subscription.due_date）完全无关。

use crate::meta;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use time::{Date, OffsetDateTime};

/// 派生用的名额：起算日、结束日、地区
#[derive(Debug, Clone)]
pub struct Slot {
    pub start: Date,
    pub end: Option<Date>,
    pub region: String,
}

/// 批次的一期
#[derive(Debug, Clone)]
pub struct Period {
    pub start: Date,
    pub end: Date,
    pub count: i32,
}

/// 一个批次与它的各期（按 start 升序，从第 0 期到最后一期）
#[derive(Debug, Clone)]
pub struct Batch {
    /// `2026-10-01|美区`
    pub key: String,
    pub seq: i32,
    pub region: String,
    pub periods: Vec<Period>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// 终点早于今天
    Expired,
    /// 终点就是今天（今天还算有效）
    Today,
    Active,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Expired => "expired",
            State::Today => "today",
            State::Active => "active",
        }
    }
}

/// 从各期派生出来的字段
#[derive(Debug, Clone)]
pub struct Derived {
    pub start: Date,
    /// 最后一期的终点：下次续费日 / 到期日
    pub end: Date,
    /// 最后一期的个数
    pub count: i32,
    /// 节点月 = Σ 各期个数
    pub node_months: i64,
    pub renewals: usize,
    pub state: State,
    pub summary: String,
}

/// 今天（组织时区的日期）
pub fn today() -> Date { OffsetDateTime::now_utc().to_offset(meta::org_offset()).date() }

/// `2026-10-31`
pub fn fmt_date(d: Date) -> String { format!("{:04}-{:02}-{:02}", d.year(), u8::from(d.month()), d.day()) }

/// `10-31`
pub fn fmt_md(d: Date) -> String { format!("{:02}-{:02}", u8::from(d.month()), d.day()) }

/// `2026-10-31` → 日期；格式不对给 None
pub fn parse_date(s: &str) -> Option<Date> {
    Date::parse(s.trim(), time::macros::format_description!("[year]-[month]-[day]")).ok()
}

/// `2026-11` → 该月 1 号；格式不对给 None
pub fn parse_month(s: &str) -> Option<Date> { parse_date(&format!("{}-01", s.trim())) }

/// 批次键：`2026-10-01|美区`
pub fn key(start: Date, region: &str) -> String { format!("{}|{region}", fmt_date(start)) }

/// 第 k 期的 [起点, 终点)；日期超出范围为 None
pub fn period(start: Date, k: i64) -> Option<(Date, Date)> { Some((meta::add_months(start, k)?, meta::add_months(start, k + 1)?)) }

/// 名额在第 k 期计费：期起点早于结束日（没结束的一直计）
pub fn billed(s: &Slot, k: i64) -> bool {
    match (s.end, meta::add_months(s.start, k)) {
        (_, None) => false,
        (None, Some(_)) => true,
        (Some(e), Some(ps)) => ps < e,
    }
}

/// 名额最后计费的那一期的最后一天（「算到 11-14」）；结束日不晚于起算日（一期都没计）为 None
pub fn billed_until(start: Date, end: Date) -> Option<Date> {
    let k = last_billed(start, end)?;
    period(start, k).map(|(_, e)| e.previous_day().unwrap_or(e))
}

/// 结束日为 end 时最后计费的那一期：起点早于 end 的最后一期；一期都没有为 None
fn last_billed(start: Date, end: Date) -> Option<i64> {
    if end <= start {
        return None;
    }
    let mut k = 0;
    while meta::add_months(start, k + 1).is_some_and(|s| s < end) {
        k += 1;
    }
    Some(k)
}

/// 批次的最后一期：还有没结束的名额（含空出待定）→ 含今天的那一期（今天 ≥ 起点的最后一期，起算日在今天之后的为第 0 期）；
/// 全部结束 → 含 max(结束日) 前一天的那一期（最后一个计费的期）；一期都没计过为 None
pub fn last_period(start: Date, ends: &[Option<Date>], today: Date) -> Option<i64> {
    if ends.iter().any(Option::is_none) {
        let mut k = 0;
        while meta::add_months(start, k + 1).is_some_and(|s| s <= today) {
            k += 1;
        }
        return Some(k);
    }
    last_billed(start, ends.iter().flatten().max().copied()?)
}

/// 按 (起算日, 地区) 分批、编号（一期都没计过的批次不出现、不占编号），每批从第 0 期到最后一期，每期个数 = 该期计费的名额数
pub fn derive_batches(slots: &[Slot], today: Date) -> Vec<Batch> {
    let mut groups: BTreeMap<(Date, &str), Vec<&Slot>> = BTreeMap::new();
    for s in slots {
        groups.entry((s.start, s.region.as_str())).or_default().push(s);
    }
    let mut out = vec![];
    for ((start, region), ss) in groups {
        let ends: Vec<Option<Date>> = ss.iter().map(|s| s.end).collect();
        let Some(last) = last_period(start, &ends, today) else { continue };
        let periods = (0..=last)
            .filter_map(|k| {
                let (ps, pe) = period(start, k)?;
                Some(Period { start: ps, end: pe, count: ss.iter().filter(|s| billed(s, k)).count() as i32 })
            })
            .collect();
        out.push(Batch { key: key(start, region), seq: out.len() as i32 + 1, region: region.to_string(), periods });
    }
    out
}

/// 月度应收的一行：某批「起点落在这个月」的一期
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub seq: i32,
    pub region: String,
    pub start: Date,
    pub end: Date,
    pub count: i32,
}

/// 某自然月（month = 该月 1 号）的应收：各批起点落在这个月里的期（个数 > 0 的），按起点、编号排。
/// 不受批次「最后一期」限制：没结束的名额按一直续下去算，所以看下个月是预计数
pub fn receivable(slots: &[Slot], batches: &[Batch], month: Date) -> Vec<Line> {
    let Some(next) = meta::add_months(month, 1) else { return vec![] };
    let mut out = vec![];
    for b in batches {
        let Some(first) = b.periods.first() else { continue };
        let ss: Vec<&Slot> = slots.iter().filter(|s| s.start == first.start && s.region == b.region).collect();
        let mut k = 0;
        while let Some((ps, pe)) = period(first.start, k).filter(|(ps, _)| *ps < next) {
            if ps >= month {
                let count = ss.iter().filter(|s| billed(s, k)).count() as i32;
                if count > 0 {
                    out.push(Line { seq: b.seq, region: b.region.clone(), start: ps, end: pe, count });
                }
            }
            k += 1;
        }
    }
    out.sort_by_key(|l| (l.start, l.seq));
    out
}

/// 某天有效的名额数：已起算（起算日 ≤ 这天）且没结束（结束日晚于这天）
pub fn active_on(slots: &[Slot], d: Date) -> usize { slots.iter().filter(|s| s.start <= d && s.end.is_none_or(|e| d < e)).count() }

/// 自动说明：只有一期 → ""；多期个数都一样 → 「含N次续期」；个数有变 → 「初始5个 → 续期4个」，
/// 多次变化按顺序串起来、相邻相同的合并（5、5、4 是「初始5个 → 续期4个」）
pub fn summary(counts: &[i32]) -> String {
    let Some(&first) = counts.first() else { return String::new() };
    if counts.len() == 1 {
        return String::new();
    }
    if counts.iter().all(|&c| c == first) {
        return format!("含{}次续期", counts.len() - 1);
    }
    let mut out = vec![format!("初始{first}个")];
    let mut prev = first;
    for &c in &counts[1..] {
        if c != prev {
            out.push(format!("续期{c}个"));
            prev = c;
        }
    }
    out.join(" → ")
}

/// 标题：有效的「第05批 · 美区 · 当前有效4个」，已到期的「第01批 · 美区 · 原4个（已到期）」
pub fn title(seq: i32, region: &str, count: i32, state: State) -> String {
    if state == State::Expired {
        format!("第{seq:02}批 · {region} · 原{count}个（已到期）")
    } else {
        format!("第{seq:02}批 · {region} · 当前有效{count}个")
    }
}

/// 派生：没有期时给 None
pub fn derive(b: &Batch, today: Date) -> Option<Derived> {
    // 各期已按起点升序
    let (first, last) = (b.periods.first()?, b.periods.last()?);
    let (start, end) = (first.start, last.end);
    let counts: Vec<i32> = b.periods.iter().map(|p| p.count).collect();
    let state = match end.cmp(&today) {
        std::cmp::Ordering::Less => State::Expired,
        std::cmp::Ordering::Equal => State::Today,
        std::cmp::Ordering::Greater => State::Active,
    };
    Some(Derived {
        start,
        end,
        count: last.count,
        node_months: b.periods.iter().map(|p| i64::from(p.count)).sum(),
        renewals: b.periods.len() - 1,
        state,
        summary: summary(&counts),
    })
}

fn periods_json(b: &Batch) -> Vec<Value> {
    b.periods.iter().map(|p| json!({"start_date": fmt_date(p.start), "end_date": fmt_date(p.end), "months": 1, "count": p.count})).collect()
}

/// 管理端的 Batch（含已到期的）
pub fn admin_json(b: &Batch, d: &Derived) -> Value {
    json!({
        "key": b.key, "seq": b.seq, "region": b.region, "summary": d.summary,
        "title": title(b.seq, &b.region, d.count, d.state),
        "start_date": fmt_date(d.start), "end_date": fmt_date(d.end),
        "count": d.count, "months": b.periods.len(), "node_months": d.node_months, "renewals": d.renewals, "state": d.state.as_str(),
        "periods": periods_json(b),
    })
}

/// 客户页的 PublicBatch：说明行 desc = 自动说明（不出现节点名）
pub fn public_json(b: &Batch, d: &Derived) -> Value {
    json!({
        "seq": b.seq, "region": b.region, "title": title(b.seq, &b.region, d.count, d.state), "desc": d.summary,
        "start_date": fmt_date(d.start), "end_date": fmt_date(d.end),
        "count": d.count, "months": b.periods.len(), "node_months": d.node_months, "state": d.state.as_str(),
        "periods": periods_json(b),
    })
}

/// 客户页整份数据：只含没到期的批次（按 seq 升序，调用方已排好），地区按批次里首次出现的顺序
pub fn public_page(team: &str, today: Date, batches: &[Batch]) -> Value {
    let live: Vec<(&Batch, Derived)> = batches.iter().filter_map(|b| derive(b, today).map(|d| (b, d))).filter(|(_, d)| d.state != State::Expired).collect();
    // (地区, 各批次个数, 今日到期个数)
    let mut regions: Vec<(&str, Vec<i32>, i32)> = Vec::new();
    for (b, d) in &live {
        let i = match regions.iter().position(|r| r.0 == b.region) {
            Some(i) => i,
            None => {
                regions.push((&b.region, vec![], 0));
                regions.len() - 1
            }
        };
        regions[i].1.push(d.count);
        if d.state == State::Today {
            regions[i].2 += d.count;
        }
    }
    json!({
        "team": team, "today": fmt_date(today),
        "regions": regions.iter().map(|(r, counts, td)| json!({"region": r, "counts": counts, "total": counts.iter().sum::<i32>(), "today_count": td})).collect::<Vec<_>>(),
        "node_months": live.iter().map(|(_, d)| d.node_months).sum::<i64>(),
        "batches": live.iter().map(|(b, d)| public_json(b, d)).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::date;

    fn s(start: Date, end: Option<Date>, region: &str) -> Slot { Slot { start, end, region: region.into() } }

    fn shape(b: &Batch) -> Vec<(Date, Date, i32)> { b.periods.iter().map(|p| (p.start, p.end, p.count)).collect() }

    #[test]
    fn summary_wording() {
        assert_eq!(summary(&[5]), "");
        assert_eq!(summary(&[5, 5]), "含1次续期");
        assert_eq!(summary(&[6, 6, 6]), "含2次续期");
        assert_eq!(summary(&[5, 4]), "初始5个 → 续期4个");
        assert_eq!(summary(&[6, 5, 4]), "初始6个 → 续期5个 → 续期4个");
        assert_eq!(summary(&[5, 5, 4, 4]), "初始5个 → 续期4个");
    }

    #[test]
    fn titles_pad_seq_and_mark_expired() {
        assert_eq!(title(5, "美区", 4, State::Active), "第05批 · 美区 · 当前有效4个");
        assert_eq!(title(1, "美区", 4, State::Expired), "第01批 · 美区 · 原4个（已到期）");
    }

    #[test]
    fn add_months_clamps_to_month_end() {
        assert_eq!(meta::add_months(date!(2026 - 01 - 31), 1), Some(date!(2026 - 02 - 28)));
        assert_eq!(meta::add_months(date!(2028 - 01 - 31), 1), Some(date!(2028 - 02 - 29)));
        assert_eq!(meta::add_months(date!(2026 - 11 - 30), 3), Some(date!(2027 - 02 - 28)));
    }

    #[test]
    fn open_batch_runs_to_period_containing_today() {
        // 10 个 10-01 起算，其中 1 个 10-20 结束（10 月照算）、1 个 11-01 结束（正好落在第 1 期起点：第 1 期不算）、1 个 11-02 结束（第 1 期照算）
        let mut ss: Vec<Slot> = (0..7).map(|_| s(date!(2026 - 10 - 01), None, "美区")).collect();
        ss.push(s(date!(2026 - 10 - 01), Some(date!(2026 - 10 - 20)), "美区"));
        ss.push(s(date!(2026 - 10 - 01), Some(date!(2026 - 11 - 01)), "美区"));
        ss.push(s(date!(2026 - 10 - 01), Some(date!(2026 - 11 - 02)), "美区"));
        let bs = derive_batches(&ss, date!(2026 - 11 - 15));
        assert_eq!(bs.len(), 1);
        assert_eq!(shape(&bs[0]), [(date!(2026 - 10 - 01), date!(2026 - 11 - 01), 10), (date!(2026 - 11 - 01), date!(2026 - 12 - 01), 8)]);
        let d = derive(&bs[0], date!(2026 - 11 - 15)).unwrap();
        assert_eq!((d.end, d.count, bs[0].periods.len(), d.node_months, d.renewals, d.state), (date!(2026 - 12 - 01), 8, 2, 18, 1, State::Active));
        assert_eq!(d.summary, "初始10个 → 续期8个");
        // 今天正好是期起点：属于新的一期
        assert_eq!(derive_batches(&ss, date!(2026 - 12 - 01))[0].periods.len(), 3);
        // 今天在起算日之前（手动名额填了以后的日子）：只有第 0 期
        assert_eq!(derive_batches(&ss, date!(2026 - 09 - 20))[0].periods.len(), 1);
        assert_eq!(bs[0].key, "2026-10-01|美区");
    }

    #[test]
    fn month_end_alignment() {
        // 1-31 起算：2-28 → 3-31 → 4-30，不漂到 28 号
        let ss = vec![s(date!(2026 - 01 - 31), None, "美区")];
        let b = &derive_batches(&ss, date!(2026 - 04 - 10))[0];
        assert_eq!(
            shape(b),
            [(date!(2026 - 01 - 31), date!(2026 - 02 - 28), 1), (date!(2026 - 02 - 28), date!(2026 - 03 - 31), 1), (date!(2026 - 03 - 31), date!(2026 - 04 - 30), 1)]
        );
    }

    #[test]
    fn fully_ended_batch_stops_at_last_billed_period() {
        // 都结束了：最后一期 = 含 max(结束日) 前一天的那一期，之后不再续
        let ss = vec![s(date!(2026 - 08 - 15), Some(date!(2026 - 09 - 20)), "墨西哥"), s(date!(2026 - 08 - 15), Some(date!(2026 - 10 - 15)), "墨西哥")];
        let b = &derive_batches(&ss, date!(2026 - 12 - 01))[0];
        assert_eq!(shape(b), [(date!(2026 - 08 - 15), date!(2026 - 09 - 15), 2), (date!(2026 - 09 - 15), date!(2026 - 10 - 15), 2)]);
        let d = derive(b, date!(2026 - 12 - 01)).unwrap();
        assert_eq!((d.end, d.count, d.state), (date!(2026 - 10 - 15), 2, State::Expired));
        assert_eq!(derive(b, date!(2026 - 10 - 15)).unwrap().state, State::Today);
        assert_eq!(billed_until(date!(2026 - 08 - 15), date!(2026 - 10 - 15)), Some(date!(2026 - 10 - 14)));
        assert_eq!(billed_until(date!(2026 - 08 - 15), date!(2026 - 10 - 16)), Some(date!(2026 - 11 - 14)));
        // 结束日不晚于起算日：一期都不算，批次不出现、不占编号
        let ss = vec![s(date!(2026 - 10 - 01), Some(date!(2026 - 10 - 01)), "美区"), s(date!(2026 - 10 - 30), None, "美区")];
        let bs = derive_batches(&ss, date!(2026 - 10 - 31));
        assert_eq!((bs.len(), bs[0].seq, bs[0].key.as_str()), (1, 1, "2026-10-30|美区"));
        assert_eq!(billed_until(date!(2026 - 10 - 01), date!(2026 - 10 - 01)), None);
    }

    #[test]
    fn batches_numbered_by_start_then_region() {
        let ss = vec![s(date!(2026 - 10 - 30), None, "美区"), s(date!(2026 - 10 - 01), None, "美区"), s(date!(2026 - 10 - 01), None, "墨西哥"), s(date!(2026 - 10 - 01), None, "美区")];
        let bs = derive_batches(&ss, date!(2026 - 10 - 31));
        let got: Vec<(i32, &str, i32)> = bs.iter().map(|b| (b.seq, b.key.as_str(), b.periods[0].count)).collect();
        assert_eq!(got, [(1, "2026-10-01|墨西哥", 1), (2, "2026-10-01|美区", 2), (3, "2026-10-30|美区", 1)]);
    }

    #[test]
    fn receivable_lists_periods_starting_in_month() {
        let mut ss: Vec<Slot> = (0..9).map(|_| s(date!(2026 - 10 - 01), None, "美区")).collect();
        ss.push(s(date!(2026 - 10 - 30), None, "美区"));
        ss.push(s(date!(2026 - 10 - 01), Some(date!(2026 - 11 - 14)), "美区"));
        let today = date!(2026 - 10 - 31);
        let bs = derive_batches(&ss, today);
        let l = |seq, start, end, count| Line { seq, region: "美区".into(), start, end, count };
        assert_eq!(receivable(&ss, &bs, date!(2026 - 10 - 01)), [l(1, date!(2026 - 10 - 01), date!(2026 - 11 - 01), 10), l(2, date!(2026 - 10 - 30), date!(2026 - 11 - 30), 1)]);
        // 下个月：预计数（没结束的照续，11-14 结束的那个在 11-01 那期照算）
        assert_eq!(receivable(&ss, &bs, date!(2026 - 11 - 01)), [l(1, date!(2026 - 11 - 01), date!(2026 - 12 - 01), 10), l(2, date!(2026 - 11 - 30), date!(2026 - 12 - 30), 1)]);
        assert_eq!(receivable(&ss, &bs, date!(2026 - 12 - 01))[0].count, 9);
        // 月初 / 月末有效个数
        assert_eq!((active_on(&ss, date!(2026 - 10 - 01)), active_on(&ss, today)), (10, 11));
        assert_eq!(active_on(&ss, date!(2026 - 11 - 14)), 10);
        assert!(receivable(&ss, &bs, date!(2026 - 09 - 01)).is_empty());
    }

    #[test]
    fn public_page_hides_expired_and_groups_regions() {
        let today = date!(2026 - 10 - 31);
        let ss = vec![
            s(date!(2026 - 07 - 01), Some(date!(2026 - 07 - 20)), "美区"),
            s(date!(2026 - 10 - 01), None, "美区"),
            s(date!(2026 - 10 - 01), None, "美区"),
            s(date!(2026 - 10 - 05), None, "墨西哥"),
        ];
        let bs = derive_batches(&ss, today);
        let v = public_page("团队2", today, &bs);
        let seqs: Vec<i64> = v["batches"].as_array().unwrap().iter().map(|b| b["seq"].as_i64().unwrap()).collect();
        assert_eq!(seqs, [2, 3], "第01批已到期不出现");
        assert_eq!(v["regions"], json!([{"region": "美区", "counts": [2], "total": 2, "today_count": 0}, {"region": "墨西哥", "counts": [1], "total": 1, "today_count": 0}]));
        assert_eq!(v["node_months"], 3);
        assert_eq!(v["batches"][0]["title"], "第02批 · 美区 · 当前有效2个");
        assert_eq!(v["batches"][0]["end_date"], "2026-11-01");
        assert!(v["batches"][0].get("key").is_none() && v["batches"][0].get("note").is_none());
    }
}
