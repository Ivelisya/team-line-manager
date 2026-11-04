//! 计费台账（按节点数向团队收费）与客户页：batch list，billing init / month / paid / slot / add / rm / exempt，customer-page。
//! 每个节点（计费名额，由中控每分钟对账自动增减：节点进团队就计费、离开就停止，替换节点时新节点接过旧节点的）从开始收费日起每满一个月收一次（一期），
//! 每一期只有已支付 / 待支付两种状态；按月看 = 每个节点起点落在这个月的那一期。名额用 id 前缀（batch list <团队> 能看到）或占用它的节点名指定。
//! 客户页链接是只读分享链接，客户不登录，页面只有每个节点的名字、地区、这一期的区间、已付 / 待付，不出现 IP、供应商、价格

use crate::cmds::{st, Ctx};
use crate::ops::confirm;
use crate::resolve::Want;
use crate::table;
use anyhow::bail;
use serde_json::{json, Value};

fn slot_state_cn(s: &Value) -> String {
    match s["state"].as_str() {
        Some("ended") => format!("已停止收费（{} 停）", st(s, "/end_date")),
        _ if s["end_date"].is_string() => format!("收费中（{} 停）", st(s, "/end_date")),
        _ => "收费中".into(),
    }
}

/// 团队的计费数据 `GET /teams/{id}/batches`
fn load(ctx: &Ctx, team: &str) -> anyhow::Result<Value> {
    let id = ctx.team_id(team)?;
    ctx.c.get(&format!("/teams/{id}/batches"))
}

/// 团队的正式名字（用户可能写的是 id 前缀）；查不到就用原写法
fn team_name(ctx: &Ctx, id: &str, fallback: &str) -> String {
    ctx.teams().ok().and_then(|t| t.as_array().and_then(|a| a.iter().find(|x| st(x, "/id") == id).map(|x| st(x, "/name")))).unwrap_or_else(|| fallback.to_string())
}

fn arr(v: &Value) -> &[Value] { v.as_array().map_or(&[][..], Vec::as_slice) }

fn slots_table(d: &Value) -> String {
    let rows: Vec<Vec<String>> =
        arr(&d["slots"]).iter().map(|s| vec![st(s, "/id").chars().take(8).collect(), st(s, "/name"), st(s, "/region"), st(s, "/start_date"), slot_state_cn(s)]).collect();
    table::render(&["名额", "节点", "地区", "开始收费", "状态"], &rows)
}

fn events_table(evs: &[Value]) -> String {
    let rows: Vec<Vec<String>> = evs.iter().map(|e| vec![table::ts(&st(e, "/at")).chars().take(16).collect(), st(e, "/text")]).collect();
    table::render(&["时间", "记录"], &rows)
}

/// `batch list <团队> [--events]`：计费节点（含已停止的）、客户页链接
pub fn list(ctx: &Ctx, team: &str, events: bool) -> anyhow::Result<u8> {
    let d = load(ctx, team)?;
    let evs = if events { Some(ctx.c.get_q(&format!("/teams/{}/billing-events", st(&d, "/team/id")), &[("limit", "200".into())])?.as_array().cloned().unwrap_or_default()) } else { None };
    let data = match &evs {
        Some(e) => {
            let mut x = d.clone();
            x["events"] = json!(e);
            x
        }
        None => d.clone(),
    };
    ctx.emit(&data, || {
        let name = st(&d, "/team/name");
        let t = &d["totals"];
        let mut out = format!("{name} · 今天 {} · 收费中的节点 {} 个（已停止的 {} 个）\n", st(&d, "/today"), st(t, "/active"), st(t, "/ended"));
        if arr(&d["slots"]).is_empty() {
            out.push_str("还没有计费节点（还没开始记账：fleetctl billing init --since 2026-10-01）");
        } else {
            out.push_str(&slots_table(&d));
        }
        if let Some(e) = &evs {
            out.push_str(&format!("\n\n增减记录（新的在前）：\n{}", if e.is_empty() { "（没有）".into() } else { events_table(e) }));
        }
        out.push_str(&format!("\n\n{}", page_text(&name, &d["page"])));
        out
    });
    Ok(0)
}

