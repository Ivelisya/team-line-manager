//! 替换节点（replace）的步骤：改软路由出口 → 交换编号。发起时按快照算好的计划（改哪些路由的哪些出口、旧节点去哪）存在作业参数里，
//! 见 api/replace.rs；路由上的脚本见 routers/replace.rs。
//! 换线路机（move_entry）在验收新入口之后、摘旧入口之前也走这里的「改软路由出口」（计划见 api/lifecycle.rs，新入口取参数 `entry_id`）。
//!
//! 改路由：要改哪些出口按执行时路由的配置重新确定（经隧道读源配置，同一套匹配：api::replace::replace_exit / api::lifecycle::move_exit），
//! 发起时的计划只作展示。不在线的路由按最近一次采集看，有出口要改就一台都不改。出口对不上新入口（同预览的原因）也一台都不改。
//! 在线的路由配置读不了（读失败、解析不了），哪怕快照里看不出它有出口要改，也一台都不改、作业失败：看不出就不能当成没有
//! （旧节点进备用、到期回收后客户的出口就断了）；确认它没用的，重新发起时选不改软路由。路由上根本没有源配置（OpenClash 没配）的算没有出口。
//! 同一台路由同时只有一个作业在改（替换、换线路机、重试都一样，见 `Locks`）：别的作业在改就排队（Outcome::Queue），作业的其余步骤照常并行。
//! 排队时只等那台路由的锁（`routers_queued_id`），不读配置、不占连接。
//! 搬空线路机同批的换线路机作业由一个带头的一起改（见 batch.rs）：要改的出口是几个作业合起来的，每台路由只改一次、重启一次，
//! 任何一台失败全部换回、几个节点的旧入口都不摘。
//! 再一台一台改（备份 → 改出口 → 重启 OpenClash → 测延迟）。哪台失败了，它自己已经改回（脚本做），之前改好的也按各自的备份改回去，
//! 作业失败：路由保持一致，修好后重试从头改。改回只动本作业改过、现在还是本作业写的那几个出口（routers::replace），
//! 同一台路由上别的作业之后改的出口不会被抹掉。
//! 改过的路由、第一次改之前的备份时间戳与要改的出口记在参数 `routers_applied`，确认都改回了、或这一步做完（成功时与 `routers_stuck` 一起清）才清掉：
//! 中途被打断（外层超时、中控重启）或改回失败时它还在，这时作业不能取消（jobs::cancel_blocker），重试照它接着做完（不重新确定出口，
//! 备份戳沿用第一次的）。改回失败的另记在 `routers_stuck`（哪台、哪些出口、改之前指向哪），作业详情据此给出「需要人工处理」。
//! 凭据（uuid、公钥、short-id）执行时才从库里取，只进 ssh 的 stdin，不进日志与审计。写进每个出口的那一整组（含凭据）随 `routers_applied` 记在作业参数里
//! （参数不经 API 给出），改回时照它核对路由上现在的值：仍是本作业写的才改回，已被别人改过的跳过并在作业日志说明；不从库里现取，新入口在卡住期间被轮换也不会误判。

use super::batch;
use super::*;
use crate::api::lifecycle::{move_exit, relay_addr};
use crate::api::replace::{replace_entries, replace_exit, Ent, ExitPlan, RouterPlan};
use crate::routers::{self, clash, replace as rr, tunnel};
use std::collections::HashSet;

#[derive(Clone, Serialize, Deserialize)]
struct Applied {
    id: Uuid,
    name: String,
    /// 第一次改时的隧道端口（只作记录：改回时按路由现在的端口连，期间重新接入换过端口也连得上）
    port: i32,
    /// 这台第一次改之前的备份（回滚用它）
    stamp: String,
    /// 这台要改的出口：中控在这一步中途重启、接着做时照它重做（路由的配置可能已经改好，不再按配置重新确定）
    #[serde(default)]
    exits: Vec<ExitPlan>,
    /// 最近一次写进这台各出口的整组参数（含凭据）：改回时照它核对路由上现在的值，不从库里现取（卡住期间入口被轮换也认得出是不是本作业写的）
    #[serde(default)]
    written: Vec<rr::Edit>,
}

