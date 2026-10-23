//! 搬空线路机（evacuate）同批的换线路机作业一起改软路由：每台路由只改一次、只重启一次 OpenClash（每次重启都断开路由上的全部连接）。
//!
//! 搬空时每个节点一个 move_entry 作业，参数 `batch` 相同（api/lifecycle.rs）。各作业照常并行建新入口、验收；走到「改软路由出口」时：
//! - 同批还有作业没走到这一步（建新入口、等线路机、验收）：等它们（排队，路由没动，能取消）；
//! - 都到了：id 最小的那个带头，把同批停在这一步的作业认领下来（被认领的参数记 `routers_batch_leader`，带头的记 `routers_batch`），
//!   按各自的新入口把全部出口一起改：每台路由一次备份、一次改完、一次重启（replace.rs，改回、加锁、改不了的判断同单个作业）。
//!   被认领的从这时起不能取消（取消会摘掉路由要指向的新入口，jobs::cancel_blocker）；
//! - 被认领的等带头的结果：带头的这一步做完（改好、或没有出口要改）它们跟着完成，步骤输出是其中自己的那几个出口；
//!   带头的失败（任何一台失败全部换回，旧入口都不摘）它们跟着失败；改回失败要人工处理时同样记上 `routers_stuck`，
//!   重试带头的那个接着改完，之后重试它们就跟着完成。
//!
//! 失败停着、被取消的不算同批在等的；之后单独重试的，同批别的已经改过，就自己改（同单独换线路机）。
//! 带头的排队等路由的锁时被取消：被认领的放开，重新推一个带头的。

use super::*;

/// 同批另一个作业此刻的样子
#[derive(Debug, PartialEq)]
pub(super) struct Sib {
    pub id: Uuid,
    /// 节点名
    pub name: String,
    /// 作业状态
    pub state: String,
    /// 作业走到第几步；`at` 是「改软路由出口」那一步的下标
    pub step: i32,
    pub at: i32,
    /// 「改软路由出口」这一步的状态、错误、输出
    pub step_state: String,
    pub error: Option<String>,
    pub output: Value,
    /// 它带头时认领的作业（参数 routers_batch）
    pub batch: Vec<Uuid>,
    /// 认领它的作业（参数 routers_batch_leader）
    pub leader: Option<Uuid>,
    /// 改回失败要人工处理的路由（参数 routers_stuck）
    pub stuck: Value,
    /// 有没确认改回的（参数 routers_applied 不空）：它自己接着做完，不进同批
    pub applied: bool,
}

impl Sib {
    fn active(&self) -> bool { matches!(self.state.as_str(), "queued" | "running" | "waiting") }
    fn at_routers(&self) -> bool { self.active() && self.step == self.at }
    fn stuck(&self) -> bool { self.stuck.as_array().is_some_and(|a| !a.is_empty()) }
}

const BEHIND: &str = "等同批搬空的其它节点建好新入口，再一起改软路由（每台路由只改一次、重启一次）";
const TURN: &str = "等同批搬空的作业一起改软路由";
const BUSY: &str = "同批搬空的另一个作业正在改软路由，等它改完";