/// `billing init --since 日期 [--yes]`：不加 --yes 只预览
pub fn init(ctx: &Ctx, since: &str, yes: bool) -> anyhow::Result<u8> {
    let p = ctx.c.post("/billing/init", &json!({"since": since, "dry_run": true}))?;
    let show = |v: &Value| {
        let mut out = String::new();
        for t in arr(&v["teams"]) {
            if t["skipped"] == true {
                out.push_str(&format!("{}：已有 {} 个计费节点，跳过\n", st(t, "/team/name"), st(t, "/existing")));
            } else {
                let names: Vec<String> = arr(&t["nodes"]).iter().map(|n| format!("{}（{}）", st(n, "/name"), st(n, "/region"))).collect();
                out.push_str(&format!("{}：{} 个 {}\n", st(t, "/team/name"), st(t, "/count"), names.join("、")));
            }
        }
        out
    };
    if !yes {
        ctx.emit(&p, || format!("预览（{} 开始收费，共 {} 个；加 --yes 才会建）：\n{}", st(&p, "/since"), st(&p, "/total"), show(&p)));
        return Ok(0);
    }
    let d = ctx.c.post("/billing/init", &json!({"since": since}))?;
    ctx.emit(&d, || format!("已开始记账（{} 开始收费，建了 {} 个；之后中控每分钟自动对账）：\n{}", st(&d, "/since"), st(&d, "/total"), show(&d)));
    Ok(0)
}

/// `2026-11` → 「2026 年 11 月」
fn month_cn(m: &str) -> String { format!("{} 年 {} 月", &m[..m.len().min(4)], m.get(5..7).and_then(|x| x.parse::<u8>().ok()).unwrap_or(0)) }

/// 一期写成「2026-10-30 → 11-30 到期」：起点是购买（开始收费）那天，终点是到期日（同供应商写的到期日，下一期从这天起）
fn span(n: &Value) -> String {
    let end = st(n, "/period_end");
    format!("{} → {} 到期", st(n, "/period_start"), end.get(5..).unwrap_or(&end))
}

/// 某月清单的文字（GET /teams/{id}/billing/month 的 data）：一句话 + 每个节点一行（按本期生效日排列）
fn month_text(d: &Value) -> String {
    let nodes = arr(&d["nodes"]);
    let unpaid: Vec<String> = nodes.iter().filter(|n| n["paid"] != true).map(|n| st(n, "/name")).collect();
    let head = format!("{} · {}", st(d, "/team/name"), month_cn(&st(d, "/month")));
    let mut out = if nodes.is_empty() {
        format!("{head}：没有要收钱的节点")
    } else if unpaid.is_empty() {
        format!("{head}：要收 {} 个节点的钱，都收齐了", nodes.len())
    } else {
        let names = if unpaid.len() <= 5 { format!("（{}）", unpaid.join("、")) } else { String::new() };
        format!("{head}：要收 {} 个节点的钱，已支付 {} 个，待支付 {} 个{names}", nodes.len(), nodes.len() - unpaid.len(), unpaid.len())
    };
    if !nodes.is_empty() {
        let rows: Vec<Vec<String>> = nodes
            .iter()
            .map(|n| {
                let mut tags = vec![];
                if n["manual"] == true {
                    tags.push("手动添加");
                }
                if n["ended"] == true {
                    tags.push("已停止收费");
                }
                if n["upcoming"] == true {
                    tags.push("待续期");
                }
                vec![st(n, "/name"), st(n, "/region"), span(n), if n["paid"] == true { "已支付".into() } else { "待支付".into() }, tags.join("、")]
            })
            .collect();
        out.push('\n');
        out.push_str(&table::render(&["节点", "地区", "这一期", "收费", "备注"], &rows));
    }
    out
}

/// `billing month <团队> [2026-11]`：这个月每个节点的那一期，已支付 / 待支付（月份默认本月）
pub fn month(ctx: &Ctx, team: &str, month: Option<&str>) -> anyhow::Result<u8> {
    let id = ctx.team_id(team)?;
    let q: Vec<(&str, String)> = month.map(|m| vec![("month", m.to_string())]).unwrap_or_default();
    let d = ctx.c.get_q(&format!("/teams/{id}/billing/month"), &q)?;
    ctx.emit(&d, || month_text(&d));
    Ok(0)
}