impl Applied {
    /// 改回用：这台上本作业写入的出口的样子（出口名、server:port 和整组凭据：路由上整组参数都对上才算「仍是本作业写的」）。
    /// 记录里没有（旧版本留下的）：没法核对，返回原因（不能当成改回了）
    fn back(&self) -> Result<Vec<rr::Edit>, String> {
        if self.written.is_empty() {
            return Err("作业记录里没有写入出口的参数，没法核对是不是本作业写的".into());
        }
        Ok(self.written.clone())
    }
}

/// 改回失败、要人工处理的一台路由（参数 `routers_stuck`，作业的 `manual` 由它而来，见 jobs::manual）
#[derive(Serialize)]
struct Stuck {
    id: Uuid,
    name: String,
    exits: Vec<String>,
    /// 各出口改之前指向的 server:port
    was: Vec<String>,
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

/// 入口的参数：只取属于这几个节点（各作业的新入口所在的节点）、在用的入口
async fn new_entry(db: &sqlx::PgPool, id: Uuid, nodes: &[Uuid]) -> sqlx::Result<Option<NewEntry>> {
    sqlx::query_as(
        "select e.client_uuid, e.short_id, e.port, r.reality_sni, r.reality_public_key from entry e join relay r on r.machine_id = e.relay_id
         where e.id = $1 and e.node_id = any($2) and e.lifecycle = 'active'",
    )
    .bind(id)
    .bind(nodes)
    .fetch_optional(db)
    .await
}

/// 还在用的备份的时间戳：没做完的作业（失败的、在重试的）参数里记着的，改配置时脚本不删它们
async fn pinned_stamps(db: &sqlx::PgPool) -> sqlx::Result<Vec<String>> {
    sqlx::query_scalar(
        "select distinct a->>'stamp' from job j,
             jsonb_array_elements(case when jsonb_typeof(j.params->'routers_applied') = 'array' then j.params->'routers_applied' else '[]'::jsonb end) a
         where j.state not in ('succeeded', 'cancelled') and a->>'stamp' is not null",
    )
    .fetch_all(db)
    .await
}

/// 出口要不要改、改成什么：按库里现在的入口算，同预览
enum Pick {
    /// 换线路机：旧入口 → 新入口 (id, 端口)；旧线路机的 IP，新线路机的 (IP, public_host)
    Move { old_entry: Uuid, new: (Uuid, u16), from_ip: String, to: (String, String) },
    /// 替换：旧节点没删除的入口 → 新节点在用的入口
    Replace { old: Vec<Ent>, new: Vec<Ent> },
}

impl Pick {
    /// `node`：作业的目标（替换是旧节点，换线路机是节点自己）
    async fn load(db: &sqlx::PgPool, job: &JobRow, node: Uuid, moving: bool) -> anyhow::Result<Pick> {
        if !moving {
            let mut conn = db.acquire().await?;
            let (old, new) = replace_entries(&mut conn, node, uuid_of(&job.params, "new_id")?).await?;
            return Ok(Pick::Replace { old, new });
        }
        let eid = uuid_of(&job.params, "entry_id")?;
        let port: Option<i32> = sqlx::query_scalar("select port from entry where id = $1 and node_id = $2 and lifecycle = 'active'")
            .bind(eid)
            .bind(node)
            .fetch_optional(db)
            .await?;
        let Some(port) = port else { bail!("新入口已经不在用了：重新发起换线路机") };
        let (from_ip, _) = relay_addr(db, uuid_of(&job.params, "from_relay_id")?).await?;
        let to = relay_addr(db, uuid_of(&job.params, "to_relay_id")?).await?;
        Ok(Pick::Move { old_entry: uuid_of(&job.params, "old_entry_id")?, new: (eid, port as u16), from_ip, to })
    }

    fn exit(&self, er: &clash::EntryRef, srv: &str, port: u16) -> Option<Result<ExitPlan, String>> {
        match self {
            Pick::Move { old_entry, new, from_ip, to } => move_exit(er, *old_entry, Some(*new), from_ip, to, srv, port).map(Ok),
            Pick::Replace { old, new } => replace_exit(old, new, er, srv, port),
        }
    }

