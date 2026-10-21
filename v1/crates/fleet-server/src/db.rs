//! 连接池、迁移、时序分区维护与小时汇总。

use anyhow::Context;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Postgres, Transaction};
use std::collections::BTreeSet;
use time::{Date, Duration, OffsetDateTime};
use uuid::Uuid;

pub type Tx<'a> = Transaction<'a, Postgres>;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

pub async fn connect(url: &str) -> anyhow::Result<PgPool> {
    PgPoolOptions::new().max_connections(20).connect(url).await.with_context(|| "连接 PostgreSQL")
}

pub fn new_id() -> Uuid { Uuid::now_v7() }

/// 按天分区的时序表
pub const PARTITIONED: &[&str] = &["host_metric", "entry_traffic", "probe_result", "access_event"];

fn part_name(table: &str, d: Date) -> String {
    format!("{table}_{:04}{:02}{:02}", d.year(), u8::from(d.month()), d.day())
}

/// 建好今天起往后 `ahead` 天的分区，删掉早于保留期的分区。幂等。
pub async fn maintain_partitions(db: &PgPool, raw_days: u32, access_days: u32, ahead: i64) -> anyhow::Result<()> {
    let today = OffsetDateTime::now_utc().date();
    for t in PARTITIONED {
        // 只补建缺的分区；建分区要父表的重锁，排在长查询后面会堵住所有上报的 insert，
        // 所以限 2 秒，拿不到就记 warn、下次维护再试（上报时缺分区还会按需补建）
        let days: BTreeSet<Date> = (-1..=ahead).map(|i| today + Duration::days(i)).collect();
        let names: Vec<String> = days.iter().map(|d| part_name(t, *d)).collect();
        let existing: Vec<(String,)> = sqlx::query_as(
            "select c.relname::text from pg_class c join pg_inherits i on i.inhrelid = c.oid join pg_class p on p.oid = i.inhparent
             where p.relname = $1 and c.relname = any($2)",
        )
        .bind(t)
        .bind(&names)
        .fetch_all(db)
        .await
        .with_context(|| format!("查 {t} 已有分区"))?;
        let existing: BTreeSet<String> = existing.into_iter().map(|(n,)| n).collect();
        for d in days {
            let n = part_name(t, d);
            if existing.contains(&n) {
                continue;
            }
            if let Err(e) = create_partition(db, t, d).await {
                tracing::warn!("建分区 {n} 失败（可能被长查询挡住），下次维护再试：{e:#}");
            }
        }
        let keep = if *t == "access_event" { access_days } else { raw_days } as i64;
        let cutoff = part_name(t, today - Duration::days(keep));
        if let Err(e) = drop_expired(db, t, &cutoff).await {
            tracing::warn!("清理 {t} 过期分区失败，下次再试：{e:#}");
        }
    }
    Ok(())
}

/// 删除名字早于 `cutoff` 的分区。先 `detach ... concurrently`（不长时间锁父表）再 drop；
/// 拿不到锁（5 秒）就放弃，下次维护再试。脱离了父表但没删掉的残留表也在这里补删。
async fn drop_expired(db: &PgPool, t: &str, cutoff: &str) -> anyhow::Result<()> {
    let mut conn = db.acquire().await?;
    sqlx::query("set lock_timeout = '5s'").execute(&mut *conn).await?;
    let r = drop_expired_on(&mut conn, t, cutoff).await;
    // 连接要还回池里：无论成败都把超时恢复
    let _ = sqlx::query("reset lock_timeout").execute(&mut *conn).await;
    r
}

async fn drop_expired_on(conn: &mut sqlx::PgConnection, t: &str, cutoff: &str) -> anyhow::Result<()> {
    let attached: Vec<(String,)> = sqlx::query_as(
        "select c.relname::text from pg_inherits i join pg_class c on c.oid = i.inhrelid
         join pg_class p on p.oid = i.inhparent where p.relname = $1 and c.relname < $2 order by 1",
    )
    .bind(t)
    .bind(cutoff)
    .fetch_all(&mut *conn)
    .await?;
    for (n,) in attached {
        let detach = format!("alter table {t} detach partition {n} concurrently");
        if let Err(e) = sqlx::query(&detach).execute(&mut *conn).await {
            // 上次中断的 detach 会停在「待完成」状态，先尝试收尾
            let fin = format!("alter table {t} detach partition {n} finalize");
            if sqlx::query(&fin).execute(&mut *conn).await.is_err() {
                tracing::warn!("分区 {n} 暂时无法摘下（下次再试）：{e}");
                continue;
            }
        }
        match sqlx::query(&format!("drop table if exists {n}")).execute(&mut *conn).await {
            Ok(_) => tracing::info!("删除过期分区 {n}"),
            Err(e) => tracing::warn!("分区 {n} 已摘下但没删掉（下次再试）：{e}"),
        }
    }
    // 摘下后没来得及删的残留表
    let orphans: Vec<(String,)> = sqlx::query_as(
        "select c.relname::text from pg_class c
         where c.relkind = 'r' and not c.relispartition and c.relnamespace = (select oid from pg_namespace where nspname = current_schema())
           and c.relname ~ ('^' || $1 || '_[0-9]{8}$') and c.relname < $2",
    )
    .bind(t)
    .bind(cutoff)
    .fetch_all(&mut *conn)
    .await?;
    for (n,) in orphans {
        match sqlx::query(&format!("drop table if exists {n}")).execute(&mut *conn).await {
            Ok(_) => tracing::info!("删除残留分区表 {n}"),
            Err(e) => tracing::warn!("残留分区表 {n} 没删掉（下次再试）：{e}"),
        }
    }
    Ok(())
}