/// 月清单一行的名额 id 前 8 位
fn id8(n: &Value) -> String { st(n, "/slot_id").chars().take(8).collect() }

/// 要标的节点：--all = 这个月所有还不是目标状态的；否则按名字（或名额 id 前缀，≥ 4 位）逐个找，找不到报错并列出这个月有哪些
fn pick<'a>(nodes: &'a [Value], names: &[String], all: bool, want: bool) -> anyhow::Result<Vec<&'a Value>> {
    if all {
        return Ok(nodes.iter().filter(|n| (n["paid"] == true) != want).collect());
    }
    if names.is_empty() {
        bail!("要写节点名，或加 --all");
    }
    let mut out = vec![];
    for q in names {
        let q = q.trim();
        let hits: Vec<&Value> = nodes.iter().filter(|n| st(n, "/name") == q || (q.len() >= 4 && st(n, "/slot_id").starts_with(&q.to_ascii_lowercase()))).collect();
        match hits.as_slice() {
            [n] => out.push(*n),
            [] => bail!("这个月没有「{q}」要收费；这个月有：{}", nodes.iter().map(|n| format!("{}（{}）", st(n, "/name"), id8(n))).collect::<Vec<_>>().join("、")),
            _ => bail!("「{q}」对上了多个节点，用名额 id 前缀指定：\n{}", hits.iter().map(|n| format!("  {}  {}  {} 开始收费  {}", id8(n), st(n, "/name"), st(n, "/start_date"), span(n))).collect::<Vec<_>>().join("\n")),
        }
    }
    Ok(out)
}

#[derive(clap::Args)]
pub struct PaidOpts {
    /// 团队
    team: String,
    /// 节点名（团队1-10；手动添加的写备注），可写多个；重名时报错并列出名额 id，改写 id 前缀（≥ 4 位）；或用 --all
    nodes: Vec<String>,
    /// 这个月所有待支付的（配合 --unpaid 是所有已支付的）
    #[arg(long, conflicts_with = "nodes")]
    all: bool,
    /// 月份，如 2026-11（默认本月）：标的是每个节点起点在这个月的那一期
    #[arg(long, value_name = "YYYY-MM")]
    month: Option<String>,
    /// 改回待支付
    #[arg(long)]
    unpaid: bool,
    /// 不加只预览
    #[arg(long)]
    yes: bool,
}

/// `billing paid <团队> [节点…] [--all] [--month] [--unpaid] [--yes]`：标记已支付 / 改回待支付；不加 --yes 只预览
pub fn paid(ctx: &Ctx, o: &PaidOpts) -> anyhow::Result<u8> {
    let id = ctx.team_id(&o.team)?;
    let q: Vec<(&str, String)> = o.month.as_ref().map(|m| vec![("month", m.clone())]).unwrap_or_default();
    let d = ctx.c.get_q(&format!("/teams/{id}/billing/month"), &q)?;
    let want = !o.unpaid;
    let picked = pick(arr(&d["nodes"]), &o.nodes, o.all, want)?;
    let what = if want { "标为已支付" } else { "改回待支付" };
    let mon = month_cn(&st(&d, "/month"));
    if picked.is_empty() {
        ctx.emit(&json!({"changed": 0, "month": d["month"]}), || format!("{mon}没有要{what}的节点"));
        return Ok(0);
    }
    let names: Vec<String> = picked.iter().map(|n| st(n, "/name")).collect();
    if !o.yes {
        let p = json!({"preview": true, "month": d["month"], "paid": want, "nodes": names});
        ctx.emit(&p, || format!("预览（加 --yes 才会改）：{mon} {what}：{}（共 {} 个）", names.join("、"), names.len()));
        return Ok(0);
    }
    let ids: Vec<String> = picked.iter().map(|n| st(n, "/slot_id")).collect();
    let r = ctx.c.post(&format!("/teams/{id}/billing/paid"), &json!({"month": d["month"], "slot_ids": ids, "paid": want}))?;
    ctx.emit(&r, || format!("已{what}：{}（共 {} 个）\n\n{}", names.join("、"), names.len(), month_text(&r)));
    Ok(0)
}

