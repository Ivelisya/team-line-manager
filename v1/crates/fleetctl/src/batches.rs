//! 客户批次账与客户到期页：batch list / add / renew / edit / rm / period，customer-page。
//! 批次账记的是「卖给客户的节点什么时候到期」，与供应商订阅无关；一个批次 = 首购一段 + 每次续期一段。
//! 批次用 `团队#编号` 指定（`团队2#05` 或 `团队2#5`）。客户页链接是只读分享链接，客户不登录，页面不出现 IP、供应商、价格、节点名

use crate::cmds::{st, Ctx};
use crate::ops::confirm;
use crate::table;
use anyhow::{anyhow, bail};
use serde_json::{json, Value};

fn state_cn(s: &str) -> &str {
    match s {
        "expired" => "已到期",
        "today" => "今天到期",
        _ => "有效",
    }
}

/// `团队2#05` → (团队2, 5)
fn split_ref(r: &str) -> anyhow::Result<(&str, i64)> {
    let bad = || anyhow!("批次要写成 团队#编号，如 团队2#05（先 fleetctl batch list <团队> 看列表）：「{r}」");
    let (team, seq) = r.trim().rsplit_once('#').ok_or_else(bad)?;
    let seq: i64 = seq.trim().parse().map_err(|_| bad())?;
    if team.trim().is_empty() {
        return Err(bad());
    }
    Ok((team.trim(), seq))
}

/// 团队的批次接口数据 `{team, today, page, batches}`，附团队 id
fn load(ctx: &Ctx, team: &str) -> anyhow::Result<(String, Value)> {
    let id = ctx.team_id(team)?;
    let d = ctx.c.get(&format!("/teams/{id}/batches"))?;
    Ok((id, d))
}

/// 找 `团队#编号` 指的批次，返回 (团队名, 批次)
fn find(ctx: &Ctx, r: &str) -> anyhow::Result<(String, Value)> {
    let (team, seq) = split_ref(r)?;
    let (_, d) = load(ctx, team)?;
    let name = st(&d, "/team/name");
    let b = d["batches"].as_array().and_then(|a| a.iter().find(|b| b["seq"].as_i64() == Some(seq))).cloned();
    b.map(|b| (name.clone(), b)).ok_or_else(|| anyhow!("{name} 里没有第{seq:02}批（fleetctl batch list {name} 看列表）"))
}

/// 团队的正式名字（用户可能写的是 id 前缀）；查不到就用原写法
fn team_name(ctx: &Ctx, id: &str, fallback: &str) -> String {
    ctx.teams().ok().and_then(|t| t.as_array().and_then(|a| a.iter().find(|x| st(x, "/id") == id).map(|x| st(x, "/name")))).unwrap_or_else(|| fallback.to_string())
}

fn span(b: &Value) -> String { format!("{} → {}", st(b, "/start_date"), st(b, "/end_date")) }

