//! 替换节点（replace）的步骤：改软路由出口 → 交换编号。发起时按快照算好的计划（改哪些路由的哪些出口、旧节点去哪）存在作业参数里，
//! 见 api/replace.rs；路由上的脚本见 routers/replace.rs。
//! 换线路机（move_entry）在验收新入口之后、摘旧入口之前也走这里的「改软路由出口」（计划见 api/lifecycle.rs，新入口取参数 `entry_id`）。
//!
//! 改路由：要改哪些出口按执行时路由的配置重新确定（经隧道读源配置，同一套匹配：api::replace::replace_exit / api::lifecycle::move_exit），
//! 发起时的计划只作展示。不在线的路由按最近一次采集看，有出口要改就一台都不改。
//! 同一台路由同时只有一个作业在改（替换、换线路机、重试都一样，见 `Locks`）：别的作业在改就排队（Outcome::Queue），作业的其余步骤照常并行。
//! 再一台一台改（备份 → 改出口 → 重启 OpenClash → 测延迟）。哪台失败了，它自己已经换回原配置（脚本做），之前改好的也按各自的备份换回去，
//! 作业失败：路由保持一致，修好后重试从头改。换回也失败的记在参数 `routers_stuck`，这时作业不能取消（jobs::cancel_blocker），人工处理好后重试。
//! 改过的路由、备份的时间戳与要改的出口记在参数 `routers_applied`（中控在这一步中途重启、接着做时照这份重做，回滚仍用第一次改之前的那份备份）。
//! 凭据（uuid、公钥、short-id）执行时才从库里取，只进 ssh 的 stdin，不进参数、日志与审计。

use super::*;
use crate::api::lifecycle::{move_exit, relay_addr};
use crate::api::replace::{replace_entries, replace_exit, Ent, ExitPlan, RouterPlan};
use crate::routers::{self, clash, replace as rr, tunnel};

#[derive(Clone, Serialize, Deserialize)]
struct Applied {
    id: Uuid,
    name: String,
    port: i32,
    /// 这台第一次改之前的备份（回滚用它）
    stamp: String,
    /// 这台要改的出口：中控在这一步中途重启、接着做时照它重做（路由的配置可能已经改好，不再按配置重新确定）
    #[serde(default)]
    exits: Vec<ExitPlan>,
}

/// 新入口现在的参数（执行时从库里取）
#[derive(sqlx::FromRow)]
struct NewEntry {
    client_uuid: Uuid,
    short_id: String,
    port: i32,
    reality_sni: String,
    reality_public_key: String,
}

/// 出口要不要改、改成什么：按库里现在的入口算，同预览
enum Pick {
    /// 换线路机：旧入口 → 新入口 (id, 端口)；旧线路机的 IP，新线路机的 (IP, public_host)
    Move { old_entry: Uuid, new: (Uuid, u16), from_ip: String, to: (String, String) },
    /// 替换：旧节点没删除的入口 → 新节点在用的入口
    Replace { old: Vec<Ent>, new: Vec<Ent> },
}

impl Pick {
    async fn load(c: &Ctx<'_>, moving: bool) -> anyhow::Result<Pick> {
        if !moving {
            let mut conn = c.st.db.acquire().await?;
            let (old, new) = replace_entries(&mut conn, c.target_id()?, c.param_uuid("new_id")?).await?;
            return Ok(Pick::Replace { old, new });
        }
        let eid = c.param_uuid("entry_id")?;
        let port: Option<i32> = sqlx::query_scalar("select port from entry where id = $1 and node_id = $2 and lifecycle = 'active'")
            .bind(eid)
            .bind(c.target_id()?)
            .fetch_optional(&c.st.db)
            .await?;
        let Some(port) = port else { bail!("新入口已经不在用了：重新发起换线路机") };
        let (from_ip, _) = relay_addr(&c.st.db, c.param_uuid("from_relay_id")?).await?;
        let to = relay_addr(&c.st.db, c.param_uuid("to_relay_id")?).await?;
        Ok(Pick::Move { old_entry: c.param_uuid("old_entry_id")?, new: (eid, port as u16), from_ip, to })
    }