/// 找名额：id 前缀（≥ 4 位）或节点名（占用它的，或停止前占用它的；有多个时先要没停止的）。返回 (名额, 团队名)
fn find_slot(ctx: &Ctx, q: &str) -> anyhow::Result<(Value, String)> {
    let q = q.trim();
    let mut hits: Vec<(Value, String)> = vec![];
    for t in arr(&ctx.teams()?).iter().filter(|t| t["spare"] != true) {
        let d = ctx.c.get(&format!("/teams/{}/batches", st(t, "/id")))?;
        for s in arr(&d["slots"]) {
            let id = st(s, "/id");
            if (q.len() >= 4 && id.starts_with(&q.to_ascii_lowercase())) || st(s, "/name") == q {
                hits.push((s.clone(), st(t, "/name")));
            }
        }
    }
    if hits.len() > 1 {
        let live: Vec<(Value, String)> = hits.iter().filter(|(s, _)| s["state"] != "ended").cloned().collect();
        if !live.is_empty() {
            hits = live;
        }
    }
    match hits.len() {
        0 => bail!("找不到计费节点「{q}」（fleetctl batch list <团队> 看名额 id）"),
        1 => Ok(hits.remove(0)),
        _ => {
            let lines: Vec<String> = hits.iter().map(|(s, t)| format!("  {}  {t} {}  {}  {}", &st(s, "/id")[..8], st(s, "/name"), st(s, "/start_date"), slot_state_cn(s))).collect();
            bail!("「{q}」对上了多个计费节点，用名额 id 前缀指定：\n{}", lines.join("\n"))
        }
    }
}

fn slot_line(team: &str, s: &Value) -> String {
    format!(
        "{team} · {} · {} · {} 开始收费 · {}{}",
        st(s, "/name"),
        st(s, "/region"),
        st(s, "/start_date"),
        slot_state_cn(s),
        s["note"].as_str().filter(|n| !n.is_empty() && s["manual"] != true).map(|n| format!(" · {n}")).unwrap_or_default()
    )
}

#[derive(clap::Args)]
pub struct SlotOpts {
    /// 名额：id 前缀，或占用它的节点名（团队1-10）
    slot: String,
    /// 改开始收费日，如 2026-10-30（以后每期从这天算；最晚今天加一个月；已支付的期按第几期跟着挪到新的期）
    #[arg(long, value_name = "日期")]
    start: Option<String>,
    /// 改地区（美区、墨西哥…）
    #[arg(long)]
    region: Option<String>,
    /// 改备注（≤ 60 字；空字符串清掉）
    #[arg(long)]
    note: Option<String>,
    /// 停止收费（只限手动添加的；系统节点离开团队即停止，这里只能改它的停止日，不晚于今天）：起点早于这天的期照收
    #[arg(long, value_name = "日期", conflicts_with = "reopen")]
    end: Option<String>,
    /// 恢复收费（只限手动添加的）
    #[arg(long)]
    reopen: bool,
}

/// `billing slot <名额> [--start] [--region] [--note] [--end|--reopen]`：不给改动时显示这个名额
pub fn slot(ctx: &Ctx, o: &SlotOpts) -> anyhow::Result<u8> {
    let (cur, team) = find_slot(ctx, &o.slot)?;
    let mut body = json!({});
    for (k, v) in [("start_date", &o.start), ("region", &o.region), ("note", &o.note), ("end_date", &o.end)] {
        if let Some(v) = v {
            body[k] = json!(v);
        }
    }
    if o.reopen {
        body["end_date"] = Value::Null;
    }
    if body.as_object().is_some_and(|m| m.is_empty()) {
        ctx.emit(&cur, || slot_line(&team, &cur));
        return Ok(0);
    }
    let s = ctx.c.patch(&format!("/billing-slots/{}", st(&cur, "/id")), &body)?;
    ctx.emit(&s, || format!("已保存\n{}", slot_line(&team, &s)));
    Ok(0)
}

#[derive(clap::Args)]
pub struct AddOpts {
    /// 团队
    team: String,
    /// 开始收费日，如 2026-10-15
    #[arg(long, value_name = "日期")]
    start: String,
    /// 地区：美区、墨西哥、菲律宾…（1–20 字）
    #[arg(long)]
    region: String,
    /// 个数（1–50）
    #[arg(long, default_value_t = 1)]
    count: i32,
    /// 备注（≤ 60 字），如 墨西哥家宽：清单和客户页上就用它当名字
    #[arg(long)]
    note: Option<String>,
}

