//! 操作记录：查询（分类、搜索、翻页）与每种动作的中文标题、一句话摘要
use super::ok;
use crate::auth::Actor;
use crate::db::Args;
use crate::error::{ApiError, ApiQuery, ApiResult};
use crate::state::AppState;
use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use time::OffsetDateTime;
use uuid::Uuid;

/// 订阅字段的写法：(字段名, 中文名, 把值写成文字的函数)
type FieldFmt<'a> = (&'a str, &'a str, &'a dyn Fn(&Value) -> String);

#[derive(Deserialize)]
pub struct AuditQ {
    target_id: Option<Uuid>,
    limit: Option<i64>,
    /// 翻页：只要 id 小于它的（上一页最后一条的 id）
    before_id: Option<i64>,
    /// 分类：all | auth | exec | machine | config（见 `audit_cat`）
    cat: Option<String>,
    /// 搜索标题、摘要、操作人（见 `audit_search`）
    q: Option<String>,
}

/// 远程执行类的动作：在机器上跑命令、SSH 登录、网页终端、取日志（restore.probe 是已删掉的恢复预检留下的旧记录）
const EXEC_ACTIONS: &[&str] = &["exec", "ssh.login", "terminal.open", "terminal.close", "machine.logs", "server.logs", "restore.probe"];

/// 分类条件（操作记录页的分段）：登录与账户（auth.*）、远程执行（`EXEC_ACTIONS`）、
/// 机器与入口（生命周期、回收站、改名、费用续费、入口、作业、升级 agent），其余是设置与告警（目录、密钥、通知、静默、告警、访问规则）
fn audit_cat(args: &mut Args, cat: &str) -> ApiResult<String> {
    const MACHINE: &str = "action ~ '^(machine|entry|relay|subscription|agent|job|renewal|recycle)(\\.|$)'";
    let exec = args.add(EXEC_ACTIONS);
    Ok(match cat {
        "auth" => " and action like 'auth.%'".into(),
        "exec" => format!(" and action = any({exec})"),
        "machine" => format!(" and {MACHINE} and action <> all({exec})"),
        "config" => format!(" and action not like 'auth.%' and action <> all({exec}) and not {MACHINE}"),
        _ => return Err(ApiError::bad_request("cat 只能是 all、auth、exec、machine、config")),
    })
}

/// 搜索条件：只搜界面上看得到的——操作人（按界面上的写法：中控命令行、「名称（API 密钥）」）、标题（标题含关键字的动作）、
/// 写审计时存下的标题与摘要（search 列，见 `audit_search_text`），不翻 detail 里界面不展示的值。
/// search 里的命令是隐藏的（没有 exec 权限的人看到的那份，免得靠搜索试出命令内容）；有 exec 权限的另搜命令原文
fn audit_search(args: &mut Args, q: &str, can_exec: bool) -> Option<String> {
    let like = crate::access::like_pattern(q)?;
    let lower = q.trim().to_lowercase();
    let titles: Vec<&str> = AUDIT_ACTIONS.iter().copied().filter(|a| audit_text(a, &json!({})).0.to_lowercase().contains(&lower)).collect();
    let (l, t) = (args.add(like), args.add(titles));
    let command = if can_exec { format!(" or case when jsonb_typeof(detail->'command') = 'string' then detail->>'command' end ilike {l}") } else { String::new() };
    Some(format!(
        " and (case when actor = 'cli' then '中控命令行' when actor like 'apikey:%' then substr(actor, 8) || '（API 密钥）' else actor end ilike {l}
           or action = any({t}) or search ilike {l}{command})"
    ))
}

/// 没有 exec 权限的人看到的命令：只给字数（第一行也可能带口令），摘要随之隐藏
fn hide_command(d: &mut Value) {
    if let Some(c) = d.get("command").and_then(Value::as_str) {
        d["command"] = json!(format!("命令已隐藏（{} 字）", c.chars().count()));
    }
}

/// 存进 search 列的搜索文本：标题与摘要（命令隐藏后的，同没有 exec 权限的人看到的）
pub fn audit_search_text(action: &str, d: &Value) -> String {
    let mut d = d.clone();
    hide_command(&mut d);
    let (title, summary) = audit_text(action, &d);
    format!("{title}\n{summary}")
}

/// 操作记录查询的语句超时：关键词搜索沿主键倒序边扫边筛，命中少的词可能扫很多行
const AUDIT_TIMEOUT_S: u32 = 5;

#[derive(Serialize, sqlx::FromRow)]
struct AuditOut {
    id: i64,
    #[serde(with = "time::serde::rfc3339")]
    ts: OffsetDateTime,
    actor: String,
    action: String,
    target_kind: Option<String>,
    target_id: Option<Uuid>,
    detail: Value,
}

