//! 软路由配置的变更时间线：采集发现 OpenClash 配置文件的 sha256 变了（和上一份存下来的快照比），就记一条 router_config_change——
//! 时间、新旧 sha、出口的增删改、来源。只记录，不提供回滚：中控作业失败时本来就会自动改回，换线之后旧入口已摘掉，回到旧备份只会断网。
//!
//! - 差异：按出口名比 server:port 与凭据（类型、uuid、flow、SNI、公钥、short-id）是否变，凭据只记「变了」，不存明文。
//!   比不出来的（新配置解析不了、中控重启后内存里没有旧配置）差异为空、原因写进 note。
//! - 来源：中控作业改配置（替换 / 换线路机 / 改出口指向 / 同步出口，jobs/steps/replace.rs）动手前记一笔「在改」（[`Claims::editing`]），
//!   脚本回话后记上改完 / 改回后配置的 sha（[`Claims::edited`]）；采集发现变化时找这台路由最近 10 分钟内改过的作业：
//!   改完的 sha 与新 sha 一致，或脚本还在跑（还没有 sha）的就算它。找不到就是路由上改的（手改，或团队1 旁路由上的看门服务重新生成配置）。
//!   这些记录只在内存里（中控重启就没了，之后的变化记成「路由上改的」）。
//! - 每台路由只留最近 [`KEEP`] 条。

use super::clash::Proxy;
use serde_json::{json, Value};
use sqlx::PgConnection;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use time::OffsetDateTime;
use uuid::Uuid;

/// 每台路由留最近这么多条
pub const KEEP: i64 = 200;
/// 作业改完（脚本回话）之后多久内的变化算它的
const DONE_WINDOW: Duration = Duration::from_secs(600);
/// 作业脚本在跑时最多算它多久（改配置脚本最长 7 分钟 + 断线重连等回话，见 routers/replace.rs 与 jobs/steps/replace.rs）
const RUNNING_WINDOW: Duration = Duration::from_secs(1200);

struct Claim {
    job: Uuid,
    kind: String,
    /// 改完 / 改回后配置的 sha256；脚本还在跑、或没拿到回话时为空
    sha: Option<String>,
    until: Instant,
}

/// 中控作业改了哪台路由的配置（只在内存里，见模块说明）
#[derive(Default)]
pub struct Claims(Mutex<HashMap<Uuid, Vec<Claim>>>);

impl Claims {
    /// 作业要在这台路由上动手了（改配置或改回，脚本跑之前调用）
    pub fn editing(&self, router: Uuid, job: Uuid, kind: &str) {
        let now = Instant::now();
        let mut m = self.0.lock().unwrap();
        let v = m.entry(router).or_default();
        v.retain(|c| c.until > now);
        match v.iter_mut().find(|c| c.job == job) {
            Some(c) => (c.sha, c.until) = (None, now + RUNNING_WINDOW),
            None => v.push(Claim { job, kind: kind.to_string(), sha: None, until: now + RUNNING_WINDOW }),
        }
    }

    /// 脚本回话了（成功、自己改回了、改回了都算）：`sha` 是它报的改完后配置的 sha256，没拿到为 None
    pub fn edited(&self, router: Uuid, job: Uuid, sha: Option<&str>) {
        let mut m = self.0.lock().unwrap();
        if let Some(c) = m.get_mut(&router).and_then(|v| v.iter_mut().find(|c| c.job == job)) {
            (c.sha, c.until) = (sha.map(String::from), Instant::now() + DONE_WINDOW);
        }
    }

    /// 新 sha 是不是某个作业改出来的：sha 一致的优先，其次是脚本还在跑 / 没拿到 sha 的。返回 (作业 id, 作业类型)
    fn find(&self, router: Uuid, sha: &str) -> Option<(Uuid, String)> {
        let now = Instant::now();
        let m = self.0.lock().unwrap();
        let live: Vec<&Claim> = m.get(&router)?.iter().rev().filter(|c| c.until > now).collect();
        live.iter().find(|c| c.sha.as_deref() == Some(sha)).or_else(|| live.iter().find(|c| c.sha.is_none())).map(|c| (c.job, c.kind.clone()))
    }
}

/// 一次配置变化
pub struct Change {
    pub old_sha: String,
    pub new_sha: String,
    pub job: Option<(Uuid, String)>,
    pub diff: Vec<Value>,
    pub note: Option<String>,
}

/// 某个 sha 对应的配置里的出口：解析成功才有（采集 Mem 里的两种形状共用）
pub type Parsed<'a> = Option<(&'a str, Option<&'a [Proxy]>)>;

/// 认出一次变化：`old` / `new` 是旧 / 新配置的 (sha, 解析出的出口)；`new_err` 是新配置读不了的原因
pub fn detect(claims: &Claims, router: Uuid, old_sha: &str, new_sha: &str, old: Parsed, new: Parsed, new_err: Option<&str>) -> Change {
    let old = old.filter(|o| o.0 == old_sha).and_then(|o| o.1);
    let new = new.filter(|n| n.0 == new_sha).and_then(|n| n.1);
    let (diff, note) = match (old, new) {
        (Some(o), Some(n)) => (diff(o, n), None),
        (_, None) => (vec![], Some(format!("新配置读不了，比不出出口差异：{}", new_err.unwrap_or("没有取到")))),
        (None, Some(_)) => (vec![], Some("没有旧配置可比（中控重启过，或旧配置解析不了），比不出出口差异".to_string())),
    };
    Change { old_sha: old_sha.into(), new_sha: new_sha.into(), job: claims.find(router, new_sha), diff, note }
}