/// 本作业在同批里该做什么
#[derive(Debug, PartialEq)]
pub(super) enum Role<'a> {
    /// 跟着这个带头的，等它的结果
    Follow(&'a Sib),
    /// 等（同批别的还没走到这一步、等被认领、等别的一批改完）
    Wait(&'static str),
    /// 带头的这一步做完了：跟着完成
    Done(&'a Sib),
    /// 带头的失败了：跟着失败
    Fail(&'a Sib),
    /// 带头：认领这些作业，一起改
    Lead(Vec<Uuid>),
    /// 同批没有别的停在这一步：自己改
    Solo,
}

/// `me`：本作业；`flag`：认领本作业的（参数 routers_batch_leader）；`sibs`：同批别的作业
pub(super) fn role(me: Uuid, flag: Option<Uuid>, sibs: &[Sib]) -> Role<'_> {
    let leaders: Vec<&Sib> = sibs.iter().filter(|s| s.batch.contains(&me)).collect();
    if let Some(l) = leaders.iter().find(|l| l.at_routers()) {
        return Role::Follow(l);
    }
    if let Some(l) = leaders.iter().find(|l| matches!(l.step_state.as_str(), "done" | "skipped")) {
        return Role::Done(l);
    }
    // 带头的改回失败、要人工处理：一直跟着它（它重试改完之前本作业也不能取消、不能自己改）
    if let Some(l) = leaders.iter().find(|l| l.state == "failed" && l.step == l.at && l.stuck()) {
        return Role::Fail(l);
    }
    // 带头的失败了（已全部换回）：跟着失败一次；之后重试时已经放开（flag 清了），重新推
    if let Some(l) = leaders.iter().find(|l| flag == Some(l.id) && l.state == "failed") {
        return Role::Fail(l);
    }
    if sibs.iter().any(|s| s.at_routers() && !s.batch.is_empty()) {
        return Role::Wait(BUSY);
    }
    if sibs.iter().any(|s| s.active() && s.step < s.at) {
        return Role::Wait(BEHIND);
    }
    // 已经跟着本作业的（重试时它先看到了本作业上一次认领的名单）照样认领
    let others: Vec<Uuid> = sibs.iter().filter(|s| s.at_routers() && s.leader.is_none_or(|l| l == me) && !s.applied).map(|s| s.id).collect();
    if others.iter().any(|id| *id < me) {
        return Role::Wait(TURN);
    }
    if others.is_empty() { Role::Solo } else { Role::Lead(others) }
}

fn ids(v: &Value) -> Vec<Uuid> { v.as_array().into_iter().flatten().filter_map(|x| x.as_str()).filter_map(|s| Uuid::try_parse(s).ok()).collect() }

/// 同批别的作业
async fn siblings(db: &sqlx::PgPool, batch: &str, me: Uuid) -> sqlx::Result<Vec<Sib>> {
    type Row = (Uuid, String, String, i32, i32, String, Option<String>, Option<Value>, Option<Value>, Option<String>, Option<Value>, bool);
    let rows: Vec<Row> = sqlx::query_as(
        "select j.id, coalesce(display_name(m.kind::text, m.label, t.name, m.number), '（已删除）'), j.state::text, j.step, s.idx, s.state::text, s.error, s.output,
                j.params->'routers_batch', j.params->>'routers_batch_leader', j.params->'routers_stuck',
                coalesce(jsonb_typeof(j.params->'routers_applied') = 'array' and jsonb_array_length(j.params->'routers_applied') > 0, false)
         from job j join job_step s on s.job_id = j.id and s.key = 'routers'
              left join machine m on m.id = j.target_id left join team t on t.id = m.team_id
         where j.kind = 'move_entry' and j.params->>'batch' = $1 and j.id <> $2",
    )
    .bind(batch)
    .bind(me)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, name, state, step, at, step_state, error, output, batch, leader, stuck, applied)| Sib {
            id,
            name,
            state,
            step,
            at,
            step_state,
            error,
            output: output.unwrap_or(Value::Null),
            batch: batch.as_ref().map(ids).unwrap_or_default(),
            leader: leader.and_then(|s| Uuid::try_parse(&s).ok()),
            stuck: stuck.unwrap_or(Value::Null),
            applied,
        })
        .collect())
}

/// 带头的步骤输出里属于 `job` 的出口（每台路由的 exits 与 delay_ms 一一对应，见 replace.rs 的输出）
fn only(out: &Value, job: Uuid) -> Vec<Value> {
    let me = json!(job);
    out["routers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let idx: Vec<usize> = r["exits"].as_array()?.iter().enumerate().filter(|(_, x)| x["job"] == me).map(|(i, _)| i).collect();
            if idx.is_empty() {
                return None;
            }
            let mut v = r.clone();
            v["exits"] = idx.iter().map(|i| r["exits"][*i].clone()).collect();
            v["delay_ms"] = idx.iter().map(|i| r["delay_ms"][*i].clone()).collect();
            Some(v)
        })
        .collect()
}

/// 认领本作业的已经不再带它了（被取消、改别的一批）：放开。只在还是它时清，免得清掉刚认领的
async fn release(c: &Ctx<'_>, flag: Option<Uuid>) -> anyhow::Result<()> {
    if let Some(f) = flag {
        sqlx::query("update job set params = params || '{\"routers_batch_leader\": null}' where id = $1 and lease_owner = $2 and params->>'routers_batch_leader' = $3")
            .bind(c.job.id)
            .bind(c.owner)
            .bind(f.to_string())
            .execute(&c.st.db)
            .await?;
    }
    Ok(())
}

/// 排队：说法变了才写日志（再领几次照样排队不重复写）
async fn wait(c: &Ctx<'_>, why: String, patch: Value) -> anyhow::Result<Outcome> {
    if c.job.params["routers_queued"].as_str() != Some(why.as_str()) {
        c.log(&why).await;
    }
    let mut p = json!({"routers_queued": why, "routers_queued_id": null});
    if let (Some(p), Value::Object(x)) = (p.as_object_mut(), patch) {
        p.extend(x);
    }
    c.set_params(p).await?;
    Ok(Outcome::Queue(10))
}