    fn exit(&self, er: &clash::EntryRef, srv: &str, port: u16) -> Option<Result<ExitPlan, String>> {
        match self {
            Pick::Move { old_entry, new, from_ip, to } => move_exit(er, *old_entry, Some(*new), from_ip, to, srv, port).map(Ok),
            Pick::Replace { old, new } => replace_exit(old, new, er, srv, port),
        }
    }
}

/// 同一台软路由同时只有一个作业在改：PG 会话级 advisory lock，键由路由 id 算出。用一条不还给连接池的连接，
/// 读配置 → 改 → 重启 → 测 / 换回期间持有；这一步结束（含出错、超时被丢弃、中控退出）连接关掉，锁随之放开。
/// 一个作业要改几台时按 id 顺序加锁，拿不到就全部放开去排队，不会互相等死
struct Locks(sqlx::PgConnection);

impl Locks {
    async fn new(c: &Ctx<'_>) -> anyhow::Result<Self> { Ok(Locks(c.st.db.acquire().await?.detach())) }

    /// `wait`：等到拿到为止（中控在这一步中途重启、接着做：之前改了一半的路由不能让给别人）；否则拿不到返回 false
    async fn take(&mut self, id: Uuid, wait: bool) -> anyhow::Result<bool> {
        if wait {
            sqlx::query("select pg_advisory_lock(hashtextextended('router:' || $1::text, 0))").bind(id).execute(&mut self.0).await?;
            return Ok(true);
        }
        Ok(sqlx::query_scalar("select pg_try_advisory_lock(hashtextextended('router:' || $1::text, 0))").bind(id).fetch_one(&mut self.0).await?)
    }

    async fn release(&mut self, id: Uuid) -> anyhow::Result<()> {
        sqlx::query("select pg_advisory_unlock(hashtextextended('router:' || $1::text, 0))").bind(id).execute(&mut self.0).await?;
        Ok(())
    }
}

/// 脚本自己说的原因（`fleet：` 开头的行），没有就用 stderr 的最后几行
fn reason(o: &Output) -> String {
    let own: Vec<&str> = o.stderr.lines().filter_map(|l| l.strip_prefix("fleet：")).collect();
    if !own.is_empty() {
        return own.join("；");
    }
    let tail: Vec<&str> = o.stderr.lines().rev().filter(|l| !l.trim().is_empty()).take(3).collect();
    format!("退出码 {}：{}", o.code, tail.into_iter().rev().collect::<Vec<_>>().join(" / "))
}

/// ssh 没连上就失败了（连接、握手、登录、主机公钥这几关，脚本一行都没跑）：这台路由上什么都没动，不用换回
fn never_ran(o: &Output) -> bool {
    const BEFORE: &[&str] = &["connect to host", "banner exchange", "kex_exchange_identification", "Connection refused", "No route to host", "Could not resolve", "Network is unreachable"];
    o.code == 255
        && o.stdout.trim().is_empty()
        && (matches!(o.ssh_kind(), Some(ErrKind::HostKey | ErrKind::Auth)) || BEFORE.iter().any(|p| o.stderr.contains(p)))
}

/// 经隧道读路由现在的 OpenClash 源配置，解析出口
async fn read_config(c: &Ctx<'_>, r: &routers::Row) -> Result<Vec<clash::Proxy>, String> {
    let o = ssh::exec(tunnel::via_tunnel(&c.st.cfg, r.id, r.tunnel_port), &rr::read_script(), rr::READ_TIMEOUT).await.map_err(|e| format!("{e:#}"))?;
    if o.code == 255 {
        return Err(tunnel::explain(&o, true));
    }
    if !o.ok() {
        return Err(reason(&o));
    }
    if o.stdout.len() > clash::MAX_CONFIG {
        return Err("配置文件超过 1 MiB，不解析".into());
    }
    clash::parse(&o.stdout)
}

/// 配置里要改的出口：对上的入口属于 `node`、没删除，且 `pick` 说要改的
fn exits_in(proxies: &[clash::Proxy], entries: &[clash::Entry], node: Uuid, pick: &Pick) -> Result<Vec<ExitPlan>, String> {
    let mut out = vec![];
    for p in proxies {
        let (Some(er), _) = clash::match_entry(p, entries) else { continue };
        let (Some(srv), Some(port)) = (p.server.as_deref(), p.port) else { continue };
        if er.node_id != node || er.lifecycle == "removed" {
            continue;
        }
        match pick.exit(&er, srv, port) {
            Some(Ok(e)) => out.push(ExitPlan { exit: p.name.clone(), ..e }),
            Some(Err(why)) => return Err(format!("出口「{}」：{why}", p.name)),
            None => {}
        }
    }
    Ok(out)
}

/// 最近一次采集里这台路由有没有要改的出口（不在线、配置读不了时只能看它）
fn snap_uses(r: &routers::Row, node: Uuid, pick: &Pick) -> bool {
    let Some(snap) = r.snapshot() else { return false };
    let used = routers::exits_to(&snap, node).any(|(x, er)| matches!((x.server.as_deref(), x.port), (Some(s), Some(p)) if pick.exit(er, s, p).is_some()));
    used
}

pub(super) async fn routers(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    if c.job.params["routers"] == false {
        return Ok(Outcome::Skipped("选择了不改软路由".into()));
    }
    let moving = c.job.kind == "move_entry";
    let (what, redo, kept) = if moving { ("旧入口", "重新发起换线路机", "旧入口没摘") } else { ("旧节点", "重新发起替换", "编号没改") };
    let node = c.target_id()?;
    // 新入口属于谁：替换是新节点，换线路机是节点自己
    let new = if moving { node } else { c.param_uuid("new_id")? };
    // 每次尝试（含重试）从头来：上一次的记录作废
    if c.first {
        c.set_params(json!({"routers_applied": [], "routers_stuck": [], "routers_queued": null})).await?;
    }
    let applied: Vec<Applied> = if c.first { vec![] } else { serde_json::from_value(c.job.params["routers_applied"].clone()).unwrap_or_default() };
    let pick = Pick::load(c, moving).await?;
    let entries: Vec<clash::Entry> = sqlx::query_as(clash::ENTRIES).fetch_all(&c.st.db).await?;
    let mut rows = routers::load(&c.st.db, None).await?;
    rows.sort_by_key(|r| r.id);
    let plan: Vec<RouterPlan> = serde_json::from_value(c.job.params["plan"]["routers"].clone()).unwrap_or_default();
    // 1. 不在线的：最近一次采集里有要改的出口（或这一步中途重启前已经改过它），一台都不改
    let down: Vec<String> = rows
        .iter()
        .filter(|r| r.state() != "online" && (applied.iter().any(|a| a.id == r.id) || snap_uses(r, node, &pick)))
        .map(|r| format!("{}（{}）", r.name, r.error.as_deref().unwrap_or(if r.state() == "offline" { "离线" } else { "还没接入" })))
        .collect();
    if !down.is_empty() {
        return give_up(c, &applied, None, format!("软路由 {} 不在线，一台都没有改", down.join("、")), ErrKind::Network, kept).await;
    }
    // 2. 在线的按 id 顺序：读现在的配置定要改的出口，要改的加锁
    let mut locks = Locks::new(c).await?;
    let resumed = !applied.is_empty();
    let mut todo: Vec<(&routers::Row, Vec<ExitPlan>)> = vec![];
    let mut bad = vec![];
    for r in rows.iter().filter(|r| r.state() == "online") {
        let prev = applied.iter().find(|a| a.id == r.id);
        let got = locks.take(r.id, resumed).await?;
        let exits = match prev {
            Some(a) => a.exits.clone(),
            None => match read_config(c, r).await.and_then(|ps| exits_in(&ps, &entries, node, &pick).map_err(|why| format!("{why}：{redo}"))) {
                Ok(x) => x,
                Err(e) if plan.iter().any(|p| p.id == r.id) || snap_uses(r, node, &pick) => {
                    bad.push(format!("{}（{e}）", r.name));
                    vec![]
                }
                Err(e) => {
                    c.warn(format!("软路由「{}」的配置读不了（{e}）：最近一次采集里它没有出口指向{what}，不改", r.name)).await;
                    vec![]
                }
            },
        };
        if exits.is_empty() {
            if got && prev.is_none() {
                locks.release(r.id).await?;
            }
            continue;
        }
        if !got {
            // 别的作业在改这台路由：已经拿到的全部放开（连接关掉），排队等它改完
            drop(locks);
            if c.job.params["routers_queued"].as_str() != Some(r.name.as_str()) {
                c.log(format!("软路由「{}」正在被别的作业改，排队等它改完", r.name)).await;
            }
            c.set_params(json!({"routers_queued": r.name})).await?;
            return Ok(Outcome::Queue(10));
        }
        todo.push((r, exits));
    }
    if c.job.params["routers_queued"].is_string() {
        c.set_params(json!({"routers_queued": null})).await?;
    }
    if !bad.is_empty() {
        return give_up(c, &applied, None, format!("软路由 {} 的配置读不了或对不上，一台都没有改", bad.join("、")), ErrKind::Remote, kept).await;
    }
    if todo.is_empty() && applied.is_empty() {
        return Ok(Outcome::Skipped(format!("没有软路由的出口指向{what}")));
    }
    // 和发起时的计划不一样（之后路由的配置改过）：写一行，免得对着预览找不到
    let names = |rs: Vec<(Uuid, &str, Vec<&str>)>| rs.into_iter().map(|(_, n, xs)| format!("{n} 的 {}", xs.join("、"))).collect::<Vec<_>>();
    let now: Vec<(Uuid, &str, Vec<&str>)> = todo.iter().map(|(r, xs)| (r.id, r.name.as_str(), xs.iter().map(|x| x.exit.as_str()).collect())).collect();
    let then: Vec<(Uuid, &str, Vec<&str>)> = plan.iter().map(|r| (r.id, r.name.as_str(), r.exits.iter().map(|x| x.exit.as_str()).collect())).collect();
    let key = |v: &[(Uuid, &str, Vec<&str>)]| v.iter().flat_map(|(id, _, xs)| xs.iter().map(move |x| (*id, x.to_string()))).collect::<BTreeSet<_>>();
    if !resumed && key(&now) != key(&then) {
        let then = names(then);
        c.log(format!(
            "按软路由现在的配置要改：{}（发起时的预览是：{}）",
            if now.is_empty() { "没有".to_string() } else { names(now).join("；") },
            if then.is_empty() { "没有".to_string() } else { then.join("；") }
        ))
        .await;
    }
    // 3. 一台一台改
    let mut applied = applied;
    let mut done = vec![];
    for (r, exits) in &todo {
        let mut ed = vec![];
        for x in exits {
            let e: Option<NewEntry> = sqlx::query_as(
                "select e.client_uuid, e.short_id, e.port, r.reality_sni, r.reality_public_key from entry e join relay r on r.machine_id = e.relay_id
                 where e.id = $1 and e.node_id = $2 and e.lifecycle = 'active'",
            )
            .bind(x.new_entry_id)
            .bind(new)
            .fetch_optional(&c.st.db)
            .await?;
            let Some(e) = e else { return give_up(c, &applied, None, format!("新入口（{}）已经不在用了：{redo}", x.to()), ErrKind::Remote, kept).await };
            ed.push(rr::Edit {
                exit: x.exit.clone(),
                old_server: x.old_server.clone(),
                old_port: x.old_port,
                server: x.server.clone(),
                port: e.port as u16,
                uuid: e.client_uuid.to_string(),
                sni: e.reality_sni,
                pbk: e.reality_public_key,
                sid: e.short_id,
            });
        }
        let stamp = rr::stamp(OffsetDateTime::now_utc());
        if !applied.iter().any(|a| a.id == r.id) {
            applied.push(Applied { id: r.id, name: r.name.clone(), port: r.tunnel_port, stamp: stamp.clone(), exits: exits.clone() });
        }
        c.set_params(json!({"routers_applied": applied})).await?;
        let what: Vec<String> = ed.iter().map(|e| format!("{} {}:{} → {}:{}", e.exit, e.old_server, e.old_port, e.server, e.port)).collect();
        c.log(format!("软路由「{}」：改出口 {}（先备份，改完重启 OpenClash 并测延迟）", r.name, what.join("，"))).await;
        let res = ssh::exec(tunnel::via_tunnel(&c.st.cfg, r.id, r.tunnel_port), &rr::apply_script(&ed, &stamp), rr::APPLY_TIMEOUT).await;
        let (kind, why, self_undone) = match res {
            Ok(o) if o.ok() && o.kv("replace") == Some("ok") => {
                let delays: Vec<String> = (1..=ed.len()).map(|i| format!("{} {} ms", ed[i - 1].exit, o.kv(&format!("delay_{i}")).unwrap_or("?"))).collect();
                c.log(format!("软路由「{}」改好了：备份 {}，延迟 {}", r.name, o.kv("backup").unwrap_or("?"), delays.join("，"))).await;
                done.push(json!({"id": r.id, "name": r.name, "exits": ed.iter().map(|e| json!({"exit": e.exit, "to": format!("{}:{}", e.server, e.port)})).collect::<Vec<_>>(),
                    "backup": o.kv("backup"), "delay_ms": (1..=ed.len()).map(|i| o.kv(&format!("delay_{i}")).and_then(|d| d.parse::<i64>().ok())).collect::<Vec<_>>()}));
                continue;
            }
            Ok(o) if o.code == 255 => {
                if o.ssh_kind() == Some(ErrKind::HostKey) {
                    tunnel::close_master(&c.st.cfg, r.id).await;
                }
                // 没连上：这台什么都没改（不去换回它，也不算换回失败）；连上之后断的：可能改了一半，要换回
                let untouched = never_ran(&o);
                let why = tunnel::explain(&o, true) + if untouched { "（没连上，这台什么都没改）" } else { "" };
                (o.ssh_kind().unwrap_or(ErrKind::Network), why, untouched)
            }
            // 2：一行没写；其它：脚本已经换回这次改之前的备份（rolled_back=1），或换回也失败了
            Ok(o) => (ErrKind::Remote, reason(&o), o.code == 2 || o.kv("rolled_back") == Some("1")),
            Err(e) => (kind_of(&e), format!("{e:#}"), false),
        };
        // 这一台自己换回了（或没连上、什么都没改），而且这次的备份就是第一次改之前的：不用再回滚它
        let undone = self_undone.then_some((r.id, stamp.as_str()));
        return give_up(c, &applied, undone, format!("软路由「{}」：{why}", r.name), kind, kept).await;
    }
    // 马上采一次改过的路由：快照别等下一轮，紧接着的预览按新配置算
    c.st.routers_refresh.now(todo.iter().map(|(r, _)| r.id)).await;
    Ok(Outcome::Done(json!({"routers": done})))
}

/// 改路由没成：之前改好的（和没自己换回的那一台，`undone` 是自己换回了的那台与它这次的备份）按各自第一次改之前的备份换回，作业失败。
/// 都换回了清掉 `routers_applied`；换回失败的记进 `routers_stuck`（作业不能再取消，人工处理好后重试）
async fn give_up(c: &Ctx<'_>, applied: &[Applied], undone: Option<(Uuid, &str)>, head: String, kind: ErrKind, kept: &str) -> anyhow::Result<Outcome> {
    let mut back = vec![];
    let mut stuck = vec![];
    let mut stuck_names = vec![];
    for a in applied.iter().rev() {
        if undone.is_some_and(|(id, stamp)| a.id == id && a.stamp == stamp) {
            continue;
        }
        let out = ssh::exec(tunnel::via_tunnel(&c.st.cfg, a.id, a.port), &rr::rollback_script(&a.stamp), rr::ROLLBACK_TIMEOUT).await;
        let why = match out {
            Ok(o) if o.ok() && matches!(o.kv("rollback"), Some("ok" | "none")) => {
                back.push(a.name.clone());
                continue;
            }
            Ok(o) => if o.code == 255 { tunnel::explain(&o, true) } else { reason(&o) },
            Err(e) => format!("{e:#}"),
        };
        stuck.push(format!("{}（{why}，备份 /etc/fleet-backup/openclash/*.{}）", a.name, a.stamp));
        stuck_names.push(a.name.clone());
    }
    if !applied.is_empty() {
        c.st.routers_refresh.now(applied.iter().map(|a| a.id)).await;
    }
    let mut msg = head;
    if !back.is_empty() {
        msg += &format!("；已换回原配置：{}", back.join("、"));
    }
    if stuck.is_empty() {
        c.set_params(json!({"routers_applied": []})).await?;
        msg += &format!("。软路由都是原样，{kept}：处理好后点重试");
    } else {
        c.set_params(json!({"routers_stuck": stuck_names})).await?;
        msg += &format!("；换回失败、需要人工处理：{}。处理好后点重试（这时不能取消）", stuck.join("、"));
    }
    c.warn(&msg).await;
    Err(StepError::err(kind, msg))
}

/// 交换编号：旧节点挪去目标团队（备用）/ 空号，新节点接旧编号，同一个事务里走 PATCH 的那套（machines::patch_tx）；
/// auto_compact 时新节点原来的团队随后压缩。已经换好（中控在提交后重启、接着做）的直接算完成
pub(super) async fn renumber(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let old = c.target_id()?;
    let new = c.param_uuid("new_id")?;
    let old_team = c.param_uuid("old_team_id")?;
    let old_number = c.job.params["old_number"].as_i64().map(|n| n as i32).ok_or_else(|| anyhow!("作业参数缺少 old_number"))?;
    let to_team = c.param_uuid("to_team_id")?;
    let given = c.job.params["to_number"].as_i64().map(|n| n as i32);
    let actor = c.job.created_by.as_str();
    let mut tx = c.st.db.begin().await?;
    // 先锁团队（按 id）再锁机器（按 id）：和退役收尾的「团队 → 机器」同序
    let new_team0: Option<Uuid> = sqlx::query_scalar("select team_id from machine where id = $1").bind(new).fetch_optional(&mut *tx).await?.flatten();
    let mut teams: Vec<Uuid> = [Some(old_team), Some(to_team), new_team0].into_iter().flatten().collect();
    teams.sort();
    teams.dedup();
    sqlx::query("select 1 from team where id = any($1) order by id for update").bind(&teams).execute(&mut *tx).await?;
    type Cur = (Uuid, i32, Option<Uuid>, Option<i32>, String);
    let rows: Vec<Cur> = sqlx::query_as("select id, revision, team_id, number, lifecycle::text from machine where id = any($1) order by id for update")
        .bind(vec![old, new])
        .fetch_all(&mut *tx)
        .await?;
    let get = |id: Uuid| rows.iter().find(|r| r.0 == id).cloned();
    let (Some(o), Some(n)) = (get(old), get(new)) else { bail!("旧节点或新节点已经不存在") };
    if n.2 == Some(old_team) && n.3 == Some(old_number) && o.2 == Some(to_team) {
        return Ok(Outcome::Skipped("编号已经换好".into()));
    }
    if n.4 != "active" {
        bail!("新节点现在是「{}」，不能接替：重新发起替换", crate::meta::lifecycle_label(&n.4));
    }
    if o.2 != Some(old_team) || o.3 != Some(old_number) {
        return Err(remote("旧节点的团队或编号在发起替换之后被改过，编号没有动：软路由已经指向新节点，按需要手动改名（fleetctl rename）"));
    }
    let to_number = match given {
        Some(x) => x,
        None => {
            let taken: Vec<i32> = sqlx::query_scalar(
                "select number from machine where team_id = $1 and number is not null and kind <> 'relay' and lifecycle <> 'retired' and id <> all($2)",
            )
            .bind(to_team)
            .bind(vec![old, new])
            .fetch_all(&mut *tx)
            .await?;
            let mut taken: BTreeSet<i32> = taken.into_iter().collect();
            if to_team == old_team {
                taken.insert(old_number);
            }
            (1..=999).find(|x| !taken.contains(x)).ok_or_else(|| remote("旧节点要去的团队编号用完了"))?
        }
    };
    use crate::api::machines::{patch_tx, PatchIn};
    let api = |e: crate::error::ApiError| remote(e.message);
    // 旧节点先挪开，旧编号空出来再给新节点（唯一索引 machine_number_live）
    let mut relays = patch_tx(&mut tx, actor, old, PatchIn::rename(o.1, to_team, to_number)).await.map_err(api)?.relays;
    relays.extend(patch_tx(&mut tx, actor, new, PatchIn::rename(n.1, old_team, old_number)).await.map_err(api)?.relays);
    // 旧节点进了备用：回收站的「原因」写明是被谁替换下来的
    let (old_before, new_before) = (c.job.params["old_name"].as_str().unwrap_or(""), c.job.params["new_name"].as_str().unwrap_or(""));
    sqlx::query("update machine m set lifecycle_reason = $2 from team t where m.id = $1 and t.id = m.team_id and t.spare")
        .bind(old)
        .bind(format!("替换下来：{new_before} 接替了 {old_before}"))
        .execute(&mut *tx)
        .await?;
    // 来源画像跟着客户走：旧节点见过的运营商 / 城市记到新节点（学习期起点取最早的 first_seen），客户换过来不算「新出现」、
    // 新机器也不用重新学 7 天。连接数不搬（统计还算在旧节点上）；新节点自己已有的不动
    sqlx::query(
        "insert into node_source_seen (node_id, dim, value, first_seen, last_seen, conns, flag)
         select $2, dim, value, first_seen, last_seen, 0, null from node_source_seen where node_id = $1 on conflict do nothing",
    )
    .bind(old)
    .bind(new)
    .execute(&mut *tx)
    .await?;
    let mut renumbered = 0;
    if let Some(t) = new_team0.filter(|_| c.st.cfg.numbering.auto_compact) {
        let (ch, rs) = crate::db::compact_team(&mut tx, t, actor, "替换节点后自动压缩").await?;
        renumbered = ch.len();
        relays.extend(rs);
    }
    let names: Vec<(Uuid, String)> = sqlx::query_as(
        "select m.id, display_name(m.kind::text, m.label, t.name, m.number) from machine m left join team t on t.id = m.team_id where m.id = any($1)",
    )
    .bind(vec![old, new])
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    relays.sort();
    relays.dedup();
    for r in relays {
        c.st.relay_bumped(r);
    }
    c.st.emit(crate::state::Event::Machines);
    let name = |id: Uuid| names.iter().find(|x| x.0 == id).map(|x| x.1.clone()).unwrap_or_default();
    let (on, nn) = (name(old), name(new));
    c.log(format!("编号已交换：新节点现在是 {nn}，旧节点现在是 {on}{}", if renumbered > 0 { format!("；{renumbered} 台节点因压缩改了编号（见审计）") } else { String::new() })).await;
    Ok(Outcome::Done(json!({"old": on, "new": nn, "renumbered": renumbered})))
}