/// `billing add <团队> --start --region [--count N] [--note]`：手动添加的计费节点（系统外的节点）
pub fn add(ctx: &Ctx, o: &AddOpts) -> anyhow::Result<u8> {
    let id = ctx.team_id(&o.team)?;
    let v = ctx.c.post(&format!("/teams/{id}/billing-slots"), &json!({"region": o.region, "start_date": o.start, "count": o.count, "note": o.note.clone().unwrap_or_default()}))?;
    let team = team_name(ctx, &id, &o.team);
    ctx.emit(&v, || format!("已手动添加 {} 个计费节点\n{}", arr(&v).len(), arr(&v).iter().map(|s| slot_line(&team, s)).collect::<Vec<_>>().join("\n")));
    Ok(0)
}

/// `billing rm <名额>`：只能删手动添加的（录错了）
pub fn rm(ctx: &Ctx, q: &str, yes: bool) -> anyhow::Result<u8> {
    let (cur, team) = find_slot(ctx, q)?;
    if cur["manual"] != true {
        bail!("只有手动添加的计费节点能删除；系统节点离开团队后由对账自动停止收费，不想收它的钱用 fleetctl billing exempt <节点> --on");
    }
    confirm(&format!("删除 {}？", slot_line(&team, &cur)), yes)?;
    let d = ctx.c.delete(&format!("/billing-slots/{}", st(&cur, "/id")))?;
    ctx.emit(&d, || "已删除".into());
    Ok(0)
}

/// `billing exempt <节点> --on|--off`
pub fn exempt(ctx: &Ctx, node: &str, on: bool) -> anyhow::Result<u8> {
    let (id, name) = ctx.find_id(node, Want::Live)?;
    let d = ctx.c.put(&format!("/machines/{id}/billing-exempt"), &json!({"exempt": on}))?;
    ctx.emit(&d, || {
        if on {
            format!("{name} 已设为不收费：当它离开团队，计费节点今天停止收费（已经开始的这一期照收）")
        } else {
            format!("{name} 已恢复收费：当它加入团队，这一期有效期尚未结束的原样接回，否则按购买日新开始收费")
        }
    });
    Ok(0)
}

/// 客户页链接的一段说明（没有链接时提示怎么生成）
fn page_text(team: &str, p: &Value) -> String {
    if p.is_null() {
        return format!("客户页链接：还没有（fleetctl customer-page {team} --rotate 生成）");
    }
    let viewed = match p["last_view_at"].as_str() {
        Some(t) => format!("最近查看 {}（{}）", table::ts(t), p["last_view_ip"].as_str().unwrap_or("-")),
        None => "还没人看过".into(),
    };
    let state = if p["enabled"] == true { "启用" } else { "已停用（打开回 404）" };
    format!("客户页链接：{}\n  状态 {state}；查看密码 {}；固定有效；{viewed}", p["link"].as_str().unwrap_or("（解不开，--rotate 换新链接）"), p["password"].as_str().unwrap_or("请换新链接"))
}

#[derive(clap::Args)]
pub struct PageOpts {
    /// 团队
    team: String,
    /// 生成链接；已有就换新链接，旧链接立即失效
    #[arg(long, conflicts_with_all = ["disable", "enable", "delete"])]
    rotate: bool,
    /// 停用：打开回 404，链接不变
    #[arg(long, conflicts_with_all = ["enable", "delete"])]
    disable: bool,
    /// 重新启用
    #[arg(long, conflicts_with = "delete")]
    enable: bool,
    /// 删除链接
    #[arg(long)]
    delete: bool,
    #[arg(long)]
    yes: bool,
}