pub async fn audit(a: Actor, State(st): State<AppState>, ApiQuery(q): ApiQuery<AuditQ>) -> ApiResult<Json<Value>> {
    // 没有 exec 权限的密钥看不到命令（第一行也可能带口令）：只给字数，摘要随之隐藏
    let can_exec = a.require_scope("exec").is_ok();
    // 只拼给了的条件：按对象查时才走得上 (target_id, id desc) 索引；分类、搜索沿主键倒序边扫边筛，同样按 before_id 往前翻
    let mut args = Args::default();
    let mut w = String::from("true");
    if let Some(t) = q.target_id {
        w += &format!(" and target_id = {}", args.add(t));
    }
    if let Some(b) = q.before_id {
        w += &format!(" and id < {}", args.add(b));
    }
    match q.cat.as_deref().map(str::trim) {
        None | Some("" | "all") => {}
        Some(c) => w += &audit_cat(&mut args, c)?,
    }
    if let Some(s) = q.q.as_deref().and_then(|s| audit_search(&mut args, s, can_exec)) {
        w += &s;
    }
    let sql = format!(
        "select id, ts, actor, action, target_kind, target_id, detail from audit_log where {w} order by id desc limit {}",
        args.add(q.limit.unwrap_or(100).clamp(1, 1000))
    );
    let mut tx = st.db.begin().await?;
    sqlx::query(&format!("set local statement_timeout = '{AUDIT_TIMEOUT_S}s'")).execute(&mut *tx).await?;
    let v: Vec<AuditOut> = sqlx::query_as_with(&sql, args.0).fetch_all(&mut *tx).await.map_err(|e| match &e {
        sqlx::Error::Database(d) if d.code().as_deref() == Some("57014") => ApiError::new(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            format!("搜索超过 {AUDIT_TIMEOUT_S} 秒还没查完，已中止：换个更具体的关键词，或先按分类、对象筛选"),
        ),
        _ => e.into(),
    })?;
    tx.commit().await?;
    let out: Vec<Value> = v
        .into_iter()
        .map(|mut a| {
            if !can_exec {
                hide_command(&mut a.detail);
            }
            let (title, summary) = audit_text(&a.action, &a.detail);
            let mut j = serde_json::to_value(&a).unwrap_or_default();
            j["title"] = json!(title);
            j["summary"] = json!(summary);
            j
        })
        .collect();
    Ok(ok(out))
}

/// 所有写审计的动作（git grep 'audit(' 核对过）；新增动作时补这里和 audit_text。按标题搜索时在这里面找。
/// machine.restore、restore.probe 是已删掉的「恢复已退役机器」留下的旧记录，标题照样显示
const AUDIT_ACTIONS: &[&str] = &[
    "machine.provision", "machine.retire", "machine.recycle", "recycle.import", "machine.purge", "machine.rename", "machine.update", "machine.restore", "machine.replace", "machine.tune", "subscription.set", "renewal",
    "team.create", "team.update", "team.delete", "team.renumber", "provider.create", "provider.update", "provider.delete", "plan.create", "plan.update", "plan.delete", "entry.enable",
    "entry.disable", "entry.rotate", "entry.share", "entry.add", "entry.move", "entry.repoint", "relay.evacuate", "job.retry", "job.cancel", "silence.create",
    "silence.delete", "incident.ack", "restore.probe", "agent.upgrade", "notify.create", "notify.update", "notify.delete", "exec", "machine.logs", "server.logs", "apikey.create",
    "apikey.revoke", "auth.login", "auth.login_failed", "auth.logout", "auth.totp_enabled", "auth.totp_disabled", "auth.totp_recovery_used",
    "auth.totp_recovery_regenerated", "auth.user_removed", "auth.user_disabled", "auth.password_changed", "auth.session_revoked", "auth.verify_failed", "policy.draft", "policy.discard", "policy.publish", "policy.rollback",
    "settings.access_retention", "settings.fx_rate", "terminal.open", "terminal.close", "ssh.login",
    "router.create", "router.update", "router.delete", "router.bootstrap", "router.bootstrap_used", "router.deploy",
];

