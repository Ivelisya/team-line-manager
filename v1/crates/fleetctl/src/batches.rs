//! 计费台账（按节点数向团队收费）与客户到期页：batch list，billing init / month / slot / add / rm / exempt，customer-page。
//! 计费名额 = 团队里一个按月计费的节点位置，由中控每分钟对账自动增减（节点加入 / 离开，24 小时内一进一出算替换）；批次 = 同一天起算、同地区的名额，
//! 从起算日起每月同日续一期。名额用 id 前缀（batch list <团队> --slots 能看到）或占用它的节点名指定。客户页链接是只读分享链接，客户不登录，页面不出现 IP、供应商、价格、节点名

use crate::cmds::{st, Ctx};
use crate::ops::confirm;
use crate::resolve::Want;
use crate::table;
use anyhow::bail;
use serde_json::{json, Value};

fn state_cn(s: &str) -> &str {
    match s {
        "expired" => "已到期",
        "today" => "今天到期",
        _ => "有效",
    }
}

fn slot_state_cn(s: &Value) -> String {
    match s["state"].as_str() {
        Some("vacated") => format!("空出待定（{} 起）", table::ts(&st(s, "/vacated_at")).chars().take(16).collect::<String>()),
        Some("ended") => format!("已结束 {}", st(s, "/end_date")),
        _ if s["end_date"].is_string() => format!("在用（{} 结束）", st(s, "/end_date")),
        _ => "在用".into(),
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

/// 下次续费：有效批次里最近的终点与个数（最多 2 个日子）
fn next_renewals(d: &Value) -> String {
    let mut ends: Vec<(String, i64)> = vec![];
    for b in arr(&d["batches"]).iter().filter(|b| b["state"] != "expired") {
        let e = st(b, "/end_date");
        let c = b["count"].as_i64().unwrap_or(0);
        match ends.iter_mut().find(|x| x.0 == e) {
            Some(x) => x.1 += c,
            None => ends.push((e, c)),
        }
    }
    ends.sort();
    if ends.is_empty() {
        return "-".into();
    }
    ends.iter().take(2).map(|(e, c)| format!("{e}（{c} 个）")).collect::<Vec<_>>().join("、")
}

fn slots_table(d: &Value) -> String {
    let rows: Vec<Vec<String>> = arr(&d["slots"])
        .iter()
        .map(|s| {
            vec![
                st(s, "/id").chars().take(8).collect(),
                st(s, "/name"),
                st(s, "/region"),
                st(s, "/start_date"),
                s["batch_seq"].as_i64().map_or("-".into(), |n| format!("{n:02}")),
                slot_state_cn(s),
            ]
        })
        .collect();
    table::render(&["名额", "节点", "地区", "起算", "批次", "状态"], &rows)
}

fn events_table(evs: &[Value]) -> String {
    let rows: Vec<Vec<String>> = evs.iter().map(|e| vec![table::ts(&st(e, "/at")).chars().take(16).collect(), st(e, "/text")]).collect();
    table::render(&["时间", "记录"], &rows)
}

/// `batch list <团队> [--slots] [--events]`
pub fn list(ctx: &Ctx, team: &str, slots: bool, events: bool) -> anyhow::Result<u8> {
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
        let mut out = format!(
            "{name} · 今天 {} · 当前计费节点 {} 个（待定 {}、空出待定 {}）· 下次续费 {}\n",
            st(&d, "/today"),
            st(t, "/active"),
            st(t, "/pending"),
            st(t, "/vacated"),
            next_renewals(&d)
        );
        let bs = arr(&d["batches"]);
        if bs.is_empty() {
            out.push_str("还没有计费批次（还没初始化：fleetctl billing init --since 2026-10-01）");
        } else {
            let rows: Vec<Vec<String>> = bs
                .iter()
                .map(|b| {
                    vec![
                        format!("{:02}", b["seq"].as_i64().unwrap_or(0)),
                        st(b, "/region"),
                        format!("{} → {}", st(b, "/start_date"), st(b, "/end_date")),
                        format!("{} 个", st(b, "/count")),
                        format!("{} 期", st(b, "/months")),
                        st(b, "/node_months"),
                        state_cn(&st(b, "/state")).to_string(),
                        b["summary"].as_str().unwrap_or("").to_string(),
                    ]
                })
                .collect();
            out.push_str(&table::render(&["批次", "地区", "起算 → 下次续费", "当前个数", "已计", "节点月", "状态", "说明"], &rows));
        }
        let pending = arr(&d["pending"]);
        if !pending.is_empty() {
            out.push_str("\n\n待定（24 小时内有节点离开就算替换，否则到点算新增）：\n");
            let rows: Vec<Vec<String>> =
                pending.iter().map(|p| vec![st(p, "/machine/name"), table::ts(&st(p, "/seen_at")).chars().take(16).collect(), table::ts(&st(p, "/decide_at")).chars().take(16).collect()]).collect();
            out.push_str(&table::render(&["节点", "加入", "到点"], &rows));
        }
        if slots {
            out.push_str(&format!("\n\n计费名额：\n{}", slots_table(&d)));
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
                out.push_str(&format!("{}：已有 {} 个名额，跳过\n", st(t, "/team/name"), st(t, "/existing")));
            } else {
                let names: Vec<String> = arr(&t["nodes"]).iter().map(|n| format!("{}（{}）", st(n, "/name"), st(n, "/region"))).collect();
                out.push_str(&format!("{}：{} 个 {}\n", st(t, "/team/name"), st(t, "/count"), names.join("、")));
            }
        }
        out
    };
    if !yes {
        ctx.emit(&p, || format!("预览（{} 起算，共 {} 个；加 --yes 才会建）：\n{}", st(&p, "/since"), st(&p, "/total"), show(&p)));
        return Ok(0);
    }
    let d = ctx.c.post("/billing/init", &json!({"since": since}))?;
    ctx.emit(&d, || format!("已初始化（{} 起算，建了 {} 个；之后中控每分钟自动对账）：\n{}", st(&d, "/since"), st(&d, "/total"), show(&d)));
    Ok(0)
}