/// 无参数显示链接与状态；`--rotate` / `--disable` / `--enable` / `--delete` 改
pub fn page(ctx: &Ctx, o: &PageOpts) -> anyhow::Result<u8> {
    let id = ctx.team_id(&o.team)?;
    let path = format!("/teams/{id}/customer-page");
    let name = || team_name(ctx, &id, &o.team);
    if o.rotate {
        let had = !ctx.c.get(&format!("/teams/{id}/batches"))?["page"].is_null();
        if had {
            confirm(&format!("给 {} 换新的客户页链接？旧链接立即失效，已经发给客户的要重新发", name()), o.yes)?;
        }
        let p = ctx.c.post(&path, &json!({}))?;
        ctx.emit(&p, || format!("{}\n{}", if had { "已换新链接，旧链接已失效" } else { "已生成客户页链接" }, page_text(&name(), &p)));
    } else if o.disable || o.enable {
        let p = ctx.c.patch(&path, &json!({"enabled": o.enable}))?;
        ctx.emit(&p, || format!("{}\n{}", if o.enable { "已启用" } else { "已停用" }, page_text(&name(), &p)));
    } else if o.delete {
        confirm(&format!("删除 {} 的客户页链接？用它的客户之后打不开（计费记录保留）", name()), o.yes)?;
        let d = ctx.c.delete(&path)?;
        ctx.emit(&d, || "已删除客户页链接".to_string());
    } else {
        let d = ctx.c.get(&format!("/teams/{id}/batches"))?;
        let page = d["page"].clone();
        ctx.emit(&page, || page_text(&name(), &page));
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(name: &str, id: &str, paid: bool) -> Value {
        json!({"slot_id": id, "name": name, "region": "美区", "period_start": "2026-10-30", "period_end": "2026-11-30", "paid": paid, "manual": false, "ended": false, "upcoming": false})
    }

    #[test]
    fn billing_month_text_reads_plainly() {
        let mut d = json!({"team": {"name": "团队1"}, "month": "2026-10", "nodes": [n("团队1-10", "aaaa1111", false), n("团队1-01", "bbbb2222", true)]});
        let t = month_text(&d);
        assert!(t.starts_with("团队1 · 2026 年 10 月：要收 2 个节点的钱，已支付 1 个，待支付 1 个（团队1-10）\n"), "{t}");
        assert!(t.contains("2026-10-30 → 11-30 到期") && t.contains("待支付") && t.contains("已支付"), "{t}");
        d["nodes"][0]["paid"] = json!(true);
        let t = month_text(&d);
        assert!(t.starts_with("团队1 · 2026 年 10 月：要收 2 个节点的钱，都收齐了"), "{t}");
        d["nodes"] = json!([]);
        assert!(month_text(&d).starts_with("团队1 · 2026 年 10 月：没有要收钱的节点"));
    }

    #[test]
    fn billing_paid_picks_by_name_prefix_or_all() {
        let nodes = vec![n("团队1-10", "aaaa1111", false), n("团队1-01", "bbbb2222", true), n("墨西哥家宽", "cccc3333", false)];
        let names = |v: Vec<&Value>| v.iter().map(|x| st(x, "/name")).collect::<Vec<_>>();
        assert_eq!(names(pick(&nodes, &[], true, true).unwrap()), ["团队1-10", "墨西哥家宽"]);
        assert_eq!(names(pick(&nodes, &[], true, false).unwrap()), ["团队1-01"]);
        assert_eq!(names(pick(&nodes, &["墨西哥家宽".into(), "BBBB".into()], false, true).unwrap()), ["墨西哥家宽", "团队1-01"]);
        assert!(pick(&nodes, &["团队1-99".into()], false, true).unwrap_err().to_string().contains("这个月有：团队1-10（aaaa1111）、团队1-01（bbbb2222）、墨西哥家宽（cccc3333）"));
        assert!(pick(&nodes, &[], false, true).is_err());
        // 重名：报错并列出候选（名额 id、开始收费日），用 id 前缀可以指定
        let mut dup = vec![n("团队1-05", "dddd4444", false), n("团队1-05", "eeee5555", false)];
        dup[0]["start_date"] = json!("2026-10-01");
        let e = pick(&dup, &["团队1-05".into()], false, true).unwrap_err().to_string();
        assert!(e.contains("dddd4444") && e.contains("eeee5555") && e.contains("2026-10-01 开始收费"), "{e}");
        assert_eq!(names(pick(&dup, &["EEEE".into()], false, true).unwrap()).len(), 1);
    }
}