/// 按需补建某些天的分区（agent 补传几天前的数据时，常规维护只建了昨天起的分区）。
/// 返回建不出来的那些天；调用方丢弃这些天的样本，而不是让整份上报失败。
pub async fn ensure_partitions(db: &PgPool, table: &str, days: &BTreeSet<Date>) -> Vec<Date> {
    if days.is_empty() {
        return vec![];
    }
    let names: Vec<String> = days.iter().map(|d| part_name(table, *d)).collect();
    let existing: Vec<(String,)> = sqlx::query_as(
        "select c.relname::text from pg_class c join pg_inherits i on i.inhrelid = c.oid where c.relname = any($1)",
    )
    .bind(&names)
    .fetch_all(db)
    .await
    .unwrap_or_default();
    let existing: BTreeSet<String> = existing.into_iter().map(|(n,)| n).collect();
    let mut failed = vec![];
    for d in days {
        let n = part_name(table, *d);
        if existing.contains(&n) {
            continue;
        }
        if let Err(e) = create_partition(db, table, *d).await {
            // 并发的另一份上报可能刚建好
            let again: Option<(String,)> = sqlx::query_as(
                "select c.relname::text from pg_class c join pg_inherits i on i.inhrelid = c.oid where c.relname = $1",
            )
            .bind(&n)
            .fetch_optional(db)
            .await
            .unwrap_or(None);
            if again.is_none() {
                tracing::warn!("按需建分区 {n} 失败：{e:#}");
                failed.push(*d);
            }
        }
    }
    failed
}

async fn create_partition(db: &PgPool, table: &str, d: Date) -> anyhow::Result<()> {
    let n = part_name(table, d);
    let mut conn = db.acquire().await?;
    // 建分区要父表的重锁：2 秒拿不到就放弃，不堵住别的写入
    sqlx::query("set lock_timeout = '2s'").execute(&mut *conn).await?;
    let sql = format!("create table if not exists {n} partition of {table} for values from ('{d}') to ('{e}')", e = d + Duration::days(1));
    let r = sqlx::query(&sql).execute(&mut *conn).await;
    let _ = sqlx::query("reset lock_timeout").execute(&mut *conn).await;
    r?;
    Ok(())
}

/// 一轮最多补算的脏小时数（长时间断网后的积压分几轮消化）
const DIRTY_BATCH: i64 = 96;

/// 向下取整到 UTC 整点
pub fn floor_hour(t: OffsetDateTime) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(t.unix_timestamp().div_euclid(3600) * 3600).unwrap_or(t)
}

/// 按脏标记补算小时汇总的一轮（遥测 rollup_dirty、访问 access_dirty 共用）：当前小时必算；被标脏的小时
/// （有新原始数据落进来的整点，含中控停机后 agent 补传的旧数据）补算。
/// 上一小时关闭后不再无条件重算——每轮算完当前小时会给它留一个脏标记，所以它在下一个小时的第一轮恰好再算一次收尾，
/// 之后只有迟到数据到来（上报时标脏）才会再算。
/// 标记按 (小时, touched_at) 精确删除：补算期间又有新数据落进来的小时，标记已被刷新，不会被误删。
pub async fn rollup_dirty<F, Fut>(db: &PgPool, table: &str, roll: F) -> anyhow::Result<()>
where
    F: Fn(OffsetDateTime) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    let cur = floor_hour(OffsetDateTime::now_utc());
    let dirty: Vec<(OffsetDateTime, OffsetDateTime)> = sqlx::query_as(&format!("select hour, touched_at from {table} where hour <= $1 order by hour limit $2"))
        .bind(cur)
        .bind(DIRTY_BATCH)
        .fetch_all(db)
        .await?;
    let mut hours: BTreeSet<OffsetDateTime> = dirty.iter().map(|d| d.0).collect();
    hours.insert(cur);
    for h in &hours {
        roll(*h).await?;
    }
    if !dirty.is_empty() {
        let (hs, ts): (Vec<OffsetDateTime>, Vec<OffsetDateTime>) = dirty.into_iter().unzip();
        sqlx::query(&format!("delete from {table} d using unnest($1::timestamptz[], $2::timestamptz[]) u(h, t) where d.hour = u.h and d.touched_at = u.t"))
            .bind(&hs)
            .bind(&ts)
            .execute(db)
            .await?;
    }
    sqlx::query(&format!(
        "insert into {table} (hour, touched_at) values ($1, clock_timestamp()) on conflict (hour) do update set touched_at = excluded.touched_at"
    ))
    .bind(cur)
    .execute(db)
    .await?;
    Ok(())
}