fn target(p: &Proxy) -> String { format!("{}:{}", p.server.as_deref().unwrap_or("?"), p.port.map_or("?".to_string(), |x| x.to_string())) }

/// 出口的增删改（按出口名，配置顺序）：added 带 to；removed 带 from；changed 在 server:port 变了时带 from / to，凭据（含类型）变了时 creds = true
pub fn diff(old: &[Proxy], new: &[Proxy]) -> Vec<Value> {
    let by = |ps: &[Proxy]| {
        let mut m: HashMap<String, Proxy> = HashMap::new();
        for p in ps {
            m.entry(p.name.clone()).or_insert_with(|| p.clone());
        }
        m
    };
    let (o, n) = (by(old), by(new));
    let mut seen = HashSet::new();
    let mut out = vec![];
    for p in new.iter().filter(|p| seen.insert(p.name.clone())) {
        let p = &n[&p.name];
        match o.get(&p.name) {
            None => out.push(json!({"exit": p.name, "op": "added", "to": target(p)})),
            Some(q) => {
                let (moved, creds) = (target(q) != target(p), q.kind != p.kind || q.creds != p.creds);
                if moved || creds {
                    let mut x = json!({"exit": p.name, "op": "changed"});
                    if moved {
                        x["from"] = json!(target(q));
                        x["to"] = json!(target(p));
                    }
                    if creds {
                        x["creds"] = json!(true);
                    }
                    out.push(x);
                }
            }
        }
    }
    let mut seen = HashSet::new();
    for q in old.iter().filter(|q| seen.insert(q.name.clone())) {
        if !n.contains_key(&q.name) {
            out.push(json!({"exit": q.name, "op": "removed", "from": target(&o[&q.name])}));
        }
    }
    out
}

/// 记一条，并把这台路由超出 [`KEEP`] 条的旧记录删掉（和快照在同一个事务里写）
pub async fn record(tx: &mut PgConnection, router: Uuid, at: OffsetDateTime, c: &Change) -> sqlx::Result<()> {
    sqlx::query("insert into router_config_change (id, router_id, at, old_sha, new_sha, job_id, job_kind, diff, note) values ($1, $2, $3, $4, $5, $6, $7, $8, $9)")
        .bind(crate::db::new_id())
        .bind(router)
        .bind(at)
        .bind(&c.old_sha)
        .bind(&c.new_sha)
        .bind(c.job.as_ref().map(|j| j.0))
        .bind(c.job.as_ref().map(|j| j.1.as_str()))
        .bind(Value::Array(c.diff.clone()))
        .bind(&c.note)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "delete from router_config_change where router_id = $1 and id not in
           (select id from router_config_change where router_id = $1 order by at desc, id desc limit $2)",
    )
    .bind(router)
    .bind(KEEP)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// 最近的变更，新的在前。source：`job`（中控作业，带 job_id / job_kind）或 `router`（路由上改的）
pub async fn list(db: &sqlx::PgPool, router: Uuid, limit: i64) -> sqlx::Result<Value> {
    sqlx::query_scalar(
        "select coalesce(json_agg(json_build_object('id', id, 'at', at, 'old_sha', old_sha, 'new_sha', new_sha,
                'source', case when job_id is null then 'router' else 'job' end, 'job_id', job_id, 'job_kind', job_kind, 'diff', diff, 'note', note)
              order by at desc, id desc), '[]')
         from (select * from router_config_change where router_id = $1 order by at desc, id desc limit $2) c",
    )
    .bind(router)
    .bind(limit)
    .fetch_one(db)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routers::clash;

    fn ps(s: &str) -> Vec<Proxy> { clash::parse(s).unwrap() }

    #[test]
    fn diff_by_name() {
        let old = ps("proxies:\n  - {name: a, type: vless, server: s1, port: 1, uuid: u1}\n  - {name: b, type: vless, server: s2, port: 2}\n  - {name: c, type: ss, server: s3, port: 3}\n");
        let new = ps("proxies:\n  - {name: a, type: vless, server: s1, port: 1, uuid: u2}\n  - {name: b, type: vless, server: s9, port: 2}\n  - {name: d, type: ss, server: s4, port: 4}\n");
        assert_eq!(
            diff(&old, &new),
            [
                json!({"exit": "a", "op": "changed", "creds": true}),
                json!({"exit": "b", "op": "changed", "from": "s2:2", "to": "s9:2"}),
                json!({"exit": "d", "op": "added", "to": "s4:4"}),
                json!({"exit": "c", "op": "removed", "from": "s3:3"}),
            ]
        );
        assert!(diff(&old, &old).is_empty());
        assert!(!json!(diff(&old, &new)).to_string().contains("u1"), "凭据不输出");
    }

    #[test]
    fn claims_match_by_sha_or_running() {
        let (c, r, j1, j2) = (Claims::default(), Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        assert!(c.find(r, "x").is_none());
        c.editing(r, j1, "replace");
        assert_eq!(c.find(r, "x"), Some((j1, "replace".into())), "脚本在跑：先算它");
        c.edited(r, j1, Some("x"));
        assert!(c.find(r, "y").is_none() && c.find(r, "x").is_some(), "改完了：sha 对上才算");
        c.editing(r, j2, "router_sync");
        c.edited(r, j2, Some("y"));
        assert_eq!(c.find(r, "x").map(|x| x.0), Some(j1));
        assert_eq!(c.find(r, "y").map(|x| x.0), Some(j2));
        assert!(c.find(Uuid::now_v7(), "x").is_none(), "别的路由不算");
    }
}
