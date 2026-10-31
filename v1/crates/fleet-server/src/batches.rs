//! 客户批次账的派生口径：管理端的批次视图与客户页（公开）的视图都从这里算，前端只显示。
//! 批次 = 首购一段 + 每次续期一段，段与段首尾相接；起止、个数、时长、节点月、状态、自动说明、标题全部由段派生，库里不存。
//! 「今天」按组织时区（默认北京时间）的日期。与供应商订阅（subscription.due_date）完全无关。

use crate::meta;
use serde_json::{json, Value};
use time::{Date, OffsetDateTime};
use uuid::Uuid;

/// 批次的一段（首购或一次续期）
#[derive(Debug, Clone)]
pub struct Period {
    pub id: Uuid,
    pub start: Date,
    pub end: Date,
    /// 计费月数：节点月 = count × months
    pub months: i32,
    pub count: i32,
}

/// 一个批次与它的全部段（按 start 升序）
#[derive(Debug, Clone)]
pub struct Batch {
    pub id: Uuid,
    pub seq: i32,
    pub region: String,
    pub note: String,
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

/// 从段派生出来的字段
#[derive(Debug, Clone)]
pub struct Derived {
    pub start: Date,
    pub end: Date,
    /// 最后一段（按起点）的个数
    pub count: i32,
    /// 各段月数之和
    pub months: i32,
    pub node_months: i64,
    pub renewals: usize,
    pub state: State,
    pub summary: String,
}

/// 今天（组织时区的日期）
pub fn today() -> Date { OffsetDateTime::now_utc().to_offset(meta::org_offset()).date() }

/// `2026-10-31`
pub fn fmt_date(d: Date) -> String { format!("{:04}-{:02}-{:02}", d.year(), u8::from(d.month()), d.day()) }

/// `2026-10-31` → 日期；格式不对给 None
pub fn parse_date(s: &str) -> Option<Date> {
    Date::parse(s.trim(), time::macros::format_description!("[year]-[month]-[day]")).ok()
}

/// 自动说明：只有一段 → ""；多段个数都一样 → 「含N次续期」；个数有变 → 「初始5个 → 续期4个」，
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

/// 客户页说明行：自动说明与备注用「；」接起来，哪个空就只写另一个
pub fn desc(summary: &str, note: &str) -> String {
    match (summary.is_empty(), note.is_empty()) {
        (false, false) => format!("{summary}；{note}"),
        (false, true) => summary.to_string(),
        _ => note.to_string(),
    }
}

/// 标题：有效的「第05批 · 美区 · 当前有效4个」，已到期的「第01批 · 美区 · 原4个（已到期）」
pub fn title(seq: i32, region: &str, count: i32, state: State) -> String {
    if state == State::Expired {
        format!("第{seq:02}批 · {region} · 原{count}个（已到期）")
    } else {
        format!("第{seq:02}批 · {region} · 当前有效{count}个")
    }
}

/// 两段重叠（首尾相接不算）
pub fn overlaps(a: &Period, b: &Period) -> bool { a.start < b.end && b.start < a.end }

/// 派生：没有段时给 None
pub fn derive(b: &Batch, today: Date) -> Option<Derived> {
    let last = b.periods.iter().max_by_key(|p| p.start)?;
    let start = b.periods.iter().map(|p| p.start).min()?;
    let end = b.periods.iter().map(|p| p.end).max()?;
    let counts: Vec<i32> = {
        let mut ps: Vec<&Period> = b.periods.iter().collect();
        ps.sort_by_key(|p| p.start);
        ps.iter().map(|p| p.count).collect()
    };
    let state = match end.cmp(&today) {
        std::cmp::Ordering::Less => State::Expired,
        std::cmp::Ordering::Equal => State::Today,
        std::cmp::Ordering::Greater => State::Active,
    };
    Some(Derived {
        start,
        end,
        count: last.count,
        months: b.periods.iter().map(|p| p.months).sum(),
        node_months: b.periods.iter().map(|p| i64::from(p.count) * i64::from(p.months)).sum(),
        renewals: b.periods.len() - 1,
        state,
        summary: summary(&counts),
    })
}

/// 管理端的 Batch（含 id、备注、已到期的）
pub fn admin_json(b: &Batch, d: &Derived) -> Value {
    json!({
        "id": b.id, "seq": b.seq, "region": b.region, "note": b.note, "summary": d.summary,
        "title": title(b.seq, &b.region, d.count, d.state),
        "start_date": fmt_date(d.start), "end_date": fmt_date(d.end),
        "count": d.count, "months": d.months, "node_months": d.node_months, "renewals": d.renewals, "state": d.state.as_str(),
        "periods": b.periods.iter().map(|p| json!({"id": p.id, "start_date": fmt_date(p.start), "end_date": fmt_date(p.end), "months": p.months, "count": p.count})).collect::<Vec<_>>(),
    })
}

/// 客户页的 PublicBatch：不给 id、备注原文，说明行合成 desc
pub fn public_json(b: &Batch, d: &Derived) -> Value {
    json!({
        "seq": b.seq, "region": b.region, "title": title(b.seq, &b.region, d.count, d.state), "desc": desc(&d.summary, &b.note),
        "start_date": fmt_date(d.start), "end_date": fmt_date(d.end),
        "count": d.count, "months": d.months, "node_months": d.node_months, "state": d.state.as_str(),
        "periods": b.periods.iter().map(|p| json!({"start_date": fmt_date(p.start), "end_date": fmt_date(p.end), "months": p.months, "count": p.count})).collect::<Vec<_>>(),
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

    fn p(start: Date, end: Date, months: i32, count: i32) -> Period { Period { id: Uuid::now_v7(), start, end, months, count } }

    fn batch(seq: i32, region: &str, note: &str, periods: Vec<Period>) -> Batch { Batch { id: Uuid::now_v7(), seq, region: region.into(), note: note.into(), periods } }

    #[test]
    fn summary_wording() {
        assert_eq!(summary(&[5]), "");
        assert_eq!(summary(&[5, 5]), "含1次续期");
        assert_eq!(summary(&[6, 6, 6]), "含2次续期");
        assert_eq!(summary(&[5, 4]), "初始5个 → 续期4个");
        assert_eq!(summary(&[6, 5, 4]), "初始6个 → 续期5个 → 续期4个");
        // 相邻相同的合并
        assert_eq!(summary(&[5, 5, 4, 4]), "初始5个 → 续期4个");
        assert_eq!(summary(&[5, 4, 5]), "初始5个 → 续期4个 → 续期5个");
    }

    #[test]
    fn desc_joins_summary_and_note() {
        assert_eq!(desc("初始5个 → 续期4个", "另1个归团队1"), "初始5个 → 续期4个；另1个归团队1");
        assert_eq!(desc("", "首批4个统一到期"), "首批4个统一到期");
        assert_eq!(desc("含1次续期", ""), "含1次续期");
        assert_eq!(desc("", ""), "");
    }

    #[test]
    fn titles_pad_seq_and_mark_expired() {
        assert_eq!(title(5, "美区", 4, State::Active), "第05批 · 美区 · 当前有效4个");
        assert_eq!(title(5, "美区", 4, State::Today), "第05批 · 美区 · 当前有效4个");
        assert_eq!(title(1, "美区", 4, State::Expired), "第01批 · 美区 · 原4个（已到期）");
        assert_eq!(title(123, "墨西哥", 1, State::Active), "第123批 · 墨西哥 · 当前有效1个");
    }

    #[test]
    fn derive_totals_and_state() {
        // 第05批：9/15→10/15 1月5个，10/15→11/14 1月4个
        let b = batch(5, "美区", "另1个归团队1", vec![p(date!(2026 - 09 - 15), date!(2026 - 10 - 15), 1, 5), p(date!(2026 - 10 - 15), date!(2026 - 11 - 14), 1, 4)]);
        let d = derive(&b, date!(2026 - 10 - 21)).unwrap();
        assert_eq!((d.start, d.end, d.count, d.months, d.node_months, d.renewals), (date!(2026 - 09 - 15), date!(2026 - 11 - 14), 4, 2, 9, 1));
        assert_eq!((d.state, d.summary.as_str()), (State::Active, "初始5个 → 续期4个"));
        // 终点就是今天仍有效，次日到期
        assert_eq!(derive(&b, date!(2026 - 11 - 14)).unwrap().state, State::Today);
        assert_eq!(derive(&b, date!(2026 - 11 - 15)).unwrap().state, State::Expired);
        // 段乱序也按起点取最后一段
        let mut shuffled = b.clone();
        shuffled.periods.reverse();
        let d = derive(&shuffled, date!(2026 - 10 - 21)).unwrap();
        assert_eq!((d.count, d.summary.as_str()), (4, "初始5个 → 续期4个"));
        assert!(derive(&batch(1, "美区", "", vec![]), date!(2026 - 10 - 21)).is_none());
    }

    #[test]
    fn add_months_clamps_to_month_end() {
        // 建批次 / 续期默认终点用的是 meta::add_months：月底对齐
        assert_eq!(meta::add_months(date!(2026 - 01 - 31), 1), Some(date!(2026 - 02 - 28)));
        assert_eq!(meta::add_months(date!(2028 - 01 - 31), 1), Some(date!(2028 - 02 - 29)));
        assert_eq!(meta::add_months(date!(2026 - 09 - 15), 1), Some(date!(2026 - 10 - 15)));
        assert_eq!(meta::add_months(date!(2026 - 11 - 30), 3), Some(date!(2027 - 02 - 28)));
    }

    #[test]
    fn overlap_excludes_touching_periods() {
        let a = p(date!(2026 - 09 - 15), date!(2026 - 10 - 15), 1, 5);
        let touch = p(date!(2026 - 10 - 15), date!(2026 - 11 - 14), 1, 5);
        let over = p(date!(2026 - 10 - 14), date!(2026 - 11 - 14), 1, 5);
        assert!(!overlaps(&a, &touch) && overlaps(&a, &over) && overlaps(&over, &a));
    }

    #[test]
    fn public_page_hides_expired_and_groups_regions() {
        let today = date!(2026 - 10 - 21);
        let bs = vec![
            batch(1, "美区", "", vec![p(date!(2026 - 07 - 01), date!(2026 - 08 - 01), 1, 4)]),
            batch(5, "美区", "另1个归团队1", vec![p(date!(2026 - 09 - 15), date!(2026 - 10 - 15), 1, 5), p(date!(2026 - 10 - 15), date!(2026 - 11 - 14), 1, 4)]),
            batch(7, "墨西哥", "", vec![p(date!(2026 - 09 - 22), date!(2026 - 10 - 21), 1, 1)]),
            batch(8, "美区", "", vec![p(date!(2026 - 09 - 22), date!(2026 - 10 - 21), 1, 5)]),
            batch(9, "菲律宾", "", vec![p(date!(2026 - 09 - 27), date!(2026 - 10 - 25), 1, 1)]),
        ];
        let v = public_page("团队2", today, &bs);
        assert_eq!(v["today"], "2026-10-21");
        let seqs: Vec<i64> = v["batches"].as_array().unwrap().iter().map(|b| b["seq"].as_i64().unwrap()).collect();
        assert_eq!(seqs, [5, 7, 8, 9], "第01批已到期不出现");
        assert_eq!(v["regions"], json!([
            {"region": "美区", "counts": [4, 5], "total": 9, "today_count": 5},
            {"region": "墨西哥", "counts": [1], "total": 1, "today_count": 1},
            {"region": "菲律宾", "counts": [1], "total": 1, "today_count": 0},
        ]));
        // 节点月只算显示出来的批次：9 + 1 + 5 + 1
        assert_eq!(v["node_months"], 16);
        assert_eq!(v["batches"][0]["desc"], "初始5个 → 续期4个；另1个归团队1");
        assert_eq!(v["batches"][0]["title"], "第05批 · 美区 · 当前有效4个");
        assert!(v["batches"][0].get("id").is_none() && v["batches"][0].get("note").is_none());
        // 全部到期：空列表
        let v = public_page("团队2", date!(2027 - 01 - 01), &bs);
        assert_eq!((v["batches"].as_array().unwrap().len(), v["node_months"].as_i64()), (0, Some(0)));
    }
}
