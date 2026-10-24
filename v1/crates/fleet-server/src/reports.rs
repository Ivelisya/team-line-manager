//! 报表：月度可用率汇总（`GET /reports/uptime`）与月报（`GET /reports/monthly`、`POST /reports/monthly/send`，每月 1 号自动发一次）。
//!
//! 可用率口径同 `GET /machines/{id}/availability`（api/machines.rs）：节点的全部入口（含已移除的）探测小时汇总 `probe_hourly` 里 tcp / egress 的
//! 成功次数 ÷ 探测次数。区别只有分天的时区：抽屉按机器的账单时区分天，这里一个窗口按组织时区的自然日整段加总，窗口边缘至多差几个小时。
//! 只列在用的节点（不含线路机：线路机上的探测是它到各节点的，不代表线路机自己；不含备用团队）。
//!
//! 断线：小时汇总里某个整点小时有 TCP 探测失败（成功 < 次数）就算断线的一小时，连着的小时合成一次；最长一次按小时算（原始探测只留几天，
//! 月度窗口只能用小时汇总）。近 30 天告警次数是此刻往前 30 天内这台机器开出的告警条数（不论是否已恢复）。
//!
//! 月报：总流量、月费合计、USD/GB 最高的 3 台、可用率最差的 3 台、告警次数前 3 的规则、即将到期的台数，
//! 走通知发送队列（`notify::enqueue`，`Kind::Report`），只发给开了「月报」的渠道。

use crate::db::audit;
use crate::error::{ApiError, ApiResult};
use crate::notify::{self, Kind, Notice};
use crate::state::AppState;
use crate::views::{self, Ref};
use crate::{meta, traffic};
use serde::Serialize;
use serde_json::{json, Value};
use std::cmp::Ordering;
use std::collections::HashMap;
use time::{Date, Duration, Month, OffsetDateTime};
use uuid::Uuid;

/// `days` 最多看多少天（同 availability）
pub const MAX_DAYS: i64 = 90;
/// 上一次自动发出的月报是哪个月（app_setting，值是 `YYYY-MM`）：同一个月只发一次
const SENT_KEY: &str = "monthly_report_sent";
/// 自动发送的操作人（审计里显示）
const ACTOR: &str = "系统";
/// USD/GB 排名：这个月流量不足这么多 GB 的不参与（几乎没流量的机器会排到无穷大）
const MIN_GB: f64 = 1.0;

/// 统计窗口：组织时区的自然日，`from`、`to` 都含
pub struct Window {
    /// 按月查时是 `YYYY-MM`
    pub month: Option<String>,
    pub days: i64,
    pub from: Date,
    pub to: Date,
}

async fn org_today(st: &AppState) -> sqlx::Result<Date> {
    sqlx::query_scalar("select (now() at time zone $1)::date").bind(meta::org_tz()).fetch_one(&st.db).await
}

fn month_str(d: Date) -> String { format!("{}-{:02}", d.year(), d.month() as u8) }

/// `YYYY-MM` → 当月 1 号
pub fn parse_month(s: &str) -> ApiResult<Date> {
    let bad = || ApiError::bad_request("month 写成 YYYY-MM，例如 2026-09");
    let (y, m) = s.trim().split_once('-').ok_or_else(bad)?;
    if y.len() != 4 || m.len() != 2 {
        return Err(bad());
    }
    let month = Month::try_from(m.parse::<u8>().map_err(|_| bad())?).map_err(|_| bad())?;
    Date::from_calendar_date(y.parse().map_err(|_| bad())?, month, 1).map_err(|_| bad())
}

fn next_month(first: Date) -> Date {
    let (y, m) = if first.month() == Month::December { (first.year() + 1, Month::January) } else { (first.year(), first.month().next()) };
    Date::from_calendar_date(y, m, 1).unwrap_or(first)
}