    /// 指向这条旧入口的出口归这个作业改
    fn owns(&self, old_entry: Uuid) -> bool {
        match self {
            Pick::Move { old_entry: o, .. } => *o == old_entry,
            Pick::Replace { old, .. } => old.iter().any(|e| e.id == old_entry),
        }
    }
}

fn uuid_of(params: &Value, k: &str) -> anyhow::Result<Uuid> {
    params[k].as_str().and_then(|s| Uuid::try_parse(s).ok()).ok_or_else(|| anyhow!("作业参数缺少 {k}"))
}

/// 一起改软路由的一个作业：本作业，或搬空时同批由本作业带头一起改的（batch.rs）
struct Part {
    job: Uuid,
    /// 出口对上的入口属于哪个节点：替换是旧节点，换线路机是节点自己
    node: Uuid,
    /// 新入口属于哪个节点：替换是新节点，换线路机是节点自己
    new: Uuid,
    pick: Pick,
    /// 发起时的计划（只作展示）
    plan: Vec<RouterPlan>,
}

impl Part {
    async fn load(db: &sqlx::PgPool, job: &JobRow) -> anyhow::Result<Part> {
        let moving = job.kind == "move_entry";
        let node = job.target_id.ok_or_else(|| anyhow!("作业没有目标机器"))?;
        let new = if moving { node } else { uuid_of(&job.params, "new_id")? };
        let pick = Pick::load(db, job, node, moving).await?;
        let plan = serde_json::from_value(job.params["plan"]["routers"].clone()).unwrap_or_default();
        Ok(Part { job: job.id, node, new, pick, plan })
    }
}

/// 同一台软路由同时只有一个作业在改：PG 会话级 advisory lock，键由路由 id 算出。第一次拿到锁时把那条连接从连接池拿走（detach），
/// 读配置 → 改 → 重启 → 测 / 改回期间持有；这一步结束（含出错、超时被丢弃、中控退出）连接关掉，锁随之放开。
/// 拿不到时用的是池里的连接，什么都不占（排队时每次轮询不新建连接）。往前改都只试一次（try），拿不到就全部放开去排队，不会互相等死；
/// 只有改回（give_up）等锁：那时手里的锁都是要改回的路由
struct Locks<'a> {
    db: &'a sqlx::PgPool,
    conn: Option<sqlx::PgConnection>,
    held: HashSet<Uuid>,
}

const TRY_LOCK: &str = "select pg_try_advisory_lock(hashtextextended('router:' || $1::text, 0))";