fn periods_table(b: &Value) -> String {
    let rows: Vec<Vec<String>> = b["periods"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .enumerate()
        .map(|(i, p)| vec![(i + 1).to_string(), st(p, "/start_date"), st(p, "/end_date"), format!("{} 个月", st(p, "/months")), format!("{} 个", st(p, "/count"))])
        .collect();
    table::render(&["段", "开始", "到期", "月数", "个数"], &rows)
}

/// 一个批次的详情：标题、起止、时长、节点月、说明，再列每一段（`period` 命令的段序号就是这里的序号）
fn detail(team: &str, b: &Value) -> String {
    let mut rows = vec![
        ("批次", format!("{team}#{:02} {}", b["seq"].as_i64().unwrap_or(0), st(b, "/title"))),
        ("起止", format!("{}（{}）", span(b), state_cn(&st(b, "/state")))),
        ("时长", format!("{} 个月，{} 个节点月", st(b, "/months"), st(b, "/node_months"))),
    ];
    if let Some(s) = b["summary"].as_str().filter(|s| !s.is_empty()) {
        rows.push(("自动说明", s.to_string()));
    }
    if let Some(n) = b["note"].as_str().filter(|s| !s.is_empty()) {
        rows.push(("备注", n.to_string()));
    }
    format!("{}\n\n{}", table::kv(&rows), periods_table(b))
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

pub fn list(ctx: &Ctx, team: &str, periods: bool) -> anyhow::Result<u8> {
    let (_, d) = load(ctx, team)?;
    ctx.emit(&d, || {
        let name = st(&d, "/team/name");
        let bs = d["batches"].as_array().cloned().unwrap_or_default();
        let mut out = format!("{name} · 今天 {}\n", st(&d, "/today"));
        if bs.is_empty() {
            out.push_str(&format!("还没有批次（fleetctl batch add {name} --region 美区 --start 2026-09-15 --months 1 --count 5 建一个）"));
        } else {
            let rows: Vec<Vec<String>> = bs
                .iter()
                .map(|b| {
                    vec![
                        format!("{:02}", b["seq"].as_i64().unwrap_or(0)),
                        st(b, "/region"),
                        span(b),
                        format!("{} 个", st(b, "/count")),
                        format!("{} 个月", st(b, "/months")),
                        st(b, "/node_months"),
                        state_cn(&st(b, "/state")).to_string(),
                        b["summary"].as_str().unwrap_or("").to_string(),
                    ]
                })
                .collect();
            out.push_str(&table::render(&["编号", "地区", "起止", "当前个数", "时长", "节点月", "状态", "说明"], &rows));
            let live: Vec<&Value> = bs.iter().filter(|b| b["state"] != "expired").collect();
            out.push_str(&format!(
                "\n共 {} 批，有效 {} 批，客户页上的节点月 {}",
                bs.len(),
                live.len(),
                live.iter().map(|b| b["node_months"].as_i64().unwrap_or(0)).sum::<i64>()
            ));
            if periods {
                for b in &bs {
                    out.push_str(&format!("\n\n{}\n{}", st(b, "/title"), periods_table(b)));
                }
            }
        }
        out.push_str(&format!("\n\n{}", page_text(&name, &d["page"])));
        out
    });
    Ok(0)
}

#[derive(clap::Args)]
pub struct AddOpts {
    /// 团队
    team: String,
    /// 地区：美区、墨西哥、菲律宾…（1–20 字）
    #[arg(long)]
    region: String,
    /// 开始日期（购买日），如 2026-09-15
    #[arg(long, value_name = "日期")]
    start: String,
    /// 计费月数
    #[arg(long)]
    months: i32,
    /// 节点个数
    #[arg(long)]
    count: i32,
    /// 到期日期：不给就是开始日加 --months 个月（月底对齐）
    #[arg(long, value_name = "日期")]
    end: Option<String>,
    /// 客户页标题下一行的说明（≤ 60 字）
    #[arg(long)]
    note: Option<String>,
    /// 批次编号：不给就是团队里最大的加 1
    #[arg(long)]
    seq: Option<i32>,
}

pub fn add(ctx: &Ctx, o: &AddOpts) -> anyhow::Result<u8> {
    let id = ctx.team_id(&o.team)?;
    let mut body = json!({"region": o.region, "start_date": o.start, "months": o.months, "count": o.count});
    for (k, v) in [("end_date", &o.end), ("note", &o.note)] {
        if let Some(v) = v {
            body[k] = json!(v);
        }
    }
    if let Some(s) = o.seq {
        body["seq"] = json!(s);
    }
    let b = ctx.c.post(&format!("/teams/{id}/batches"), &body)?;
    ctx.emit(&b, || format!("已新建\n{}", detail(&team_name(ctx, &id, &o.team), &b)));
    Ok(0)
}

#[derive(clap::Args)]
pub struct RenewOpts {
    /// 批次：团队#编号，如 团队2#05
    batch: String,
    /// 续期月数
    #[arg(long)]
    months: i32,
    /// 续期后的个数：不给沿用现在的
    #[arg(long)]
    count: Option<i32>,
    /// 续期后的到期日期：不给就是现在的到期日加 --months 个月
    #[arg(long, value_name = "日期")]
    end: Option<String>,
}

pub fn renew(ctx: &Ctx, o: &RenewOpts) -> anyhow::Result<u8> {
    let (team, cur) = find(ctx, &o.batch)?;
    let mut body = json!({"months": o.months});
    if let Some(c) = o.count {
        body["count"] = json!(c);
    }
    if let Some(e) = &o.end {
        body["end_date"] = json!(e);
    }
    let b = ctx.c.post(&format!("/batches/{}/renew", st(&cur, "/id")), &body)?;
    ctx.emit(&b, || format!("已续期\n{}", detail(&team, &b)));
    Ok(0)
}

#[derive(clap::Args)]
pub struct EditOpts {
    /// 批次：团队#编号
    batch: String,
    #[arg(long)]
    region: Option<String>,
    /// 说明（≤ 60 字）；给空字符串清掉
    #[arg(long)]
    note: Option<String>,
    /// 新的批次编号（团队里不能重复）
    #[arg(long)]
    seq: Option<i32>,
}

pub fn edit(ctx: &Ctx, o: &EditOpts) -> anyhow::Result<u8> {
    if o.region.is_none() && o.note.is_none() && o.seq.is_none() {
        bail!("没有要改的：给 --region、--note 或 --seq");
    }
    let (team, cur) = find(ctx, &o.batch)?;
    let mut body = json!({});
    if let Some(r) = &o.region {
        body["region"] = json!(r);
    }
    if let Some(n) = &o.note {
        body["note"] = json!(n);
    }
    if let Some(s) = o.seq {
        body["seq"] = json!(s);
    }
    let b = ctx.c.patch(&format!("/batches/{}", st(&cur, "/id")), &body)?;
    ctx.emit(&b, || format!("已保存\n{}", detail(&team, &b)));
    Ok(0)
}

pub fn rm(ctx: &Ctx, batch: &str, yes: bool) -> anyhow::Result<u8> {
    let (team, cur) = find(ctx, batch)?;
    confirm(&format!("删除 {team} 的{}（连同它的 {} 段记录，客户页上也不再显示）？", st(&cur, "/title"), cur["periods"].as_array().map_or(0, Vec::len)), yes)?;
    let d = ctx.c.delete(&format!("/batches/{}", st(&cur, "/id")))?;
    ctx.emit(&d, || format!("已删除 {team}#{:02}", cur["seq"].as_i64().unwrap_or(0)));
    Ok(0)
}

#[derive(clap::Args)]
pub struct PeriodOpts {
    /// 批次：团队#编号
    batch: String,
    /// 第几段（从 1 起，按开始日期；batch list --periods 能看到）
    index: usize,
    /// 开始日期
    #[arg(long, value_name = "日期")]
    start: Option<String>,
    /// 到期日期
    #[arg(long, value_name = "日期")]
    end: Option<String>,
    /// 计费月数
    #[arg(long)]
    months: Option<i32>,
    /// 这一段的节点个数
    #[arg(long)]
    count: Option<i32>,
    /// 删掉这一段（批次只剩一段时不能删，要删就删整个批次）
    #[arg(long, conflicts_with_all = ["start", "end", "months", "count"])]
    rm: bool,
    #[arg(long)]
    yes: bool,
}

pub fn period(ctx: &Ctx, o: &PeriodOpts) -> anyhow::Result<u8> {
    if !o.rm && o.start.is_none() && o.end.is_none() && o.months.is_none() && o.count.is_none() {
        bail!("没有要改的：给 --start、--end、--months、--count 之一，或 --rm 删掉这一段");
    }
    let (team, cur) = find(ctx, &o.batch)?;
    let periods = cur["periods"].as_array().cloned().unwrap_or_default();
    let p = o.index.checked_sub(1).and_then(|i| periods.get(i)).ok_or_else(|| anyhow!("{}#{:02} 只有 {} 段，没有第 {} 段", team, cur["seq"].as_i64().unwrap_or(0), periods.len(), o.index))?;
    let pid = st(p, "/id");
    if o.rm {
        confirm(&format!("删除 {team}#{:02} 的第 {} 段（{} → {}）？", cur["seq"].as_i64().unwrap_or(0), o.index, st(p, "/start_date"), st(p, "/end_date")), o.yes)?;
        let b = ctx.c.delete(&format!("/batch-periods/{pid}"))?;
        ctx.emit(&b, || format!("已删除第 {} 段\n{}", o.index, detail(&team, &b)));
        return Ok(0);
    }
    let mut body = json!({});
    for (k, v) in [("start_date", &o.start), ("end_date", &o.end)] {
        if let Some(v) = v {
            body[k] = json!(v);
        }
    }
    if let Some(m) = o.months {
        body["months"] = json!(m);
    }
    if let Some(c) = o.count {
        body["count"] = json!(c);
    }
    let b = ctx.c.patch(&format!("/batch-periods/{pid}"), &body)?;
    ctx.emit(&b, || format!("已保存第 {} 段\n{}", o.index, detail(&team, &b)));
    Ok(0)
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
        confirm(&format!("删除 {} 的客户页链接？用它的客户之后打不开（批次记录保留）", name()), o.yes)?;
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

    #[test]
    fn batch_ref_accepts_padded_and_plain_seq() {
        assert_eq!(split_ref("团队2#05").unwrap(), ("团队2", 5));
        assert_eq!(split_ref("团队2#5").unwrap(), ("团队2", 5));
        assert_eq!(split_ref(" 团队2 # 12 ").unwrap(), ("团队2", 12));
        for bad in ["团队2", "团队2#", "#5", "团队2#x"] {
            assert!(split_ref(bad).is_err(), "{bad}");
        }
    }
}