/// 遥测小时汇总的一轮（脏小时语义见 `rollup_dirty`）。同一小时从原始数据重算并覆盖，幂等。
pub async fn rollup_hours(db: &PgPool) -> anyhow::Result<()> { rollup_dirty(db, "rollup_dirty", |h| rollup_hour(db, h)).await }

/// 重算一个整点小时。主机网络流量取相邻样本的正增量之和（lag），计数器回绕（重启清零）时
/// 当前读数就是重启后的增量；上一小时最后一个样本参与第一个增量的计算，不丢小时交界处的流量。
async fn rollup_hour(db: &PgPool, hour: OffsetDateTime) -> anyhow::Result<()> {
    let q = [
        "insert into host_metric_hourly (machine_id, hour, cpu_avg, cpu_max, mem_used_avg, mem_total_max, disk_used_max, disk_total_max, net_rx, net_tx)
         select machine_id, $1, avg(cpu_pct), max(cpu_pct), avg(mem_used)::bigint, max(mem_total), max(disk_used), max(disk_total),
                coalesce(sum(case when rx is null or prev_rx is null then 0 when rx >= prev_rx then rx - prev_rx else rx end), 0)::bigint,
                coalesce(sum(case when tx is null or prev_tx is null then 0 when tx >= prev_tx then tx - prev_tx else tx end), 0)::bigint
         from (select machine_id, ts, cpu_pct, mem_used, mem_total, disk_used, disk_total,
                      net_rx_bytes rx, lag(net_rx_bytes) over w prev_rx, net_tx_bytes tx, lag(net_tx_bytes) over w prev_tx
               from host_metric where ts >= $1 - interval '1 hour' and ts < $1 + interval '1 hour'
               window w as (partition by machine_id order by ts)) x
         where ts >= $1 group by machine_id
         on conflict (machine_id, hour) do update set cpu_avg = excluded.cpu_avg, cpu_max = excluded.cpu_max,
           mem_used_avg = excluded.mem_used_avg, mem_total_max = excluded.mem_total_max, disk_used_max = excluded.disk_used_max,
           disk_total_max = excluded.disk_total_max, net_rx = excluded.net_rx, net_tx = excluded.net_tx",
        "insert into entry_traffic_hourly (entry_id, hour, up_bytes, down_bytes, conns_max)
         select entry_id, $1, sum(up_bytes), sum(down_bytes), max(conns)
         from entry_traffic where ts >= $1 and ts < $1 + interval '1 hour' group by entry_id
         on conflict (entry_id, hour) do update set up_bytes = excluded.up_bytes, down_bytes = excluded.down_bytes, conns_max = excluded.conns_max",
        "insert into probe_hourly (entry_id, hour, kind, total, ok, latency_avg)
         select entry_id, $1, kind, count(*), count(*) filter (where ok), avg(latency_ms) filter (where ok)
         from probe_result where ts >= $1 and ts < $1 + interval '1 hour' group by entry_id, kind
         on conflict (entry_id, hour, kind) do update set total = excluded.total, ok = excluded.ok, latency_avg = excluded.latency_avg",
    ];
    for s in q {
        sqlx::query(s).bind(hour).execute(db).await?;
    }
    Ok(())
}

/// 保留期清理（每小时一次）：已恢复超 180 天的告警、过期会话、超 1 天的登录失败记录、
/// 超 90 天的通知记录、结束超 90 天的静默、超 365 天的审计。
pub async fn cleanup_retention(db: &PgPool) -> anyhow::Result<()> {
    let q = [
        ("告警", "delete from incident where state = 'resolved' and resolved_at < now() - interval '180 days'"),
        ("会话", "delete from session where expires_at < now()"),
        ("登录失败", "delete from login_failure where ts < now() - interval '1 day'"),
        ("通知记录", "delete from notify_log where ts < now() - interval '90 days'"),
        ("静默", "delete from silence where ends_at < now() - interval '90 days'"),
        ("审计", "delete from audit_log where ts < now() - interval '365 days'"),
    ];
    for (what, sql) in q {
        let n = sqlx::query(sql).execute(db).await.with_context(|| format!("清理{what}"))?.rows_affected();
        if n > 0 {
            tracing::info!("保留期清理：{what} {n} 条");
        }
    }
    Ok(())
}