/// 审计记录的中文标题与一句话摘要（界面直接显示）
pub fn audit_text(action: &str, d: &Value) -> (String, String) {
    let s = |k: &str| d[k].as_str().unwrap_or("").to_string();
    let title = match action {
        "machine.provision" => "上架",
        "machine.retire" => "退役",
        "machine.recycle" => "移进回收站",
        "recycle.import" => "导入回收站",
        "machine.purge" => "彻底删除",
        "machine.rename" => "改名",
        "machine.update" => "修改资料",
        "subscription.set" => "修改费用",
        "renewal" => "登记续费",
        "team.create" => "新建团队",
        "team.update" => "修改团队",
        "team.delete" => "删除团队",
        "team.renumber" => "重新编号",
        "provider.create" => "新建供应商",
        "provider.update" => "修改供应商",
        "provider.delete" => "删除供应商",
        "plan.create" => "新建套餐",
        // 只改了归档的写成归档 / 取消归档
        "plan.update" => match plan_archive(d) {
            Some(true) => "归档套餐",
            Some(false) => "取消归档套餐",
            None => "修改套餐",
        },
        "plan.delete" => "删除套餐",
        "entry.enable" => "启用入口",
        "entry.disable" => "停用入口",
        "entry.rotate" => "轮换凭据",
        "entry.share" => "查看分享链接",
        "entry.add" => "添加入口",
        "entry.move" => "换线路机",
        "entry.repoint" => "改出口指向",
        "relay.evacuate" => "搬空线路机",
        "machine.restore" => "恢复机器",
        "machine.replace" => "替换节点",
        "machine.tune" => "系统调优",
        "job.retry" => "重试作业",
        "job.cancel" => "取消作业",
        // 标记为维护的叫维护窗口，否则叫静默（同界面）
        "silence.create" if d["is_maintenance"] == true => "新建维护窗口",
        "silence.delete" if d["is_maintenance"] == true => "删除维护窗口",
        "silence.create" => "新建静默",
        "silence.delete" => "删除静默",
        "incident.ack" => "确认告警",
        "restore.probe" => "恢复预检（登录机器）",
        "agent.upgrade" => "升级 agent",
        "notify.create" => "新建通知渠道",
        "notify.update" => "修改通知渠道",
        "notify.delete" => "删除通知渠道",
        "exec" => "执行命令",
        "ssh.login" => "SSH 登录",
        "machine.logs" => "查看机器日志",
        "server.logs" => "查看中控日志",
        "terminal.open" => "打开网页终端",
        "terminal.close" => "关闭网页终端",
        "apikey.create" => "新建 API 密钥",
        "apikey.revoke" => "撤销 API 密钥",
        "auth.login" => "登录",
        "auth.login_failed" => "登录失败",
        "auth.logout" => "退出登录",
        "auth.totp_enabled" => "开启两步验证",
        "auth.totp_disabled" => "关闭两步验证",
        "auth.totp_recovery_used" => "用恢复码验证",
        "auth.totp_recovery_regenerated" => "重新生成恢复码",
        "auth.user_removed" => "删除账号",
        "auth.user_disabled" => "停用账号",
        "auth.password_changed" => "修改密码",
        "auth.session_revoked" => "撤销登录会话",
        "auth.verify_failed" => "密码或验证码校验失败",
        "policy.draft" => "修改访问规则草稿",
        "policy.discard" => "丢弃访问规则草稿",
        "policy.publish" => "发布访问规则",
        "policy.rollback" => "回滚访问规则",
        "settings.access_retention" => "修改访问记录保留期",
        "settings.fx_rate" => "修改汇率",
        "router.create" => "新建软路由",
        "router.update" => "修改软路由",
        "router.delete" => "删除软路由",
        "router.bootstrap" => "重新生成接入命令",
        "router.bootstrap_used" => "路由取走接入脚本",
        "router.deploy" => "一键部署软路由",
        other => other,
    }
    .to_string();
    let summary = match action {
        "machine.provision" => format!("{} · {}", s("name"), s("ip")),
        // 「团队2-07 → 备用-03（换新机）」：进备用的写去向；立即回收的（和这一版之前的记录）只写名字
        "machine.retire" => {
            let to = if d["mode"] == "spare" { format!(" → {}", s("to")) } else { String::new() };
            match d["reason"].as_str().filter(|r| !r.is_empty()) {
                Some(r) => format!("{}{to}（{r}）", s("name")),
                None => format!("{}{to}", s("name")),
            }
        }
        // 「团队2-07（回收时 备用-03）· 备用到期自动回收」
        "machine.recycle" => {
            let name = if d["from"] == d["name"] { s("name") } else { format!("{}（回收时 {}）", s("from"), s("name")) };
            match d["auto"].as_str() {
                Some(crate::spare::AUTO) => format!("{name} · 备用到期自动回收"),
                Some(crate::spare::SWEEP) => format!("{name} · 已退役，自动收进回收站"),
                _ => name,
            }
        }
        "recycle.import" => format!("导入 {} 台，跳过 {} 台", d["inserted"].as_u64().unwrap_or(0), d["skipped"].as_u64().unwrap_or(0)),
        "machine.purge" if d["auto"] == crate::spare::AUTO => format!("{}（回收站自动清除）", s("name")),
        "machine.purge" => s("name"),
        "machine.tune" if d["rollback"] == true => format!("{} · 撤销调优", s("name")),
        "machine.tune" => format!("{} · {}", s("name"), s("profile")),
        "renewal" => {
            let auto = if d["auto"] == true { "（自动续费）" } else { "" };
            format!("到期 {} → {}{auto}", d["old_due"].as_str().unwrap_or("?"), d["new_due"].as_str().unwrap_or("?"))
        }
        // 「团队1-03 · 价格 5.00 → 6.00 USD，到期 2026-11-01 → 2026-12-01」；这一版之前的记录没有改动前后，不写摘要
        "subscription.set" if d["after"].is_object() => {
            let changes = sub_changes(d);
            match (s("name"), changes.is_empty()) {
                (n, true) => format!("{n} · 没有改动"),
                (n, false) => format!("{n} · {changes}"),
            }
        }
        "team.renumber" => {
            let ch = d["changes"].as_array().cloned().unwrap_or_default();
            let mut parts: Vec<String> = ch.iter().take(3).map(|c| format!("{}→{}", c["from"].as_str().unwrap_or(""), c["to"].as_str().unwrap_or(""))).collect();
            if ch.len() > 3 {
                parts.push(format!("等 {} 台", ch.len()));
            }
            let why = d["why"].as_str().map(|w| format!("{w}：")).unwrap_or_default();
            format!("{why}{}", parts.join("，"))
        }
        "entry.move" => format!("{} · {} → {}", s("name"), s("from"), s("to")),
        // 「线路机1-直出 · 线路机1:20010 → 线路机3-直出 · 线路机3:20000（改 2 台软路由）」
        "entry.repoint" => format!("{} → {}（改 {} 台软路由）", s("from"), s("to"), d["routers"].as_array().map_or(0, |a| a.len())),
        "relay.evacuate" => format!("{} → {}（{} 个作业）", s("from"), s("to"), d["jobs"].as_array().map_or(0, |a| a.len())),
        // 「团队2-01 由 团队2-20 接替，旧节点改为 备用-03（改 2 台软路由）」
        "machine.replace" => {
            let n = d["routers"].as_array().map_or(0, |a| a.len());
            let r = if n > 0 { format!("（改 {n} 台软路由）") } else { "（不改软路由）".into() };
            format!("{} 由 {} 接替，旧节点改为 {}{r}", s("name"), s("new"), s("old_to"))
        }
        "machine.restore" => match d["reason"].as_str().filter(|r| !r.is_empty()) {
            Some(r) => format!("{} → {}（{r}）", s("old_name"), s("name")),
            None => format!("{} → {}", s("old_name"), s("name")),
        },
        a if a.starts_with("entry.") && d["relay"].is_string() => format!("{}:{}", s("relay"), d["port"]),
        "entry.add" if d["direct"] == true => "线路机直出入口".into(),
        "provider.update" => d["after"]["name"].as_str().unwrap_or("").to_string(),
        "team.create" if d["spare"] == true => format!("{}（备用）", s("name")),
        "team.create" | "team.delete" | "provider.create" | "provider.delete" | "plan.create" | "plan.delete" | "notify.create" | "notify.update" | "notify.delete" => s("name"),
        // 「测试团队 → 测试团队改，排序 200 → 50，设为备用」：改了名写从什么改成什么，没改名只写名字。这一版之前的记录没有 spare
        "team.update" => {
            let (b, a) = (&d["before"], &d["after"]);
            let name = if b["name"] == a["name"] { val(&a["name"]) } else { format!("{} → {}", val(&b["name"]), val(&a["name"])) };
            let mut out = match (b["sort_order"].as_i64(), a["sort_order"].as_i64()) {
                (Some(x), Some(y)) if x != y => format!("{name}，排序 {x} → {y}"),
                _ => name,
            };
            match (b["spare"].as_bool(), a["spare"].as_bool()) {
                (Some(false), Some(true)) => out.push_str("，设为备用"),
                (Some(true), Some(false)) => out.push_str("，取消备用"),
                _ => {}
            }
            out
        }
        // 「ATTVPS 基础版 · 价格 59.00 → 69.00 USD，流量 800 → 1000 GB」；只改了归档的只写名字（标题已经说了归档）。这一版之前的记录没有名字
        "plan.update" => {
            let changes = plan_changes(d);
            match (s("name"), changes.is_empty() || plan_archive(d).is_some()) {
                (n, true) => n,
                (n, false) if n.is_empty() => changes,
                (n, false) => format!("{n} · {changes}"),
            }
        }
        // 「团队1-07 → 团队1-03」；修改资料写名字和改了的国家、备注（前后值；团队、编号变了算改名）。这一版之前的记录没有备注
        "machine.rename" if d["old_name"].is_string() => format!("{} → {}", s("old_name"), s("name")),
        "machine.update" if d["name"].is_string() => {
            let mut out = s("name");
            let (b, a) = (d["before"]["country"].as_str(), d["after"]["country"].as_str());
            if b != a {
                out += &format!(" · 国家 {} → {}", b.unwrap_or("自动"), a.unwrap_or("自动"));
            }
            let note = |k: &str| d[k]["note"].as_str().filter(|x| !x.is_empty()).map_or("（空）".to_string(), |x| format!("「{x}」"));
            if d["after"].get("note").is_some() && d["before"]["note"] != d["after"]["note"] && note("before") != note("after") {
                out += &format!(" · 备注 {} → {}", note("before"), note("after"));
            }
            out
        }
        // 「团队1-03」「5 台（团队1-03、团队1-04、团队1-05、…），跳过 1 台」；这一版之前的记录没有台名，只写台数
        "agent.upgrade" => {
            let n = d["count"].as_u64().unwrap_or(0);
            let names: Vec<&str> = d["names"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
            let mut s = match names.as_slice() {
                [one] if n == 1 => one.to_string(),
                [] => format!("{n} 台"),
                l => format!("{n} 台（{}{}）", l.join("、"), if n as usize > l.len() { "、…" } else { "" }),
            };
            if let Some(k) = d["skipped"].as_u64().filter(|k| *k > 0) {
                s += &format!("，跳过 {k} 台");
            }
            s
        }
        // 「换线路机 · 团队1-03」：这一版之前的记录没有作业类型，不写摘要
        "job.cancel" | "job.retry" if d["kind"].is_string() => {
            let k = crate::meta::job_kind_label(d["kind"].as_str().unwrap_or(""));
            match d["name"].as_str() {
                Some(n) => format!("{k} · {n}"),
                None => k.to_string(),
            }
        }
        "exec" => {
            // 命令只取第一行、最多 60 字
            let cmd = s("command");
            let line = cmd.lines().next().unwrap_or("");
            let mut c: String = line.chars().take(60).collect();
            if c.len() < cmd.trim_end().len() {
                c.push('…');
            }
            let res = match (d["exit_code"].as_i64(), d["timed_out"] == true) {
                _ if d["started"] == true => "执行中或中断（未记录结果）".to_string(),
                (_, true) => "超时".to_string(),
                (Some(code), _) => format!("退出码 {code}"),
                (None, _) => format!("失败：{}", s("error")),
            };
            format!("{} · {c} · {res}", s("name"))
        }
        "machine.logs" | "restore.probe" => s("name"),
        "ssh.login" => format!("{} · {}", s("name"), if d["command"] == true { "带命令（内容不记录）" } else { "交互式" }),
        "incident.ack" => format!("{} · {}", crate::meta::rule_label(d["rule"].as_str().unwrap_or("")), s("summary")),
        "server.logs" => format!("{} 行", d["lines"]),
        "apikey.create" | "apikey.revoke" if d["via"] == "local-admin" => format!("{}（{}）· 中控命令行 local-admin", s("name"), s("prefix")),
        "terminal.open" | "terminal.close" => crate::terminal::audit_summary(action, d),
        "apikey.create" | "apikey.revoke" => format!("{}（{}）", s("name"), s("prefix")),
        "auth.login" | "auth.login_failed" | "auth.totp_recovery_regenerated" | "auth.verify_failed" => s("ip"),
        "auth.totp_disabled" if d["via"] == "totp-reset" => "中控命令行 totp-reset".into(),
        "auth.totp_disabled" => s("ip"),
        "auth.user_removed" | "auth.user_disabled" => format!("{} · 中控命令行 user-del", s("username")),
        "auth.totp_recovery_used" => format!("{} · 剩 {} 个", s("ip"), d["left"]),
        "auth.password_changed" if d["revoked_api_keys"].as_u64().is_some_and(|n| n > 0) => {
            format!("{} · 撤销其它 {} 个会话、{} 个 API 密钥", s("ip"), d["revoked_sessions"], d["revoked_api_keys"])
        }
        "auth.password_changed" => format!("{} · 撤销其它 {} 个会话", s("ip"), d["revoked_sessions"]),
        "auth.session_revoked" if d["others"].is_number() => format!("退出其它设备（{} 个）", d["others"]),
        "auth.session_revoked" => s("ip"),
        // 「团队1 · 黑名单 · 2 个域名」：这一版之前的记录没有范围名
        "policy.draft" => {
            let what = if d["cleared"] == true {
                "撤销草稿".to_string()
            } else {
                let mode = crate::meta::label(crate::meta::POLICY_MODES, d["mode"].as_str().unwrap_or(""));
                format!("{mode} · {} 个域名", d["domains"])
            };
            match d["name"].as_str() {
                Some(n) => format!("{n} · {what}"),
                None => what,
            }
        }
        // 这一版之前的只记了对象 id 与是否维护，不写摘要
        "silence.create" | "silence.delete" if d["starts_at"].is_string() => {
            let at = |k: &str| {
                let t = d[k].as_str().and_then(|v| OffsetDateTime::parse(v, &time::format_description::well_known::Rfc3339).ok());
                t.map(|t| crate::meta::org_time(t).chars().take(16).collect::<String>()).unwrap_or_default()
            };
            let what = if d["is_maintenance"] == true { "维护窗口" } else { "静默" };
            let rule = d["rule_name"].as_str().map(|r| format!(" · 只静默「{r}」")).unwrap_or_default();
            let note = d["comment"].as_str().filter(|c| !c.is_empty()).map(|c| format!("（{c}）")).unwrap_or_default();
            let object = super::incidents::subject_label(d["subject_kind"].as_str(), d["subject_name"].as_str());
            format!("{object} · {what}{rule} · {} → {}{note}", at("starts_at"), at("ends_at"))
        }
        "policy.discard" => format!("{} 个草稿", d["drafts"]),
        "router.create" if d["method"] == "deploy" => format!("{} · 一键部署", s("name")),
        "router.create" | "router.update" | "router.delete" | "router.bootstrap" => s("name"),
        "router.bootstrap_used" => format!("{} · 来自 {}", s("name"), s("ip")),
        "router.deploy" => {
            let res = match d["result"].as_str() {
                _ if d["started"] == true => "进行中或中断（未记录结果）".to_string(),
                Some("ok") => "成功".to_string(),
                _ => format!("失败：{}", s("error")),
            };
            let how = if d["password"] == true { "密码" } else { "中控密钥" };
            format!("{} · {}@{}:{}（{how}）· {res}", s("name"), s("user"), s("host"), d["port"])
        }
        // 「v5 · 回滚到 v3 · 团队1：黑名单 → 仅观察；团队2-03：白名单 · 域名 2 → 3 个 · 6 台线路机」
        "policy.publish" | "policy.rollback" => {
            let ch = d["changes"].as_array().cloned().unwrap_or_default();
            let mut parts = vec![format!("v{}", d["seq"])];
            if let Some(to) = d["rollback_of_seq"].as_i64() {
                parts.push(format!("回滚到 v{to}"));
            }
            let mut what: Vec<String> = ch.iter().take(2).map(policy_change).collect();
            if ch.len() > 2 {
                what.push(format!("等 {} 处", ch.len()));
            }
            parts.push(if what.is_empty() { "只改了备注".into() } else { what.join("；") });
            if let Some(n) = d["relays"].as_u64().filter(|n| *n > 0) {
                parts.push(format!("{n} 台线路机"));
            }
            parts.join(" · ")
        }
        // 「原始记录 7 → 3 天，汇总 31 天」：改了的写从几天到几天，没改的只写天数
        "settings.access_retention" => [("raw_days", "原始记录"), ("hourly_days", "汇总")]
            .iter()
            .filter_map(|(k, label)| match (d["from"][k].as_i64(), d["to"][k].as_i64()?) {
                (Some(a), b) if a != b => Some(format!("{label} {a} → {b} 天")),
                (_, b) => Some(format!("{label} {b} 天")),
            })
            .collect::<Vec<_>>()
            .join("，"),
        // 「CNY：手动 0.1404 USD（原为自动 0.1498）」「CNY：取消手动，改回自动 0.1498」
        "settings.fx_rate" => {
            let side = |v: &Value| match (v["usd_per_unit"].as_f64(), v["source"].as_str()) {
                (Some(r), Some(src)) => format!("{}{r} USD", if src == "manual" { "手动 " } else { "自动 " }),
                _ => "没有汇率（未折算）".into(),
            };
            let c = s("currency");
            if d["to"]["source"] == "manual" {
                format!("{c}：1 {c} = {}（原为{}）", side(&d["to"]).trim_start_matches("手动 "), side(&d["from"]))
            } else {
                format!("{c}：取消手动汇率，改回{}", side(&d["to"]))
            }
        }
        _ => String::new(),
    };
    (title, summary)
}

/// subscription.set 改了的生效值（before / after）；旧记录另写按面板校准的已用流量（接口已不收，0040 之前的审计里还有）
fn sub_changes(d: &Value) -> String {
    let (b, a) = (&d["before"], &d["after"]);
    if b.is_null() {
        return format!("新建订阅，到期 {}", val(&a["due_date"]));
    }
    let cur = a["currency"].as_str().unwrap_or("");
    let money = |v: &Value| v.as_i64().map_or("—".into(), |c| format!("{:.2}", c as f64 / 100.0));
    let gb = |v: &Value| v.as_i64().filter(|g| *g > 0).map_or("不限".into(), |g| format!("{g} GB"));
    let cycle = |v: &Value| v.as_str().and_then(|k| crate::meta::CYCLES.iter().find(|c| c.key == k)).map_or_else(|| val(v), |c| c.label.to_string());
    let onoff = |v: &Value| if v == true { "开" } else { "关" }.to_string();
    let fields: [FieldFmt; 9] = [
        ("due_date", "到期", &val),
        ("price_cents", "价格", &money),
        ("currency", "币种", &val),
        ("cycle", "周期", &cycle),
        ("traffic_gb", "流量", &gb),
        ("traffic_reset_day", "流量重置日", &|v| format!("{} 号", val(v))),
        ("remind_days", "提前提醒", &|v| format!("{} 天", val(v))),
        ("billing_tz", "账单时区", &val),
        ("auto_renew", "自动续费", &onoff),
    ];
    let mut out: Vec<String> = fields
        .iter()
        .filter(|(k, ..)| b[*k] != a[*k])
        .map(|(k, label, f)| match *k {
            // 同时换了币种：旧价格按旧币种写
            "price_cents" => match b["currency"].as_str().filter(|c| *c != cur) {
                Some(old) => format!("{label} {} {old} → {} {cur}", f(&b[*k]), f(&a[*k])),
                None => format!("{label} {} → {} {cur}", f(&b[*k]), f(&a[*k])),
            },
            _ => format!("{label} {} → {}", f(&b[*k]), f(&a[*k])),
        })
        .collect();
    match d.get("traffic_used_gb") {
        Some(Value::Null) => out.push("清除已用流量校准".into()),
        Some(v) if v.is_number() => out.push(format!("已用流量校准为 {v} GB")),
        _ => {}
    }
    out.join("，")
}

/// 访问规则版本里的一处变更（同前端发布记录的写法）：模式变了写「团队1：黑名单 → 仅观察」，没变写域名、备注改了什么：
/// 「黑名单 · 域名 2 → 3 个」「黑名单 · 换了域名（2 个），改了备注」「黑名单 · 改了备注」。
/// 没有 domains_changed / note_changed 的旧记录：用域名的模式算改了域名，不用的算改了备注
fn policy_change(c: &Value) -> String {
    let (f, t) = (&c["from"], &c["to"]);
    let mode = |v: &Value| crate::meta::label(crate::meta::POLICY_MODES, v["mode"].as_str().unwrap_or("")).to_string();
    let uses = |v: &Value| matches!(v["mode"].as_str(), Some("allowlist" | "denylist"));
    let what = if f["mode"] != t["mode"] {
        let n = if uses(t) { format!("（{} 个域名）", t["domains"]) } else { String::new() };
        format!("{} → {}{n}", mode(f), mode(t))
    } else {
        let mut parts = vec![];
        if uses(t) && c["domains_changed"].as_bool().unwrap_or(true) {
            parts.push(if f["domains"] == t["domains"] {
                format!("换了域名（{} 个）", t["domains"])
            } else {
                format!("域名 {} → {} 个", f["domains"], t["domains"])
            });
        }
        if c["note_changed"].as_bool().unwrap_or(!uses(t)) {
            parts.push("改了备注".into());
        }
        format!("{} · {}", mode(t), parts.join("，"))
    };
    format!("{}：{what}", c["name"].as_str().unwrap_or(""))
}

/// JSON 值的显示：字符串原样，null 写「—」，其余（数字）照写
fn val(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "—".into(),
        v => v.to_string(),
    }
}

