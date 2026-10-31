//! 客户端订阅：sub ls / add / edit / rotate / rm / show（都要 secrets 权限，同 share）。
//! 订阅是给自己的客户端（Clash Verge、OpenClash 的 proxy-provider、v2rayN / Shadowrocket）按一个链接拉入口列表：
//! 按规则（团队、指定机器）在请求时现算，替换节点、换线路机、轮换凭据后客户端更新订阅就是新的。链接只在 add / rotate 时输出一次。
//! 协议（--protocols）：reality（默认）/ anytls / both；AnyTLS 条目只出在 sing-box 在跑的线路机上的入口

use crate::cmds::{st, Ctx};
use crate::ops::confirm;
use crate::resolve::{self, Want};
use crate::table;
use anyhow::anyhow;
use serde_json::{json, Value};

/// 规则参数：add 时在默认规则（全部团队、不含备用团队、含线路机直出）上改，edit 时在现有规则上改；没给的不变
#[derive(clap::Args)]
pub struct RuleArgs {
    /// 只要这些团队（逗号分隔；列了备用团队就带上备用）
    #[arg(long, value_name = "团队,…", conflicts_with_all = ["except_teams", "all_teams"])]
    only_teams: Option<String>,
    /// 全部团队（不含备用团队），除了这些（逗号分隔）
    #[arg(long, value_name = "团队,…", conflicts_with = "all_teams")]
    except_teams: Option<String>,
    /// 全部团队（不含备用团队）：默认就是
    #[arg(long)]
    all_teams: bool,
    /// 不带线路机直出入口
    #[arg(long, conflicts_with = "direct")]
    no_direct: bool,
    /// 带线路机直出入口（默认带）
    #[arg(long)]
    direct: bool,
    /// 另外带上的机器（逗号分隔：节点，或线路机 = 它的直出入口），不看团队；none 清空
    #[arg(long, value_name = "机器,…")]
    include: Option<String>,
    /// 一律不带的机器（逗号分隔），优先于其余规则；none 清空
    #[arg(long, value_name = "机器,…")]
    exclude: Option<String>,
}

/// 协议的中文
fn proto_cn(p: &str) -> &str {
    match p {
        "anytls" => "只 AnyTLS",
        "both" => "Reality + AnyTLS",
        _ => "Reality",
    }
}

fn split(s: &str) -> Vec<&str> { s.split([',', '，']).map(str::trim).filter(|x| !x.is_empty()).collect() }

impl RuleArgs {
    /// 在 `base` 上应用给了的参数
    fn apply(&self, ctx: &Ctx, mut base: Value) -> anyhow::Result<Value> {
        let teams = |s: &str| split(s).into_iter().map(|t| ctx.team_id(t)).collect::<anyhow::Result<Vec<_>>>();
        let machines = |s: &str| -> anyhow::Result<Vec<String>> {
            if s.trim() == "none" {
                return Ok(vec![]);
            }
            split(s).into_iter().map(|m| ctx.find_id(m, Want::Live).map(|x| x.0)).collect()
        };
        if let Some(t) = &self.only_teams {
            base["team_mode"] = json!("only");
            base["team_ids"] = json!(teams(t)?);
        } else if let Some(t) = &self.except_teams {
            base["team_mode"] = json!("except");
            base["team_ids"] = json!(teams(t)?);
        } else if self.all_teams {
            base["team_mode"] = json!("all");
            base["team_ids"] = json!([]);
        }
        if self.no_direct || self.direct {
            base["direct"] = json!(self.direct);
        }
        if let Some(m) = &self.include {
            base["include_ids"] = json!(machines(m)?);
        }
        if let Some(m) = &self.exclude {
            base["exclude_ids"] = json!(machines(m)?);
        }
        Ok(base)
    }
}

/// 找一个订阅：id、id 前缀（至少 6 位）、名称（不分大小写）
fn find(ctx: &Ctx, q: &str) -> anyhow::Result<Value> {
    let all = ctx.list("/client-subs")?;
    let name = |v: &Value| st(v, "/name");
    resolve::named(&all, q, "订阅", &[&name], &name).cloned().map_err(|e| anyhow!("{e}（先 fleetctl sub ls 看列表）"))
}

fn fetched(v: &Value) -> String {
    match v["last_fetch_at"].as_str() {
        Some(t) => format!("{}（{}）", table::ts(t), v["last_fetch_ip"].as_str().unwrap_or("-")),
        None => "还没拉过".into(),
    }
}

/// 新建 / 轮换后的链接（只这一次）与两种格式
fn link_text(v: &Value) -> String {
    let url = st(v, "/url");
    format!(
        "链接只显示这一次，请保存好（丢了只能轮换）：\n  Clash Verge / OpenClash（proxy-provider）：{url}\n  v2rayN / Shadowrocket（base64）：{url}?format=base64"
    )
}