/// 彻底删除会连带删掉这台机器入口的流量，而入口另一端未退役的机器拿这些流量算今日/本月/周期已用：
/// 近 35 天里还有这类流量时不能删。另一端已退役（统计不再用、也在等删）的不拦，否则两台互为对端的退役机器谁都删不掉。
/// 返回给用户看的原因；None = 可以删。
pub async fn purge_blocker(db: impl sqlx::PgExecutor<'_>, machine: Uuid) -> sqlx::Result<Option<String>> {
    let until: Option<Date> = sqlx::query_scalar(
        "select ((max(h.hour) + interval '35 days 1 hour') at time zone 'Asia/Shanghai')::date + 1
         from entry e join entry_traffic_hourly h on h.entry_id = e.id
              join machine o on o.id = case when e.node_id = $1 then e.relay_id else e.node_id end
         where (e.node_id = $1 or e.relay_id = $1) and h.hour >= now() - interval '35 days' and o.lifecycle <> 'retired'",
    )
    .bind(machine)
    .fetch_one(db)
    .await?;
    Ok(until.map(|d| format!("这台机器的入口在 {d} 前仍计入其他机器的流量统计，届时再彻底删除")))
}

pub async fn audit(tx: &mut Tx<'_>, actor: &str, action: &str, target_kind: &str, target_id: Option<Uuid>, detail: serde_json::Value) -> sqlx::Result<()> {
    sqlx::query("insert into audit_log (actor, action, target_kind, target_id, detail) values ($1, $2, $3, $4, $5)")
        .bind(actor)
        .bind(action)
        .bind(target_kind)
        .bind(target_id)
        .bind(detail)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// 线路机期望状态 +1，返回新代数。调用方在提交后调用 `AppState::relay_bumped`。
pub async fn bump_relay(tx: &mut Tx<'_>, relay_id: Uuid) -> sqlx::Result<i64> {
    let (g,): (i64,) = sqlx::query_as("update relay set generation = generation + 1, generation_at = now() where machine_id = $1 returning generation")
        .bind(relay_id)
        .fetch_one(&mut **tx)
        .await?;
    Ok(g)
}

/// 把团队的未退役节点编号压缩成 1..n（锁团队行；新号 ≤ 旧号，按新号升序更新不撞唯一索引）。
/// 写审计；返回 (改动列表 (id, 旧号, 新号), 需要刷新的线路机)。
pub async fn compact_team(tx: &mut Tx<'_>, team_id: Uuid, actor: &str, why: &str) -> sqlx::Result<(Vec<(Uuid, i32, i32)>, Vec<Uuid>)> {
    let (team,): (String,) = sqlx::query_as("select name from team where id = $1 for update").bind(team_id).fetch_one(&mut **tx).await?;
    let nodes: Vec<(Uuid, i32)> = sqlx::query_as(
        "select id, number from machine where team_id = $1 and kind <> 'relay' and lifecycle <> 'retired' order by number",
    )
    .bind(team_id)
    .fetch_all(&mut **tx)
    .await?;
    let changes = crate::names::compact_numbers(nodes);
    for (id, _old, new) in &changes {
        sqlx::query("update machine set number = $2, revision = revision + 1, updated_at = now() where id = $1")
            .bind(id)
            .bind(new)
            .execute(&mut **tx)
            .await?;
    }
    let relays = bump_relays_of_nodes(tx, &changes.iter().map(|c| c.0).collect::<Vec<_>>()).await?;
    if !changes.is_empty() {
        let detail: Vec<serde_json::Value> = changes
            .iter()
            .map(|(id, o, n)| serde_json::json!({"id": id, "from": crate::names::node_name(&team, *o), "to": crate::names::node_name(&team, *n)}))
            .collect();
        audit(tx, actor, "team.renumber", "team", Some(team_id), serde_json::json!({"why": why, "changes": detail})).await?;
    }
    Ok((changes, relays))
}

/// 节点显示名变化后：刷新它们所有在用入口所在线路机的代数（备注随之同步）。
/// 按线路机 id 顺序加锁，和其它同时改多台线路机的事务同序。返回涉及的线路机（已排序去重）。
pub async fn bump_relays_of_nodes(tx: &mut Tx<'_>, node_ids: &[Uuid]) -> sqlx::Result<Vec<Uuid>> {
    let relays: Vec<Uuid> = sqlx::query_scalar("select distinct relay_id from entry where node_id = any($1) and lifecycle <> 'removed' order by relay_id")
        .bind(node_ids)
        .fetch_all(&mut **tx)
        .await?;
    for r in &relays {
        bump_relay(tx, *r).await?;
    }
    Ok(relays)
}