/// plan.update 是不是只改了归档：Some(true) 归档、Some(false) 取消归档、None 还改了别的
fn plan_archive(d: &Value) -> Option<bool> {
    let a = d["after"].as_object()?;
    (a.len() == 1).then(|| a.get("archived_at").map(|v| !v.is_null())).flatten()
}

/// plan.update 改了的字段：「价格 59.00 → 69.00 USD，流量 800 → 1000 GB」
fn plan_changes(d: &Value) -> String {
    let (b, a) = (&d["before"], &d["after"]);
    let Some(keys) = a.as_object() else { return String::new() };
    let cur = d["currency"].as_str().unwrap_or("");
    let money = |v: &Value| v.as_i64().map_or("—".into(), |c| format!("{:.2}", c as f64 / 100.0));
    let gb = |v: &Value| v.as_i64().filter(|g| *g > 0).map_or("不限".into(), |g| format!("{g}"));
    let cycle = |v: &Value| v.as_str().and_then(|k| crate::meta::CYCLES.iter().find(|c| c.key == k)).map_or_else(|| val(v), |c| c.label.to_string());
    keys.keys()
        .filter_map(|k| {
            let (x, y) = (&b[k.as_str()], &a[k.as_str()]);
            Some(match k.as_str() {
                "name" => format!("改名 {} → {}", val(x), val(y)),
                "price_cents" => format!("价格 {} → {} {cur}", money(x), money(y)),
                "currency" => format!("币种 {} → {}", val(x), val(y)),
                "cycle" => format!("周期 {} → {}", cycle(x), cycle(y)),
                "traffic_gb" => format!("流量 {} → {} GB", gb(x), gb(y)),
                "bandwidth_mbps" => format!("带宽 {} → {} Mbps", val(x), val(y)),
                "cpu_cores" => format!("CPU {} → {} 核", val(x), val(y)),
                "mem_mb" => format!("内存 {} → {} MiB", val(x), val(y)),
                "disk_gb" => format!("磁盘 {} → {} GiB", val(x), val(y)),
                "provider_id" => "换了供应商".into(),
                "notes" => "改了备注".into(),
                "archived_at" => if y.is_null() { "取消归档" } else { "归档" }.into(),
                _ => return None,
            })
        })
        .collect::<Vec<_>>()
        .join("，")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_audit_action_has_title() {
        for &a in AUDIT_ACTIONS {
            assert_ne!(audit_text(a, &json!({})).0, a, "{a} 没有中文标题");
        }
    }

    #[test]
    fn retention_summary_says_from_and_to() {
        let s = |from: Value, to: Value| audit_text("settings.access_retention", &json!({"from": from, "to": to})).1;
        assert_eq!(s(json!({"raw_days": 7, "hourly_days": 31}), json!({"raw_days": 3, "hourly_days": 31})), "原始记录 7 → 3 天，汇总 31 天");
        assert_eq!(s(json!({"raw_days": 7, "hourly_days": 31}), json!({"raw_days": 7, "hourly_days": 60})), "原始记录 7 天，汇总 31 → 60 天");
        assert_eq!(s(json!({"raw_days": 7, "hourly_days": 31}), json!({"raw_days": 1, "hourly_days": 14})), "原始记录 7 → 1 天，汇总 31 → 14 天");
    }

    #[test]
    fn fx_rate_summary() {
        let s = |d: Value| audit_text("settings.fx_rate", &d).1;
        let m = json!({"usd_per_unit": 0.14, "source": "manual"});
        let a = json!({"usd_per_unit": 0.1498, "source": "auto"});
        assert_eq!(s(json!({"currency": "CNY", "from": a, "to": m})), "CNY：1 CNY = 0.14 USD（原为自动 0.1498 USD）");
        assert_eq!(s(json!({"currency": "CNY", "from": m, "to": a})), "CNY：取消手动汇率，改回自动 0.1498 USD");
        assert_eq!(s(json!({"currency": "XYZ", "from": m, "to": null})), "XYZ：取消手动汇率，改回没有汇率（未折算）");
    }

    #[test]
    fn catalog_and_machine_summaries_name_the_object() {
        let t = |a: &str, d: Value| audit_text(a, &d);
        assert_eq!(t("team.update", json!({"before": {"name": "甲", "sort_order": 200}, "after": {"name": "甲改", "sort_order": 50}})).1, "甲 → 甲改，排序 200 → 50");
        assert_eq!(t("team.update", json!({"before": {"name": "甲", "sort_order": 1}, "after": {"name": "甲", "sort_order": 2}})).1, "甲，排序 1 → 2");
        let arch = json!({"name": "ATTVPS 基础版", "currency": "USD", "before": {"archived_at": null}, "after": {"archived_at": "2026-10-22T00:00:00Z"}});
        assert_eq!(t("plan.update", arch), ("归档套餐".to_string(), "ATTVPS 基础版".to_string()));
        let (title, s) = t("plan.update", json!({"name": "季付", "currency": "USD", "before": {"price_cents": 5900, "traffic_gb": 800}, "after": {"price_cents": 6900, "traffic_gb": 1000}}));
        assert_eq!((title.as_str(), s.as_str()), ("修改套餐", "季付 · 价格 59.00 → 69.00 USD，流量 800 → 1000 GB"));
        assert_eq!(t("machine.rename", json!({"name": "团队1-03", "old_name": "团队1-07"})).1, "团队1-07 → 团队1-03");
        let upd = |b: Value, a: Value| t("machine.update", json!({"name": "团队1-03", "before": b, "after": a})).1;
        assert_eq!(upd(json!({"country": null, "note": "老机器"}), json!({"country": "JP", "note": ""})), "团队1-03 · 国家 自动 → JP · 备注 「老机器」 → （空）");
        assert_eq!(upd(json!({"country": "JP"}), json!({"country": "JP"})), "团队1-03", "这一版之前的记录没有备注");
        assert_eq!(t("policy.draft", json!({"name": "团队1", "mode": "denylist", "domains": 2, "cleared": false})).1, "团队1 · 黑名单 · 2 个域名");
        let ch = |name: &str, f: &str, fd: i64, to: &str, td: i64| json!({"name": name, "from": {"mode": f, "domains": fd}, "to": {"mode": to, "domains": td}});
        let changes = json!([ch("团队1", "denylist", 2, "observe", 0), ch("团队2-03", "allowlist", 2, "allowlist", 3)]);
        assert_eq!(
            t("policy.publish", json!({"seq": 5, "rollback_of_seq": null, "changes": changes, "relays": 6})).1,
            "v5 · 团队1：黑名单 → 仅观察；团队2-03：白名单 · 域名 2 → 3 个 · 6 台线路机"
        );
        assert_eq!(t("policy.rollback", json!({"seq": 6, "rollback_of_seq": 4, "changes": [ch("团队1", "observe", 0, "denylist", 2)], "relays": 0})).1, "v6 · 回滚到 v4 · 团队1：仅观察 → 黑名单（2 个域名）");
        // 同模式同个数：分得清只改了备注、换了域名、两样都改
        let same = |dc: bool, nc: bool| {
            let mut c = ch("团队1", "denylist", 2, "denylist", 2);
            c["domains_changed"] = json!(dc);
            c["note_changed"] = json!(nc);
            t("policy.publish", json!({"seq": 7, "changes": [c], "relays": 0})).1
        };
        assert_eq!(same(false, true), "v7 · 团队1：黑名单 · 改了备注");
        assert_eq!(same(true, false), "v7 · 团队1：黑名单 · 换了域名（2 个）");
        assert_eq!(same(true, true), "v7 · 团队1：黑名单 · 换了域名（2 个），改了备注");
        assert_eq!(t("silence.create", json!({"is_maintenance": true})).0, "新建维护窗口");
        let sub = |b: Value, a: Value| t("subscription.set", json!({"name": "C-01", "before": b, "after": a})).1;
        let b = json!({"due_date": "2026-11-01", "price_cents": 500, "currency": "USD", "cycle": "monthly", "traffic_gb": null, "remind_days": 7, "auto_renew": false});
        let mut a = b.clone();
        for (k, v) in [("due_date", json!("2026-12-01")), ("price_cents", json!(600)), ("traffic_gb", json!(1000)), ("auto_renew", json!(true))] {
            a[k] = v;
        }
        assert_eq!(sub(b.clone(), a), "C-01 · 到期 2026-11-01 → 2026-12-01，价格 5.00 → 6.00 USD，流量 不限 → 1000 GB，自动续费 关 → 开");
        assert_eq!(sub(b.clone(), b.clone()), "C-01 · 没有改动");
        let mut a = b.clone();
        a["price_cents"] = json!(4000);
        a["currency"] = json!("CNY");
        assert_eq!(sub(b.clone(), a), "C-01 · 价格 5.00 USD → 40.00 CNY，币种 USD → CNY", "旧价格用旧币种");
        assert_eq!(t("subscription.set", json!({"due_date": "2026-11-01"})).1, "", "旧记录没有改动前后");
        assert_eq!(t("agent.upgrade", json!({"count": 12, "skipped": 1})).1, "12 台，跳过 1 台");
        assert_eq!(t("agent.upgrade", json!({"count": 1, "skipped": 0, "names": ["团队1-03"]})).1, "团队1-03");
        let three = json!(["团队1-03", "团队1-04", "线路机2"]);
        assert_eq!(t("agent.upgrade", json!({"count": 5, "skipped": 1, "names": three})).1, "5 台（团队1-03、团队1-04、线路机2、…），跳过 1 台");
        assert_eq!(t("agent.upgrade", json!({"count": 3, "skipped": 0, "names": three})).1, "3 台（团队1-03、团队1-04、线路机2）");
        assert_eq!(t("job.cancel", json!({"kind": "move_entry", "name": "模拟团队C-21"})), ("取消作业".to_string(), "换线路机 · 模拟团队C-21".to_string()));
        assert_eq!(t("job.retry", json!({"kind": "upgrade_agent", "name": null, "step": 0})).1, "升级 agent");
        assert_eq!(t("machine.tune", json!({"name": "团队1-03", "profile": "tune-v1"})), ("系统调优".to_string(), "团队1-03 · tune-v1".to_string()));
        assert_eq!(t("machine.tune", json!({"name": "团队1-03", "rollback": true})).1, "团队1-03 · 撤销调优");
        assert_eq!(t("job.cancel", json!({})).1, "");
    }

    #[test]
    fn exec_summary_shows_command_and_exit_code() {
        let long = format!("echo {}\nsecond line", "x".repeat(100));
        let (_, s) = audit_text("exec", &json!({"name": "团队1-01", "command": long, "exit_code": 3, "timed_out": false}));
        assert!(s.starts_with("团队1-01 · echo x") && s.contains("…") && s.ends_with("退出码 3") && !s.contains("second"), "{s}");
        let (_, s) = audit_text("exec", &json!({"name": "n", "command": "uptime", "exit_code": 0, "timed_out": false}));
        assert_eq!(s, "n · uptime · 退出码 0");
        let (_, s) = audit_text("exec", &json!({"name": "n", "command": "uptime", "error": "连不上"}));
        assert_eq!(s, "n · uptime · 失败：连不上");
    }
}