/// 窗口：`month`（`YYYY-MM`，本月算到今天）或 `days`（近几天含今天），都不给是本月；两个都给 400
pub async fn window(st: &AppState, month: Option<&str>, days: Option<i64>) -> ApiResult<Window> {
    let today = org_today(st).await?;
    match (month, days) {
        (Some(_), Some(_)) => Err(ApiError::bad_request("month 和 days 只能给一个")),
        (None, Some(n)) => {
            if !(1..=MAX_DAYS).contains(&n) {
                return Err(ApiError::bad_request(format!("days 只能是 1–{MAX_DAYS}")));
            }
            Ok(Window { month: None, days: n, from: today - Duration::days(n - 1), to: today })
        }
        (m, None) => {
            let first = match m {
                Some(m) => parse_month(m)?,
                None => today.replace_day(1).unwrap_or(today),
            };
            if first > today {
                return Err(ApiError::bad_request("还没到这个月"));
            }
            let to = (next_month(first) - Duration::days(1)).min(today);
            Ok(Window { month: Some(month_str(first)), days: (to - first).whole_days() + 1, from: first, to })
        }
    }
}

/// 上个月 1 号（组织时区）
async fn last_month(st: &AppState) -> sqlx::Result<Date> {
    let today = org_today(st).await?;
    let first = today.replace_day(1).unwrap_or(today);
    Ok((first - Duration::days(1)).replace_day(1).unwrap_or(first))
}

// ── 月度可用率 ──────────────────────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct NodeRow {
    id: Uuid,
    name: String,
    team_id: Option<Uuid>,
    team_name: Option<String>,
    provider_id: Option<Uuid>,
    provider_name: Option<String>,
    tcp_total: i64,
    tcp_ok: i64,
    egress_total: i64,
    egress_ok: i64,
}

#[derive(Serialize)]
struct Item {
    id: Uuid,
    name: String,
    team: Option<Ref>,
    provider: Option<Ref>,
    tcp_total: i64,
    tcp_ok: i64,
    /// 0–100；窗口内没有探测为 null
    tcp_pct: Option<f64>,
    egress_total: i64,
    egress_ok: i64,
    egress_pct: Option<f64>,
    /// 断线次数：有 TCP 探测失败的整点小时，连着的合成一次
    outages: i64,
    /// 最长一次断线的小时数（没有断线为 0）
    longest_outage_h: i64,
    /// 近 30 天（此刻往前）这台机器开出的告警条数
    incidents_30d: i64,
}

#[derive(Serialize)]
struct ProviderRow {
    id: Option<Uuid>,
    name: String,
    nodes: i64,
    /// 窗口内有 TCP 探测的节点数（平均、最差只看它们）
    with_data: i64,
    avg_tcp_pct: Option<f64>,
    worst_tcp_pct: Option<f64>,
    /// TCP 成功率最差的那台
    worst: Option<Ref>,
    avg_egress_pct: Option<f64>,
    worst_egress_pct: Option<f64>,
    outages: i64,
    incidents_30d: i64,
}

fn pct(ok: i64, total: i64) -> Option<f64> { (total > 0).then(|| 100.0 * ok as f64 / total as f64) }