/// `billing month <团队> [2026-11]`：月度应收
pub fn month(ctx: &Ctx, team: &str, month: Option<&str>) -> anyhow::Result<u8> {
    let id = ctx.team_id(team)?;
    let q: Vec<(&str, String)> = month.map(|m| vec![("month", m.to_string())]).unwrap_or_default();
    let d = ctx.c.get_q(&format!("/teams/{id}/receivable"), &q)?;
    ctx.emit(&d, || {
        let name = team_name(ctx, &id, team);
        let mut out = format!("{name} · {} 月度应收{}\n", st(&d, "/month"), if d["projected"] == true { "（含预估：还没到的续期按现在的名额算）" } else { "" });
        let lines = arr(&d["lines"]);
        if lines.is_empty() {
            out.push_str("这个月没有要收的期\n");
        } else {
            let rows: Vec<Vec<String>> = lines
                .iter()
                .map(|l| {
                    vec![
                        format!("第{:02}批", l["batch_seq"].as_i64().unwrap_or(0)),
                        st(l, "/region"),
                        format!("{} → {}", st(l, "/period_start"), st(l, "/period_end")),
                        format!("{} 个{}", st(l, "/count"), if l["projected"] == true { "（预估）" } else { "" }),
                    ]
                })
                .collect();
            out.push_str(&table::render(&["批次", "地区", "本期", "个数"], &rows));
            out.push('\n');
        }
        out.push_str(&format!("合计：本月应收 {} 个节点月", st(&d, "/total")));
        if !d["start_count"].is_null() {
            out.push_str(&format!("；月初 {} 个、月末（本月是今天）{} 个", st(&d, "/start_count"), st(&d, "/end_count")));
        }
        let evs = arr(&d["events"]);
        if !evs.is_empty() {
            out.push_str(&format!("\n\n本月增减：\n{}", events_table(evs)));
        }
        out
    });
    Ok(0)
}

/// `billing batch <团队> <批次编号或key> --start 日期`：整批改起算日
pub fn batch(ctx: &Ctx, team: &str, batch: &str, start: &str) -> anyhow::Result<u8> {
    let d = load(ctx, team)?;
    let q = batch.trim();
    let b = arr(&d["batches"]).iter().find(|b| st(b, "/key") == q || q.parse::<i64>().is_ok_and(|n| b["seq"].as_i64() == Some(n)));
    let Some(b) = b else { bail!("{} 没有批次「{q}」（fleetctl batch list {} 看编号）", st(&d, "/team/name"), team) };
    let key = st(b, "/key").replace('|', "%7C");
    let r = ctx.c.patch(&format!("/teams/{}/batches/{key}", st(&d, "/team/id")), &json!({"start_date": start}))?;
    ctx.emit(&r, || format!("已改：{} 第{:02}批 · {} 起算 {}（{} 个）", st(&d, "/team/name"), r["seq"].as_i64().unwrap_or(0), st(&r, "/region"), st(&r, "/start_date"), st(&r, "/count")));
    Ok(0)
}