/// 同批一起改（`batch` 是参数 batch）。Ok(Ok(认领下来的作业))：接着改，空 = 自己改；Ok(Err(结果))：这一轮到此为止（排队、跟着完成）；
/// Err：跟着带头的失败
pub(super) async fn join(c: &Ctx<'_>, batch: &str) -> anyhow::Result<Result<Vec<JobRow>, Outcome>> {
    let me = c.job.id;
    // 认领随时可能发生：读库里现在的，不用领取时的参数
    let flag: Option<String> = sqlx::query_scalar("select params->>'routers_batch_leader' from job where id = $1").bind(me).fetch_one(&c.st.db).await?;
    let flag = flag.and_then(|s| Uuid::try_parse(&s).ok());
    let sibs = siblings(&c.st.db, batch, me).await?;
    match role(me, flag, &sibs) {
        Role::Follow(l) => {
            let why = format!("同批搬空的「{}」带头一起改软路由出口（本节点的也在里面），等它改完", l.name);
            Ok(Err(wait(c, why, json!({"routers_batch_leader": l.id})).await?))
        }
        Role::Wait(why) => {
            release(c, flag).await?;
            Ok(Err(wait(c, why.to_string(), json!({})).await?))
        }
        Role::Done(l) => {
            release(c, flag).await?;
            c.set_params(json!({"routers_queued": null})).await?;
            let mine = only(&l.output, me);
            let done = format!("同批搬空的「{}」带头一起改了软路由出口", l.name);
            c.log(&done).await;
            Ok(Err(if mine.is_empty() { Outcome::Skipped(format!("{done}，没有出口指向本节点的旧入口")) } else { Outcome::Done(json!({"routers": mine, "batch_leader": l.id})) }))
        }
        Role::Fail(l) => {
            let err = l.error.as_deref().unwrap_or("原因见它的作业");
            let msg = if l.stuck() {
                c.set_params(json!({"routers_queued": null, "routers_batch_leader": l.id, "routers_stuck": l.stuck})).await?;
                format!("同批搬空的「{}」带头一起改软路由出口（本节点的也在里面），改回失败、需要人工处理：{err}。先处理好并重试它那个作业，它改完之后再重试本作业", l.name)
            } else {
                release(c, flag).await?;
                c.set_params(json!({"routers_queued": null, "routers_stuck": []})).await?;
                format!("同批搬空的「{}」带头一起改软路由出口（本节点的也在里面）没成，已一起换回：{err}", l.name)
            };
            Err(StepError::err(ErrKind::Remote, msg))
        }
        Role::Lead(others) => {
            let mut tx = c.st.db.begin().await?;
            let claimed: Vec<JobRow> = sqlx::query_as(
                "update job j set params = j.params || jsonb_build_object('routers_batch_leader', $1::text)
                 where j.id = any($2) and j.kind = 'move_entry' and j.params->>'batch' = $3 and j.state in ('queued', 'running', 'waiting')
                   and coalesce(j.params->>'routers_batch_leader', $1::text) = $1::text and j.step = (select s.idx from job_step s where s.job_id = j.id and s.key = 'routers')
                 returning j.id, j.kind, j.target_id, j.params, j.state::text state, j.step, j.created_by",
            )
            .bind(me)
            .bind(&others)
            .bind(batch)
            .fetch_all(&mut *tx)
            .await?;
            let ids: Vec<Uuid> = claimed.iter().map(|j| j.id).collect();
            let n = sqlx::query("update job set params = params || $2 where id = $1 and lease_owner = $3")
                .bind(me)
                .bind(json!({"routers_batch": ids, "routers_batch_leader": null, "routers_queued": null}))
                .bind(c.owner)
                .execute(&mut *tx)
                .await?
                .rows_affected();
            if n == 0 {
                bail!("作业租约已不属于本执行者，参数没有写入");
            }
            tx.commit().await?;
            if !claimed.is_empty() {
                let names: Vec<&str> = sibs.iter().filter(|s| ids.contains(&s.id)).map(|s| s.name.as_str()).collect();
                c.log(format!("同批搬空的 {} 个节点（{}）一起改软路由出口：每台路由只改一次、重启一次", claimed.len() + 1, names.join("、"))).await;
            }
            Ok(Ok(claimed))
        }
        Role::Solo => {
            release(c, flag).await?;
            Ok(Ok(vec![]))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sib(n: u128, state: &str, step: i32) -> Sib {
        Sib {
            id: Uuid::from_u128(n),
            name: format!("团队2-0{n}"),
            state: state.into(),
            step,
            at: 4,
            step_state: if step == 4 { "running".into() } else if step > 4 { "done".into() } else { "pending".into() },
            error: None,
            output: Value::Null,
            batch: vec![],
            leader: None,
            stuck: Value::Null,
            applied: false,
        }
    }

    /// 同批都走到改路由这一步才动手，id 最小的带头认领其余的；还有没走到的就等，失败停着的不等
    #[test]
    fn smallest_leads_once_all_arrive() {
        let me = Uuid::from_u128(1);
        let mut sibs = vec![sib(2, "waiting", 4), sib(3, "running", 3)];
        assert_eq!(role(me, None, &sibs), Role::Wait(BEHIND));
        sibs[1].state = "failed".into();
        assert_eq!(role(me, None, &sibs), Role::Lead(vec![Uuid::from_u128(2)]), "失败停着的不等");
        assert_eq!(role(Uuid::from_u128(5), None, &sibs), Role::Wait(TURN), "id 小的带头");
        sibs[0].state = "succeeded".into();
        sibs[0].step = 8;
        assert_eq!(role(me, None, &sibs), Role::Solo, "同批别的都改过了：自己改");
    }

    /// 被认领的跟着带头的：它在改就等，做完跟着完成，失败跟着失败一次（放开后重试重新推），改回失败一直跟着
    #[test]
    fn follower_tracks_leader() {
        let (me, lid) = (Uuid::from_u128(2), Uuid::from_u128(1));
        let mut l = sib(1, "running", 4);
        l.batch = vec![me];
        let mut sibs = vec![l, sib(3, "waiting", 4)];
        assert!(matches!(role(me, Some(lid), &sibs), Role::Follow(x) if x.id == lid));
        sibs[0].state = "failed".into();
        sibs[0].step_state = "failed".into();
        assert!(matches!(role(me, Some(lid), &sibs), Role::Fail(x) if x.id == lid));
        // 跟着失败过（flag 清了）之后重试：不再跟着它，同批别的停在这一步的由 id 小的带头
        assert_eq!(role(me, None, &sibs), Role::Lead(vec![Uuid::from_u128(3)]));
        sibs[0].stuck = json!([{"id": Uuid::nil()}]);
        assert!(matches!(role(me, None, &sibs), Role::Fail(_)), "改回失败：一直跟着");
        sibs[0].state = "succeeded".into();
        sibs[0].step = 8;
        sibs[0].step_state = "done".into();
        assert!(matches!(role(me, None, &sibs), Role::Done(x) if x.id == lid));
        // 带头的被取消（排队等锁时）：放开，重新推
        sibs[0].state = "cancelled".into();
        sibs[0].step = 4;
        sibs[0].step_state = "compensated".into();
        assert_eq!(role(me, Some(lid), &sibs), Role::Lead(vec![Uuid::from_u128(3)]));
    }

    /// 已被别的认领的、自己接着做完的不认领；同批有另一个带头的在改就等它
    #[test]
    fn claimed_and_resuming_are_left_alone() {
        let me = Uuid::from_u128(1);
        let mut sibs = vec![sib(2, "waiting", 4), sib(3, "waiting", 4)];
        sibs[0].leader = Some(Uuid::from_u128(9));
        sibs[1].applied = true;
        assert_eq!(role(me, None, &sibs), Role::Solo);
        sibs[0].leader = Some(me);
        assert_eq!(role(me, None, &sibs), Role::Lead(vec![Uuid::from_u128(2)]), "已经跟着本作业的照样认领");
        sibs[1].batch = vec![Uuid::from_u128(2)];
        assert_eq!(role(me, None, &sibs), Role::Wait(BUSY));
    }

    #[test]
    fn follower_output_is_its_own_exits() {
        let (a, b) = (Uuid::from_u128(1), Uuid::from_u128(2));
        let out = json!({"routers": [
            {"id": "r1", "name": "路由A", "exits": [{"exit": "节点01", "to": "x:1", "job": a}, {"exit": "节点02", "to": "x:2", "job": b}], "backup": "/b", "delay_ms": [10, 20]},
            {"id": "r2", "name": "路由B", "exits": [{"exit": "节点01", "to": "x:1", "job": a}], "backup": "/c", "delay_ms": [30]},
        ]});
        let mine = only(&out, b);
        assert_eq!(mine.len(), 1);
        assert_eq!((mine[0]["name"].as_str(), mine[0]["exits"][0]["exit"].as_str(), mine[0]["delay_ms"].clone()), (Some("路由A"), Some("节点02"), json!([20])));
        assert_eq!(only(&out, a).len(), 2);
    }
}