/// 升序，没有值的排最后
fn asc(a: Option<f64>, b: Option<f64>) -> Ordering {
    match (a, b) {
        (Some(x), Some(y)) => x.total_cmp(&y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn mean(v: impl Iterator<Item = f64>) -> Option<f64> {
    let (sum, n) = v.fold((0.0, 0u32), |(s, n), x| (s + x, n + 1));
    (n > 0).then(|| sum / n as f64)
}

/// 窗口内每个在用节点的可用率（TCP、出口成功率、断线、告警），再按供应商汇总。
/// 节点按 TCP 成功率从差到好排（没有探测的在最后）；供应商按平均 TCP 成功率从差到好排
pub async fn uptime(st: &AppState, w: &Window) -> ApiResult<Value> {
    let tz = meta::org_tz();
    let (from, to): (OffsetDateTime, OffsetDateTime) =
        sqlx::query_as("select $1::date::timestamp at time zone $3, ($2::date + 1)::timestamp at time zone $3").bind(w.from).bind(w.to).bind(&tz).fetch_one(&st.db).await?;
    let rows: Vec<NodeRow> = sqlx::query_as(
        "select m.id, display_name(m.kind::text, m.label, t.name, m.number) name, t.id team_id, t.name team_name,
                pv.id provider_id, pv.name provider_name,
                coalesce(sum(h.total) filter (where h.kind = 'tcp'), 0)::bigint tcp_total, coalesce(sum(h.ok) filter (where h.kind = 'tcp'), 0)::bigint tcp_ok,
                coalesce(sum(h.total) filter (where h.kind = 'egress'), 0)::bigint egress_total, coalesce(sum(h.ok) filter (where h.kind = 'egress'), 0)::bigint egress_ok
         from machine m left join team t on t.id = m.team_id left join plan pl on pl.id = m.plan_id left join provider pv on pv.id = coalesce(pl.provider_id, m.provider_id)
         left join entry e on e.node_id = m.id
         left join probe_hourly h on h.entry_id = e.id and h.kind in ('tcp', 'egress') and h.hour >= $1 and h.hour < $2
         where m.kind <> 'relay' and m.lifecycle = 'active' and m.recycled_at is null and not coalesce(t.spare, false)
         group by m.id, t.id, pv.id
         order by t.sort_order nulls first, t.name, m.number, m.label, m.id",
    )
    .bind(from)
    .bind(to)
    .fetch_all(&st.db)
    .await?;
    let ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
    // 断线：有 TCP 失败的整点小时，连续的小时合成一段（小时序号减去段内序号是常数）
    let islands: Vec<(Uuid, i64)> = sqlx::query_as(
        "with hrs as (
           select m.id, h.hour, sum(h.total) total, sum(h.ok) ok
           from machine m join entry e on e.node_id = m.id join probe_hourly h on h.entry_id = e.id
           where m.id = any($1) and h.kind = 'tcp' and h.hour >= $2 and h.hour < $3 group by m.id, h.hour),
         bad as (
           select id, (extract(epoch from hour) / 3600)::bigint - row_number() over (partition by id order by hour) grp from hrs where ok < total)
         select id, count(*)::bigint from bad group by id, grp",
    )
    .bind(&ids)
    .bind(from)
    .bind(to)
    .fetch_all(&st.db)
    .await?;
    let mut outage: HashMap<Uuid, (i64, i64)> = HashMap::new();
    for (id, len) in islands {
        let o = outage.entry(id).or_default();
        o.0 += 1;
        o.1 = o.1.max(len);
    }
    let incidents: HashMap<Uuid, i64> = sqlx::query_as::<_, (Uuid, i64)>(
        "select subject_id, count(*)::bigint from incident where subject_id = any($1) and opened_at >= now() - interval '30 days' group by 1",
    )
    .bind(&ids)
    .fetch_all(&st.db)
    .await?
    .into_iter()
    .collect();

    let mut items: Vec<Item> = rows
        .into_iter()
        .map(|r| {
            let (outages, longest_outage_h) = outage.get(&r.id).copied().unwrap_or_default();
            Item {
                tcp_pct: pct(r.tcp_ok, r.tcp_total),
                egress_pct: pct(r.egress_ok, r.egress_total),
                outages,
                longest_outage_h,
                incidents_30d: incidents.get(&r.id).copied().unwrap_or(0),
                team: r.team_id.map(|id| Ref { id, name: r.team_name.unwrap_or_default() }),
                provider: r.provider_id.map(|id| Ref { id, name: r.provider_name.unwrap_or_default() }),
                id: r.id,
                name: r.name,
                tcp_total: r.tcp_total,
                tcp_ok: r.tcp_ok,
                egress_total: r.egress_total,
                egress_ok: r.egress_ok,
            }
        })
        .collect();
    items.sort_by(|a, b| asc(a.tcp_pct, b.tcp_pct).then_with(|| asc(a.egress_pct, b.egress_pct)));

    let mut groups: Vec<(Option<Uuid>, String, Vec<&Item>)> = vec![];
    for it in &items {
        let (id, name) = it.provider.as_ref().map_or((None, "未填供应商".to_string()), |p| (Some(p.id), p.name.clone()));
        match groups.iter_mut().find(|g| g.0 == id) {
            Some(g) => g.2.push(it),
            None => groups.push((id, name, vec![it])),
        }
    }
    let mut providers: Vec<ProviderRow> = groups
        .into_iter()
        .map(|(id, name, its)| {
            let tcp: Vec<&&Item> = its.iter().filter(|i| i.tcp_pct.is_some()).collect();
            // its 已按 TCP 成功率从差到好，第一个有数据的就是最差的
            let worst = tcp.first().map(|i| Ref { id: i.id, name: i.name.clone() });
            ProviderRow {
                id,
                name,
                nodes: its.len() as i64,
                with_data: tcp.len() as i64,
                avg_tcp_pct: mean(tcp.iter().filter_map(|i| i.tcp_pct)),
                worst_tcp_pct: tcp.first().and_then(|i| i.tcp_pct),
                worst,
                avg_egress_pct: mean(its.iter().filter_map(|i| i.egress_pct)),
                worst_egress_pct: its.iter().filter_map(|i| i.egress_pct).min_by(|a, b| a.total_cmp(b)),
                outages: its.iter().map(|i| i.outages).sum(),
                incidents_30d: its.iter().map(|i| i.incidents_30d).sum(),
            }
        })
        .collect();
    providers.sort_by(|a, b| asc(a.avg_tcp_pct, b.avg_tcp_pct).then_with(|| a.name.cmp(&b.name)));

    Ok(json!({
        "month": w.month, "days": w.days, "from": w.from, "to": w.to, "tz": tz,
        "items": items, "providers": providers,
    }))
}

// ── 月报 ────────────────────────────────────────────────────────────────────

fn money(cents: i64) -> String { format!("${:.2}", cents as f64 / 100.0) }

/// 十进制单位（同网页与供应商的流量额度）
fn fmt_bytes(b: i64) -> String {
    let b = b.max(0) as f64;
    match b {
        b if b >= 1e12 => format!("{:.2} TB", b / 1e12),
        b if b >= 1e9 => format!("{:.2} GB", b / 1e9),
        b if b >= 1e6 => format!("{:.1} MB", b / 1e6),
        b => format!("{b:.0} B"),
    }
}

fn pct_text(p: Option<f64>) -> String {
    match p {
        None => "-".into(),
        Some(p) if p >= 100.0 => "100%".into(),
        Some(p) => format!("{:.2}%", p.min(99.99)),
    }
}

/// 一个月的摘要：数据（`GET /reports/monthly` 返回的 JSON）与通知用的文字（`text`）。
/// 流量按组织时区的自然日取自每日流量（`traffic::daily`，最多回看 92 天），月费是此刻在用机器的订阅月费折成 USD（不是当月的历史价格）
pub async fn monthly(st: &AppState, month: Option<&str>) -> ApiResult<Value> {
    let first = match month {
        Some(m) => parse_month(m)?,
        None => last_month(st).await?,
    };
    let w = window(st, Some(&month_str(first)), None).await?;
    let today = org_today(st).await?;
    let span = (today - w.from).whole_days() + 1;
    if span > traffic::MAX_DAYS {
        return Err(ApiError::bad_request(format!("流量明细只留近 {} 天，{} 已经查不了", traffic::MAX_DAYS, month_str(first))));
    }
    let daily = traffic::daily(st, Some(span), None, None).await?;
    let in_month = |d: &Date| *d >= w.from && *d <= w.to;
    let (down, up) = daily.total.iter().filter(|b| in_month(&b.date)).fold((0i64, 0i64), |a, b| (a.0.saturating_add(b.down), a.1.saturating_add(b.up)));
    // 每台机器这个月的业务流量：节点（不含线路机直出的行）与线路机经过它的
    let mut bytes: HashMap<Uuid, i64> = HashMap::new();
    for n in daily.nodes.iter().filter(|n| !n.direct) {
        bytes.insert(n.id, n.daily.iter().filter(|b| in_month(&b.date)).fold(0i64, |a, b| a.saturating_add(b.down).saturating_add(b.up)));
    }
    for r in &daily.relays {
        bytes.insert(r.id, r.business.iter().filter(|b| in_month(&b.date)).fold(0i64, |a, b| a.saturating_add(b.down).saturating_add(b.up)));
    }

    let live = views::live(st).await?;
    let mut costly: Vec<Value> = live
        .iter()
        .filter(|m| m.lifecycle == "active" && !m.spare)
        .filter_map(|m| {
            let usd = m.subscription.as_ref()?.monthly_usd_cents?;
            let gb = *bytes.get(&m.id)? as f64 / 1e9;
            (gb >= MIN_GB).then(|| json!({"id": m.id, "name": m.name, "monthly_usd_cents": usd, "gb": gb, "usd_per_gb": usd as f64 / 100.0 / gb}))
        })
        .collect();
    costly.sort_by(|a, b| b["usd_per_gb"].as_f64().partial_cmp(&a["usd_per_gb"].as_f64()).unwrap_or(Ordering::Equal));
    costly.truncate(3);

    let up_all = uptime(st, &w).await?;
    let worst: Vec<Value> = up_all["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| i["tcp_pct"].is_f64())
        .take(3)
        .map(|i| json!({"id": i["id"], "name": i["name"], "tcp_pct": i["tcp_pct"], "egress_pct": i["egress_pct"], "outages": i["outages"], "longest_outage_h": i["longest_outage_h"]}))
        .collect();

    let rules: Vec<(String, i64)> = sqlx::query_as(
        "select rule, count(*)::bigint c from incident
         where opened_at >= $1::date::timestamp at time zone $3 and opened_at < $2::date::timestamp at time zone $3
         group by rule order by c desc, rule limit 3",
    )
    .bind(w.from)
    .bind(w.to + Duration::days(1))
    .bind(meta::org_tz())
    .fetch_all(&st.db)
    .await?;
    let top_rules: Vec<Value> = rules.iter().map(|(r, c)| json!({"rule": r, "name": meta::rule_label(r), "count": c})).collect();

    let ov = views::overview(st).await?;
    let report = json!({
        "month": month_str(first), "from": w.from, "to": w.to, "tz": meta::org_tz(),
        "traffic": {"down": down, "up": up, "total": down.saturating_add(up)},
        "monthly_usd": ov["monthly_usd"], "machines": ov["nodes"]["total"].as_i64().unwrap_or(0) + ov["relays"]["total"].as_i64().unwrap_or(0),
        "costly": costly, "worst_uptime": worst, "top_rules": top_rules,
        "due_soon": ov["due_soon"], "overdue": ov["overdue"],
    });
    let text = text_of(&report);
    let mut report = report;
    report["text"] = json!(text);
    Ok(report)
}

/// 通知用的文字：第一行是月份，后面每项一段
fn text_of(r: &Value) -> String {
    let i = |v: &Value| v.as_i64().unwrap_or(0);
    let first = r["from"].as_str().unwrap_or("");
    let (y, m) = (first.get(..4).unwrap_or(""), first.get(5..7).unwrap_or("").trim_start_matches('0'));
    let mut l = vec![format!("{y} 年 {m} 月（{} 至 {}，{}）", first, r["to"].as_str().unwrap_or(""), meta::tz_label(r["tz"].as_str().unwrap_or("")))];
    let t = &r["traffic"];
    l.push(format!("总流量：{}（下载 {}，上传 {}）", fmt_bytes(i(&t["total"])), fmt_bytes(i(&t["down"])), fmt_bytes(i(&t["up"]))));
    let unconv: Vec<String> =
        r["monthly_usd"]["unconverted"].as_array().into_iter().flatten().map(|u| format!("{} {:.2}", u["currency"].as_str().unwrap_or(""), i(&u["cents"]) as f64 / 100.0)).collect();
    let extra = if unconv.is_empty() { String::new() } else { format!("；另有 {} 没有汇率、未折算", unconv.join("、")) };
    l.push(format!("月费合计：{}（在用 {} 台，按当前订阅折成 USD）{extra}", money(i(&r["monthly_usd"]["cents"])), i(&r["machines"])));
    let list = |title: &str, rows: Vec<String>| {
        if rows.is_empty() {
            return vec![format!("{title}：无")];
        }
        std::iter::once(format!("{title}：")).chain(rows.into_iter().enumerate().map(|(n, s)| format!("{}. {s}", n + 1))).collect::<Vec<_>>()
    };
    l.extend(list(
        "USD/GB 最高的 3 台",
        r["costly"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| format!("{} {}/GB（月费 {}，流量 {:.1} GB）", c["name"].as_str().unwrap_or(""), money_precise(c["usd_per_gb"].as_f64().unwrap_or(0.0)), money(i(&c["monthly_usd_cents"])), c["gb"].as_f64().unwrap_or(0.0)))
            .collect(),
    ));
    l.extend(list(
        "可用率最差的 3 台（TCP 探测成功率）",
        r["worst_uptime"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|w| {
                let out = if i(&w["outages"]) > 0 { format!("，断线 {} 次、最长 {} 小时", i(&w["outages"]), i(&w["longest_outage_h"])) } else { String::new() };
                format!("{} {}（出口 {}）{out}", w["name"].as_str().unwrap_or(""), pct_text(w["tcp_pct"].as_f64()), pct_text(w["egress_pct"].as_f64()))
            })
            .collect(),
    ));
    l.extend(list(
        "告警次数前 3 的规则",
        r["top_rules"].as_array().into_iter().flatten().map(|x| format!("{} {} 次", x["name"].as_str().unwrap_or(""), i(&x["count"]))).collect(),
    ));
    l.push(format!("即将到期：{} 台（已过期 {} 台）", i(&r["due_soon"]), i(&r["overdue"])));
    l.join("\n")
}

fn money_precise(usd: f64) -> String { format!("${usd:.3}") }

/// 把月报排进通知发送队列，只发给开了「月报」的已启用渠道，并写审计。没有这样的渠道回 409。
/// 手动发（`fleetctl report --send`，用来测试）和每月自动发是同一条路径
pub async fn send(st: &AppState, actor: &str, month: Option<&str>) -> ApiResult<Value> {
    let channels: i64 = sqlx::query_scalar("select count(*) from notify_channel where enabled and monthly_report").fetch_one(&st.db).await?;
    if channels == 0 {
        return Err(ApiError::conflict("没有开启「月报」的通知渠道：在设置的通知渠道里勾选「月报」（并且渠道已启用）"));
    }
    let r = monthly(st, month).await?;
    let text = r["text"].as_str().unwrap_or_default().to_string();
    let month = r["month"].as_str().unwrap_or_default().to_string();
    notify::enqueue(
        st,
        Notice {
            kind: Kind::Report,
            incident_id: None,
            rule: "monthly_report".into(),
            severity: "info".into(),
            machine: String::new(),
            summary: text.clone(),
            at: OffsetDateTime::now_utc(),
            opened_at: None,
        },
    )
    .await;
    let mut tx = st.db.begin().await?;
    audit(&mut tx, actor, "report.send", "report", None, json!({"month": month, "channels": channels, "auto": actor == ACTOR})).await?;
    tx.commit().await?;
    Ok(json!({"month": month, "channels": channels, "text": text}))
}

/// 每小时由 housekeeping 调一次：组织时区每月 1 号发上个月的月报，同一个月只发一次（先在 app_setting 里占位，发失败了再撤掉等下一小时重试）
pub async fn run_once(st: &AppState) -> anyhow::Result<()> {
    let today = org_today(st).await?;
    if today.day() != 1 {
        return Ok(());
    }
    let month = month_str(last_month(st).await?);
    let claimed = sqlx::query(
        "insert into app_setting (key, value, updated_by) values ($1, to_jsonb($2::text), $3)
         on conflict (key) do update set value = excluded.value, updated_at = now(), updated_by = excluded.updated_by where app_setting.value is distinct from excluded.value",
    )
    .bind(SENT_KEY)
    .bind(&month)
    .bind(ACTOR)
    .execute(&st.db)
    .await?
    .rows_affected()
        == 1;
    if !claimed {
        return Ok(());
    }
    match send(st, ACTOR, Some(&month)).await {
        Ok(v) => tracing::info!("月报 {month} 已排进发送队列（{} 个渠道）", v["channels"]),
        // 没有开月报的渠道：没什么可发，这个月算过了
        Err(e) if e.status == axum::http::StatusCode::CONFLICT => tracing::debug!("月报 {month} 没有发：{}", e.message),
        Err(e) => {
            sqlx::query("delete from app_setting where key = $1").bind(SENT_KEY).execute(&st.db).await?;
            anyhow::bail!("月报 {month} 生成失败：{}", e.message);
        }
    }
    Ok(())
}