/// 找名额：id 前缀（≥ 4 位）或节点名（占用它的，或空出 / 结束前占用它的；有多个时先要没结束的）。返回 (名额, 团队名)
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
        0 => bail!("找不到计费名额「{q}」（fleetctl batch list <团队> --slots 看名额 id）"),
        1 => Ok(hits.remove(0)),
        _ => {
            let lines: Vec<String> = hits.iter().map(|(s, t)| format!("  {}  {t} {}  {}  {}", &st(s, "/id")[..8], st(s, "/name"), st(s, "/start_date"), slot_state_cn(s))).collect();
            bail!("「{q}」对上了多个名额，用名额 id 前缀指定：\n{}", lines.join("\n"))
        }
    }
}

fn slot_line(team: &str, s: &Value) -> String {
    format!(
        "{team} · {} · {} · {} 起算 · 第{}批 · {}{}",
        st(s, "/name"),
        st(s, "/region"),
        st(s, "/start_date"),
        s["batch_seq"].as_i64().map_or("-".into(), |n| format!("{n:02}")),
        slot_state_cn(s),
        s["note"].as_str().filter(|n| !n.is_empty() && s["manual"] != true).map(|n| format!(" · {n}")).unwrap_or_default()
    )
}

#[derive(clap::Args)]
pub struct SlotOpts {
    /// 名额：id 前缀，或占用它的节点名（团队1-10）
    slot: String,
    /// 改起算日，如 2026-10-30
    #[arg(long, value_name = "日期")]
    start: Option<String>,
    /// 改地区（美区、墨西哥…）
    #[arg(long)]
    region: Option<String>,
    /// 改备注（≤ 60 字；空字符串清掉）
    #[arg(long)]
    note: Option<String>,
    /// 结束计费（只限手动名额与已空出的）：算到这天所在的那一期
    #[arg(long, value_name = "日期", conflicts_with = "reopen")]
    end: Option<String>,
    /// 恢复计费（只限手动名额）
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
    /// 起算日，如 2026-10-15
    #[arg(long, value_name = "日期")]
    start: String,
    /// 地区：美区、墨西哥、菲律宾…（1–20 字）
    #[arg(long)]
    region: String,
    /// 个数（1–50）
    #[arg(long, default_value_t = 1)]
    count: i32,
    /// 备注（≤ 60 字），如 墨西哥家宽
    #[arg(long)]
    note: Option<String>,
}

/// `billing add <团队> --start --region [--count N] [--note]`：手动名额（系统外的节点）
pub fn add(ctx: &Ctx, o: &AddOpts) -> anyhow::Result<u8> {
    let id = ctx.team_id(&o.team)?;
    let v = ctx.c.post(&format!("/teams/{id}/billing-slots"), &json!({"region": o.region, "start_date": o.start, "count": o.count, "note": o.note.clone().unwrap_or_default()}))?;
    let team = team_name(ctx, &id, &o.team);
    ctx.emit(&v, || format!("已添加 {} 个手动计费节点\n{}", arr(&v).len(), arr(&v).iter().map(|s| slot_line(&team, s)).collect::<Vec<_>>().join("\n")));
    Ok(0)
}

/// `billing rm <名额>`：只能删手动名额（录错了）
pub fn rm(ctx: &Ctx, q: &str, yes: bool) -> anyhow::Result<u8> {
    let (cur, team) = find_slot(ctx, q)?;
    if cur["manual"] != true {
        bail!("只有手动添加的计费节点能删除；系统节点离开团队后由对账自动结束，不想收它的钱用 fleetctl billing exempt <节点> --on");
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
            format!("{name} 已设为不计费：中控对账时当它离开团队（24 小时内有新节点加入算替换，否则计费节点减少）")
        } else {
            format!("{name} 已恢复计费：中控对账时当它加入团队（24 小时内有节点离开算替换，否则新增）")
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
    format!("客户页链接：{}\n  状态 {state}；链接生成于 {}；{viewed}", p["link"].as_str().unwrap_or("（解不开，--rotate 换新链接）"), table::ts(&st(p, "/token_at")))
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

