//! 连接池、迁移、时序分区维护与小时汇总。

use anyhow::Context;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Postgres, Transaction};
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
        for i in -1..=ahead {
            let d = today + Duration::days(i);
            let n = part_name(t, d);
            let sql = format!(
                "create table if not exists {n} partition of {t} for values from ('{d}') to ('{e}')",
                e = d + Duration::days(1)
            );
            sqlx::query(&sql).execute(db).await.with_context(|| format!("建分区 {n}"))?;
        }
        let keep = if *t == "access_event" { access_days } else { raw_days } as i64;
        let cutoff = part_name(t, today - Duration::days(keep));
        let old: Vec<(String,)> = sqlx::query_as(
            "select c.relname::text from pg_inherits i join pg_class c on c.oid = i.inhrelid
             join pg_class p on p.oid = i.inhparent where p.relname = $1 and c.relname < $2",
        )
        .bind(t)
        .bind(&cutoff)
        .fetch_all(db)
        .await?;
        for (n,) in old {
            sqlx::query(&format!("drop table if exists {n}")).execute(db).await?;
            tracing::info!("删除过期分区 {n}");
        }
    }
    Ok(())
}

/// 把最近两小时的原始数据汇总进小时表（幂等：同一小时重算覆盖）。
pub async fn rollup_hours(db: &PgPool) -> anyhow::Result<()> {
    let q = [
        "insert into host_metric_hourly (machine_id, hour, cpu_avg, cpu_max, mem_used_avg, mem_total_max, disk_used_max, disk_total_max, net_rx, net_tx)
         select machine_id, date_trunc('hour', ts) h, avg(cpu_pct), max(cpu_pct), avg(mem_used)::bigint, max(mem_total), max(disk_used), max(disk_total),
                greatest(max(net_rx_bytes) - min(net_rx_bytes), 0), greatest(max(net_tx_bytes) - min(net_tx_bytes), 0)
         from host_metric where ts >= date_trunc('hour', now()) - interval '1 hour' group by 1, 2
         on conflict (machine_id, hour) do update set cpu_avg = excluded.cpu_avg, cpu_max = excluded.cpu_max,
           mem_used_avg = excluded.mem_used_avg, mem_total_max = excluded.mem_total_max, disk_used_max = excluded.disk_used_max,
           disk_total_max = excluded.disk_total_max, net_rx = excluded.net_rx, net_tx = excluded.net_tx",
        "insert into entry_traffic_hourly (entry_id, hour, up_bytes, down_bytes, conns_max)
         select entry_id, date_trunc('hour', ts) h, sum(up_bytes), sum(down_bytes), max(conns)
         from entry_traffic where ts >= date_trunc('hour', now()) - interval '1 hour' group by 1, 2
         on conflict (entry_id, hour) do update set up_bytes = excluded.up_bytes, down_bytes = excluded.down_bytes, conns_max = excluded.conns_max",
        "insert into probe_hourly (entry_id, hour, kind, total, ok, latency_avg)
         select entry_id, date_trunc('hour', ts) h, kind, count(*), count(*) filter (where ok), avg(latency_ms) filter (where ok)
         from probe_result where ts >= date_trunc('hour', now()) - interval '1 hour' group by 1, 2, 3
         on conflict (entry_id, hour, kind) do update set total = excluded.total, ok = excluded.ok, latency_avg = excluded.latency_avg",
    ];
    for s in q {
        sqlx::query(s).execute(db).await?;
    }
    Ok(())
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
    let (g,): (i64,) = sqlx::query_as("update relay set generation = generation + 1 where machine_id = $1 returning generation")
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
    let mut relays = vec![];
    for (id, _old, new) in &changes {
        sqlx::query("update machine set number = $2, revision = revision + 1, updated_at = now() where id = $1")
            .bind(id)
            .bind(new)
            .execute(&mut **tx)
            .await?;
        relays.extend(bump_relays_of_node(tx, *id).await?);
    }
    if !changes.is_empty() {
        let detail: Vec<serde_json::Value> = changes
            .iter()
            .map(|(id, o, n)| serde_json::json!({"id": id, "from": crate::names::node_name(&team, *o), "to": crate::names::node_name(&team, *n)}))
            .collect();
        audit(tx, actor, "team.renumber", "team", Some(team_id), serde_json::json!({"why": why, "changes": detail})).await?;
    }
    relays.sort();
    relays.dedup();
    Ok((changes, relays))
}

/// 节点显示名变化后：刷新它所有在用入口所在线路机的代数（备注随之同步）。返回涉及的线路机。
pub async fn bump_relays_of_node(tx: &mut Tx<'_>, node_id: Uuid) -> sqlx::Result<Vec<Uuid>> {
    let relays: Vec<(Uuid,)> = sqlx::query_as("select distinct relay_id from entry where node_id = $1 and lifecycle <> 'removed'")
        .bind(node_id)
        .fetch_all(&mut **tx)
        .await?;
    let mut out = vec![];
    for (r,) in relays {
        bump_relay(tx, r).await?;
        out.push(r);
    }
    Ok(out)
}