impl<'a> Locks<'a> {
    fn new(db: &'a sqlx::PgPool) -> Self { Locks { db, conn: None, held: HashSet::new() } }

    /// 试一次，拿不到返回 false
    async fn take(&mut self, id: Uuid) -> anyhow::Result<bool> {
        if self.held.contains(&id) {
            return Ok(true);
        }
        let got = match &mut self.conn {
            Some(conn) => sqlx::query_scalar(TRY_LOCK).bind(id).fetch_one(&mut *conn).await?,
            None => {
                let mut pooled = self.db.acquire().await?;
                let got: bool = sqlx::query_scalar(TRY_LOCK).bind(id).fetch_one(&mut *pooled).await?;
                if got {
                    self.conn = Some(pooled.detach());
                }
                got
            }
        };
        if got {
            self.held.insert(id);
        }
        Ok(got)
    }

    /// 等到拿到为止（改回之前）
    async fn wait(&mut self, id: Uuid) -> anyhow::Result<()> {
        if self.held.contains(&id) {
            return Ok(());
        }
        if self.conn.is_none() {
            self.conn = Some(self.db.acquire().await?.detach());
        }
        let conn = self.conn.as_mut().expect("刚建好");
        sqlx::query("select pg_advisory_lock(hashtextextended('router:' || $1::text, 0))").bind(id).execute(&mut *conn).await?;
        self.held.insert(id);
        Ok(())
    }

    async fn release(&mut self, id: Uuid) -> anyhow::Result<()> {
        if let (true, Some(conn)) = (self.held.remove(&id), self.conn.as_mut()) {
            sqlx::query("select pg_advisory_unlock(hashtextextended('router:' || $1::text, 0))").bind(id).execute(&mut *conn).await?;
        }
        Ok(())
    }
}

/// 脚本自己说的原因（`fleet：` 开头的行），没有就用 stderr 的最后几行
fn reason(o: &Output) -> String {
    let own = own_lines(o);
    if !own.is_empty() {
        return own.join("；");
    }
    let tail = raw_tail(o);
    if tail.is_empty() { format!("退出码 {}，没有输出", o.code) } else { format!("退出码 {}：{tail}", o.code) }
}

/// 脚本自己说的话（`fleet：` 开头的行）
fn own_lines(o: &Output) -> Vec<&str> { o.stderr.lines().filter_map(|l| l.strip_prefix("fleet：")).collect() }

/// stderr 的最后几行原文（ssh 的英文原样，进日志）
fn raw_tail(o: &Output) -> String {
    let tail: Vec<&str> = o.stderr.lines().rev().map(str::trim).filter(|l| !l.is_empty()).take(3).collect();
    tail.into_iter().rev().collect::<Vec<_>>().join(" / ")
}

/// ssh 没连上就失败了（连接、握手、登录、主机公钥这几关，脚本一行都没跑）：这台路由上什么都没动，不用换回
fn never_ran(o: &Output) -> bool {
    const BEFORE: &[&str] = &["connect to host", "banner exchange", "kex_exchange_identification", "Connection refused", "No route to host", "Could not resolve", "Network is unreachable"];
    o.code == 255
        && o.stdout.trim().is_empty()
        && (matches!(o.ssh_kind(), Some(ErrKind::HostKey | ErrKind::Auth)) || BEFORE.iter().any(|p| o.stderr.contains(p)))
}

/// ssh 自己失败（255）的说法，原始输出另记一行日志
async fn ssh_failed(c: &Ctx<'_>, name: &str, o: &Output) -> String {
    let raw = raw_tail(o);
    if !raw.is_empty() {
        c.warn(format!("软路由「{name}」ssh 的原始输出：{raw}")).await;
    }
    tunnel::explain(o, true)
}

/// 经隧道读路由现在的 OpenClash 源配置，解析出口。路由上没有源配置（rr::NO_CONFIG）的算没有出口
async fn read_config(c: &Ctx<'_>, r: &routers::Row) -> Result<Vec<clash::Proxy>, String> {
    let o = ssh::exec(tunnel::via_tunnel(&c.st.cfg, r.id, r.tunnel_port), &rr::read_script(), rr::READ_TIMEOUT).await.map_err(|e| format!("{e:#}"))?;
    if o.code == 255 {
        return Err(ssh_failed(c, &r.name, &o).await);
    }
    if o.code == rr::NO_CONFIG {
        c.log(format!("软路由「{}」：{}，没有出口要改", r.name, reason(&o))).await;
        return Ok(vec![]);
    }
    if !o.ok() {
        return Err(reason(&o));
    }
    if o.stdout.len() > clash::MAX_CONFIG {
        return Err("配置文件超过 1 MiB，不解析".into());
    }
    clash::parse(&o.stdout)
}

/// 配置里要改的出口：对上的入口属于某个作业的 `node`、没删除，且它的 `pick` 说要改的。对不上新入口的返回（出口名, 原因），同预览
fn exits_in(proxies: &[clash::Proxy], entries: &[clash::Entry], parts: &[Part]) -> Result<Vec<ExitPlan>, (String, String)> {
    let mut out = vec![];
    for p in proxies {
        let (Some(er), _) = clash::match_entry(p, entries) else { continue };
        let (Some(srv), Some(port)) = (p.server.as_deref(), p.port) else { continue };
        if er.lifecycle == "removed" {
            continue;
        }
        for part in parts.iter().filter(|x| x.node == er.node_id) {
            match part.pick.exit(&er, srv, port) {
                Some(Ok(e)) => out.push(ExitPlan { exit: p.name.clone(), ..e }),
                Some(Err(why)) => return Err((p.name.clone(), why)),
                None => {}
            }
        }
    }
    Ok(out)
}

/// 最近一次采集里这台路由有没有要改的出口（不在线、配置读不了时只能看它）
fn snap_uses(r: &routers::Row, parts: &[Part]) -> bool {
    let Some(snap) = r.snapshot() else { return false };
    parts.iter().any(|part| {
        // 先放进变量：迭代器借着 snap，不能是闭包的尾表达式
        let used = routers::exits_to(&snap, part.node).any(|(x, er)| matches!((x.server.as_deref(), x.port), (Some(s), Some(p)) if part.pick.exit(er, s, p).is_some()));
        used
    })
}

pub(super) async fn routers(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    if c.job.params["routers"] == false {
        return Ok(Outcome::Skipped("选择了不改软路由".into()));
    }
    let moving = c.job.kind == "move_entry";
    let (what, redo, kept) = if moving { ("旧入口", "重新发起换线路机", "旧入口没摘") } else { ("旧节点", "重新发起替换", "编号没改") };
    let mut locks = Locks::new(&c.st.db);
    // 排队中：只等那台路由的锁，别的都不做
    let queued_on = if c.first { None } else { c.job.params["routers_queued_id"].as_str().and_then(|s| Uuid::try_parse(s).ok()) };
    if let Some(q) = queued_on {
        if !locks.take(q).await? {
            return Ok(Outcome::Queue(10));
        }
    }
    // 上一次没确认改回的（中途被打断、改回失败）：接着做完
    let applied: Vec<Applied> = match &c.job.params["routers_applied"] {
        Value::Null => vec![],
        v => serde_json::from_value(v.clone()).context("作业参数 routers_applied 的格式不对")?,
    };
    let resumed = !applied.is_empty();
    if c.first {
        // 重新开始（重试）：上一次带头认领的作废，没确认改回的除外（那几个作业的出口也在 routers_applied 里，接着做完）
        let reset = if resumed { json!({"routers_queued": null, "routers_queued_id": null}) } else { json!({"routers_queued": null, "routers_queued_id": null, "routers_batch": null}) };
        c.set_params(reset).await?;
    }
    // 搬空同批：一起改（batch.rs）。带头认领过的接着带（排队等锁之后、接着做完）
    let mut followers: Vec<JobRow> = vec![];
    if let Some(b) = c.job.params["batch"].as_str().filter(|_| moving) {
        let claimed: Vec<Uuid> = c.job.params["routers_batch"].as_array().into_iter().flatten().filter_map(|x| x.as_str()).filter_map(|s| Uuid::try_parse(s).ok()).collect();
        if resumed || (!c.first && !claimed.is_empty()) {
            followers = sqlx::query_as("select id, kind, target_id, params, state::text state, step, created_by from job where id = any($1) order by id")
                .bind(&claimed)
                .fetch_all(&c.st.db)
                .await?;
        } else {
            match batch::join(c, b).await? {
                Ok(f) => followers = f,
                Err(o) => return Ok(o),
            }
        }
    }
    let mut parts = vec![Part::load(&c.st.db, c.job).await?];
    for f in &followers {
        let name = sqlx::query_scalar::<_, String>("select display_name(m.kind::text, m.label, t.name, m.number) from machine m left join team t on t.id = m.team_id where m.id = $1")
            .bind(f.target_id)
            .fetch_optional(&c.st.db)
            .await?
            .unwrap_or_default();
        parts.push(Part::load(&c.st.db, f).await.with_context(|| format!("同批一起改的「{name}」"))?);
    }
    let owners: Vec<Uuid> = parts.iter().map(|p| p.new).collect();
    let entries: Vec<clash::Entry> = sqlx::query_as(clash::ENTRIES).fetch_all(&c.st.db).await?;
    let mut rows = routers::load(&c.st.db, None).await?;
    rows.sort_by_key(|r| r.id);
    let plan: Vec<&RouterPlan> = parts.iter().flat_map(|p| &p.plan).collect();
    let none_yet = if resumed { "" } else { "，一台都没有改" };
    // 1. 不在线的：最近一次采集里有要改的出口（或之前已经改过它），一台都不改
    let down: Vec<String> = rows
        .iter()
        .filter(|r| r.state() != "online" && (applied.iter().any(|a| a.id == r.id) || snap_uses(r, &parts)))
        .map(|r| format!("{}（{}）", r.name, r.error.as_deref().unwrap_or(if r.state() == "offline" { "离线" } else { "还没接入" })))
        .collect();
    if !down.is_empty() {
        return give_up(c, &mut locks, &applied, None, format!("软路由 {} 不在线{none_yet}", down.join("、")), ErrKind::Network, kept).await;
    }
    // 2. 在线的按 id 顺序：读现在的配置定要改的出口，要改的加锁。别的作业在改、而计划 / 快照 / 之前改过的说它要改：不读配置，直接排队
    let mut todo: Vec<(&routers::Row, Vec<ExitPlan>)> = vec![];
    let mut blocked = vec![];
    for r in rows.iter().filter(|r| r.state() == "online") {
        let prev = applied.iter().find(|a| a.id == r.id);
        let known = prev.is_some() || plan.iter().any(|p| p.id == r.id) || snap_uses(r, &parts);
        let got = locks.take(r.id).await?;
        let exits = if !got && known {
            None
        } else {
            Some(match prev {
                Some(a) => a.exits.clone(),
                None => match read_config(c, r).await {
                    Ok(ps) => match exits_in(&ps, &entries, &parts) {
                        Ok(x) => x,
                        Err((exit, why)) => {
                            blocked.push(format!("软路由「{}」的出口「{exit}」：{why}", r.name));
                            vec![]
                        }
                    },
                    Err(e) if known => {
                        blocked.push(format!("软路由「{}」的配置读不了：{e}", r.name));
                        vec![]
                    }
                    // 快照里看不出它有出口要改，但配置读不了就不知道现在有没有：不能跳过
                    Err(e) => {
                        blocked.push(format!(
                            "软路由「{}」的配置读不了：{e}。看不出它有没有出口指向{what}（确认没有的话，重新发起时选不改软路由，fleetctl 加 --no-routers）",
                            r.name
                        ));
                        vec![]
                    }
                },
            })
        };
        match exits {
            Some(x) if x.is_empty() => locks.release(r.id).await?,
            Some(x) if got => todo.push((r, x)),
            // 别的作业在改这台路由：已经拿到的全部放开（连接关掉），排队等它改完
            _ => {
                drop(locks);
                if queued_on != Some(r.id) {
                    c.log(format!("软路由「{}」正在被别的作业改，排队等它改完", r.name)).await;
                }
                c.set_params(json!({"routers_queued": r.name, "routers_queued_id": r.id})).await?;
                return Ok(Outcome::Queue(10));
            }
        }
    }
    if queued_on.is_some() {
        c.set_params(json!({"routers_queued": null, "routers_queued_id": null})).await?;
    }
    if !blocked.is_empty() {
        return give_up(c, &mut locks, &applied, None, format!("{}{none_yet}", blocked.join("；")), ErrKind::Remote, kept).await;
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
    let pins = pinned_stamps(&c.st.db).await?;
    let mut done = vec![];
    for (r, exits) in &todo {
        let mut ed = vec![];
        // 各出口归哪个作业（同批一起改时跟着完成的作业按它取自己的那几个，见 batch.rs）
        let jobs: Vec<Uuid> = exits.iter().map(|x| parts.iter().find(|p| p.pick.owns(x.old_entry_id)).map_or(c.job.id, |p| p.job)).collect();
        for x in exits {
            let e = match x.new_entry_id {
                Some(id) => new_entry(&c.st.db, id, &owners).await?,
                None => None,
            };
            let Some(e) = e else { return give_up(c, &mut locks, &applied, None, format!("新入口（{}）已经不在用了：{redo}", x.to()), ErrKind::Remote, kept).await };
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
        let t = OffsetDateTime::now_utc();
        let stamp = rr::stamp(t);
        // 这次要写的整组参数先记下（接着做时入口可能已经轮换过，以这次写的为准），改回时照它核对
        match applied.iter_mut().find(|a| a.id == r.id) {
            Some(a) => a.written = ed.clone(),
            None => applied.push(Applied { id: r.id, name: r.name.clone(), port: r.tunnel_port, stamp: stamp.clone(), exits: exits.clone(), written: ed.clone() }),
        }
        c.set_params(json!({"routers_applied": applied})).await?;
        let what: Vec<String> = ed.iter().map(|e| format!("{} {}:{} → {}:{}", e.exit, e.old_server, e.old_port, e.server, e.port)).collect();
        c.log(format!("软路由「{}」：改出口 {}（先备份，改完重启 OpenClash 并测延迟）", r.name, what.join("，"))).await;
        let res = ssh::exec(tunnel::via_tunnel(&c.st.cfg, r.id, r.tunnel_port), &rr::apply_script(&ed, t, &pins), rr::APPLY_TIMEOUT).await;
        let (kind, why, self_undone) = match res {
            Ok(o) if o.ok() && o.kv("replace") == Some("ok") => {
                let delays: Vec<String> = (1..=ed.len()).map(|i| format!("{} {} ms", ed[i - 1].exit, o.kv(&format!("delay_{i}")).unwrap_or("?"))).collect();
                c.log(format!("软路由「{}」改好了：备份 {}，延迟 {}", r.name, o.kv("backup").unwrap_or("?"), delays.join("，"))).await;
                done.push(json!({"id": r.id, "name": r.name, "exits": ed.iter().zip(&jobs).map(|(e, j)| json!({"exit": e.exit, "to": format!("{}:{}", e.server, e.port), "job": j})).collect::<Vec<_>>(),
                    "backup": o.kv("backup"), "delay_ms": (1..=ed.len()).map(|i| o.kv(&format!("delay_{i}")).and_then(|d| d.parse::<i64>().ok())).collect::<Vec<_>>()}));
                continue;
            }
            Ok(o) if o.code == 255 => {
                if o.ssh_kind() == Some(ErrKind::HostKey) {
                    tunnel::close_master(&c.st.cfg, r.id).await;
                }
                // 没连上：这台什么都没改（不去换回它，也不算换回失败）；连上之后断的：可能改了一半，要换回
                let untouched = never_ran(&o);
                let mut why = ssh_failed(c, &r.name, &o).await;
                why += &match o.kv("backup") {
                    _ if untouched => "，这台没连上、什么都没改".to_string(),
                    Some(bk) => format!("，已经备份 {bk}，可能改了一半"),
                    None => "，连上之后断了，可能改了一半".to_string(),
                };
                (o.ssh_kind().unwrap_or(ErrKind::Network), why, untouched)
            }
            // 2：一行没写；其它：脚本已经改回（rolled_back=1），或改回也失败了
            Ok(o) => (ErrKind::Remote, reason(&o), o.code == 2 || o.kv("rolled_back") == Some("1")),
            Err(e) => (kind_of(&e), format!("{e:#}"), false),
        };
        // 这一台自己改回了（或没连上、什么都没改），而且这次的备份就是第一次改之前的：不用再改回它
        let undone = self_undone.then_some((r.id, stamp.as_str()));
        return give_up(c, &mut locks, &applied, undone, format!("软路由「{}」：{why}", r.name), kind, kept).await;
    }
    // 马上采一次改过的路由：快照别等下一轮，紧接着的预览按新配置算
    c.st.routers_refresh.now(todo.iter().map(|(r, _)| r.id)).await;
    c.set_params(json!({"routers_applied": [], "routers_stuck": []})).await?;
    let mut out = json!({"routers": done});
    if !followers.is_empty() {
        out["batch"] = json!(followers.iter().map(|f| f.id).collect::<Vec<_>>());
    }
    Ok(Outcome::Done(out))
}

/// 一台路由上本作业写的出口改回（先等它的锁）。没改回返回原因。隧道端口按路由现在的（卡住期间重新接入会换端口）
async fn revert_one(c: &Ctx<'_>, locks: &mut Locks<'_>, a: &Applied) -> anyhow::Result<Result<(), String>> {
    locks.wait(a.id).await?;
    let back = match a.back() {
        Ok(b) => b,
        Err(why) => return Ok(Err(why)),
    };
    let port: Option<i32> = sqlx::query_scalar("select tunnel_port from router where id = $1").bind(a.id).fetch_optional(&c.st.db).await?;
    let Some(port) = port else { return Ok(Err("这台软路由已经删掉了".into())) };
    let out = ssh::exec(tunnel::via_tunnel(&c.st.cfg, a.id, port), &rr::rollback_script(&a.stamp, &back), rr::ROLLBACK_TIMEOUT).await;
    Ok(match out {
        Ok(o) if o.ok() && matches!(o.kv("rollback"), Some("ok" | "none")) => {
            // 之后被别的作业改走的出口没动：记一行
            let notes = own_lines(&o);
            if !notes.is_empty() {
                c.warn(format!("软路由「{}」改回时：{}", a.name, notes.join("；"))).await;
            }
            Ok(())
        }
        Ok(o) if o.code == 255 => Err(ssh_failed(c, &a.name, &o).await),
        Ok(o) => Err(reason(&o)),
        Err(e) => Err(format!("{e:#}")),
    })
}

/// 改路由没成：之前改好的（和没自己改回的那一台，`undone` 是自己改回了的那台与它这次的备份）按各自第一次改之前的备份改回，作业失败。
/// 改回之前等那台路由的锁（同一时刻只有一个作业写它的配置）。都改回了清掉 `routers_applied`；改回失败的留在 `routers_applied`、
/// 另记进 `routers_stuck`（作业不能再取消，重试照它接着做完）
#[allow(clippy::too_many_arguments)]
async fn give_up(
    c: &Ctx<'_>,
    locks: &mut Locks<'_>,
    applied: &[Applied],
    undone: Option<(Uuid, &str)>,
    head: String,
    kind: ErrKind,
    kept: &str,
) -> anyhow::Result<Outcome> {
    let mut back = vec![];
    let mut left = vec![];
    let mut stuck = vec![];
    let mut whys = vec![];
    for a in applied.iter().rev() {
        if undone.is_some_and(|(id, stamp)| a.id == id && a.stamp == stamp) {
            continue;
        }
        if let Err(why) = revert_one(c, locks, a).await? {
            whys.push(format!("{}（{}）", a.name, why));
            stuck.push(Stuck { id: a.id, name: a.name.clone(), exits: a.exits.iter().map(|x| x.exit.clone()).collect(), was: a.exits.iter().map(|x| format!("{}:{}", x.old_server, x.old_port)).collect() });
            left.push(a);
        } else {
            back.push(a.name.clone());
        }
    }
    if !applied.is_empty() {
        c.st.routers_refresh.now(applied.iter().map(|a| a.id)).await;
    }
    let mut msg = head;
    if !back.is_empty() {
        msg += &format!("；已改回原配置：{}", back.join("、"));
    }
    if stuck.is_empty() {
        c.set_params(json!({"routers_applied": [], "routers_stuck": []})).await?;
        msg += &format!("。软路由都是原样，{kept}：处理好后点重试");
    } else {
        left.reverse();
        c.set_params(json!({"routers_applied": left, "routers_stuck": stuck})).await?;
        msg += &format!("；没能改回、需要人工处理：{}。处理好后点重试接着改完（这时不能取消）", whys.join("、"));
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 改回照作业记着的写入参数核对，不碰库；没有记录（旧版本留下的）按出错处理
    #[test]
    fn back_uses_recorded_params() {
        let x = ExitPlan { exit: "节点01".into(), old_entry_id: Uuid::nil(), new_entry_id: Some(Uuid::nil()), old_server: "a".into(), old_port: 1, server: "b".into(), port: None, error: None };
        let mut a = Applied { id: Uuid::nil(), name: "路由A".into(), port: 1, stamp: "S".into(), exits: vec![x], written: vec![] };
        assert!(a.back().unwrap_err().contains("没法核对"));
        let e = rr::Edit { exit: "节点01".into(), old_server: "a".into(), old_port: 1, server: "b".into(), port: 2, uuid: "U".into(), sni: "s".into(), pbk: "p".into(), sid: "i".into() };
        a.written = vec![e];
        let back = a.back().unwrap();
        assert_eq!((back[0].uuid.as_str(), back[0].port), ("U", 2));
        // 记进作业参数再读回来：整组都在
        let b: Applied = serde_json::from_value(serde_json::to_value(&a).unwrap()).unwrap();
        assert_eq!(b.written[0].pbk, "p");
    }
}