pub fn ls(ctx: &Ctx) -> anyhow::Result<u8> {
    let all = ctx.list("/client-subs")?;
    ctx.emit(&json!(all), || {
        if all.is_empty() {
            return "还没有客户端订阅（fleetctl sub add <名称> 建一个）".into();
        }
        let rows: Vec<Vec<String>> = all
            .iter()
            .map(|v| {
                vec![
                    st(v, "/name"),
                    st(v, "/rule_text"),
                    proto_cn(&st(v, "/protocols")).to_string(),
                    format!("{} 条", st(v, "/items")),
                    if v["enabled"] == true { "启用".into() } else { "已停用".into() },
                    fetched(v),
                    st(v, "/id").chars().take(13).collect(),
                ]
            })
            .collect();
        table::render(&["名称", "规则", "协议", "入口", "状态", "最近拉取（来源）", "id"], &rows)
    });
    Ok(0)
}

pub fn add(ctx: &Ctx, name: &str, rule: &RuleArgs, protocols: Option<&str>) -> anyhow::Result<u8> {
    let r = rule.apply(ctx, json!({}))?;
    let mut body = json!({"name": name, "rule": r});
    if let Some(p) = protocols {
        body["protocols"] = json!(p);
    }
    let d = ctx.c.post("/client-subs", &body)?;
    ctx.emit(&d, || format!("已建订阅「{}」：{}，{}，现在 {} 个入口\n{}", st(&d, "/name"), st(&d, "/rule_text"), proto_cn(&st(&d, "/protocols")), st(&d, "/items"), link_text(&d)));
    Ok(0)
}

pub fn edit(ctx: &Ctx, q: &str, name: Option<&str>, rule: &RuleArgs, enabled: Option<bool>, protocols: Option<&str>) -> anyhow::Result<u8> {
    let cur = find(ctx, q)?;
    let mut body = json!({"rule": rule.apply(ctx, cur["rule"].clone())?});
    if let Some(n) = name {
        body["name"] = json!(n);
    }
    if let Some(e) = enabled {
        body["enabled"] = json!(e);
    }
    if let Some(p) = protocols {
        body["protocols"] = json!(p);
    }
    let d = ctx.c.patch(&format!("/client-subs/{}", st(&cur, "/id")), &body)?;
    ctx.emit(&d, || {
        let state = if d["enabled"] == true { "" } else { "（已停用：拉取回 404，令牌不变）" };
        format!("已保存「{}」：{}，{}，现在 {} 个入口{state}", st(&d, "/name"), st(&d, "/rule_text"), proto_cn(&st(&d, "/protocols")), st(&d, "/items"))
    });
    Ok(0)
}

pub fn rotate(ctx: &Ctx, q: &str, yes: bool) -> anyhow::Result<u8> {
    let cur = find(ctx, q)?;
    confirm(&format!("轮换「{}」的链接？旧链接立即失效，用它的客户端要换成新链接", st(&cur, "/name")), yes)?;
    let d = ctx.c.post(&format!("/client-subs/{}/rotate", st(&cur, "/id")), &json!({}))?;
    ctx.emit(&d, || format!("已轮换「{}」，旧链接已失效\n{}", st(&d, "/name"), link_text(&d)));
    Ok(0)
}

pub fn rm(ctx: &Ctx, q: &str, yes: bool) -> anyhow::Result<u8> {
    let cur = find(ctx, q)?;
    confirm(&format!("删除订阅「{}」？用它的客户端之后拉不到", st(&cur, "/name")), yes)?;
    let d = ctx.c.delete(&format!("/client-subs/{}", st(&cur, "/id")))?;
    ctx.emit(&d, || format!("已删除订阅「{}」", st(&cur, "/name")));
    Ok(0)
}

/// 规则与现在的条目（客户端里的名字、走哪台线路机哪个端口、AnyTLS 端口）；不显示链接
pub fn show(ctx: &Ctx, q: &str) -> anyhow::Result<u8> {
    let id = st(&find(ctx, q)?, "/id");
    let d = ctx.c.get(&format!("/client-subs/{id}"))?;
    ctx.emit(&d, || {
        let mut s = table::kv(&[
            ("名称", st(&d, "/name")),
            ("规则", st(&d, "/rule_text")),
            ("协议", proto_cn(&st(&d, "/protocols")).to_string()),
            ("状态", if d["enabled"] == true { "启用".into() } else { "已停用".into() }),
            ("最近拉取", fetched(&d)),
            ("链接生成于", table::ts(&st(&d, "/token_at"))),
            ("id", st(&d, "/id")),
        ]);
        let es = d["entries"].as_array().cloned().unwrap_or_default();
        if es.is_empty() {
            s.push_str("\n\n现在没有条目");
        } else {
            // AnyTLS 列：这份订阅出 AnyTLS 时有端口的那条才出 AnyTLS（线路机 sing-box 没在跑的为「-」）
            let at = d["protocols"].as_str().is_some_and(|p| p != "reality");
            let rows: Vec<Vec<String>> = es
                .iter()
                .map(|e| {
                    let mut r = vec![st(e, "/name"), format!("{}:{}", st(e, "/relay"), st(e, "/port"))];
                    if at {
                        r.push(e["anytls_port"].as_u64().map_or("-".into(), |p| p.to_string()));
                    }
                    r
                })
                .collect();
            let head: &[&str] = if at { &["客户端里的名字", "线路机:端口", "AnyTLS 端口"] } else { &["客户端里的名字", "线路机:端口"] };
            s.push_str(&format!("\n\n{} 个入口：\n{}", es.len(), table::render(head, &rows)));
        }
        s
    });
    Ok(0)
}
