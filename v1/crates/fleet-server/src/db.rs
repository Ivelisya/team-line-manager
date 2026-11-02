//! 连接池、迁移、时序分区维护与小时汇总。

use anyhow::Context;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Postgres, Transaction};
use std::collections::{BTreeSet, HashSet};
use time::{Date, Duration, OffsetDateTime};
use uuid::Uuid;

pub type Tx<'a> = Transaction<'a, Postgres>;

/// 遥测类写入（agent / 探测点上报、各小时汇总）的事务：本事务 `synchronous_commit = off`，提交不等 WAL 落盘。
/// 虚拟盘偶发 fsync 卡顿时，这类写入不再跟着卡住上报和鉴权；只有 PostgreSQL 或宿主机崩溃时会丢最后约几百毫秒已确认的写入（agent 不会重发；汇总下一轮重算），中控进程崩溃不丢。
/// 任务、审计、计费、配置写入不要用它。
pub async fn begin_async(db: &PgPool) -> sqlx::Result<Tx<'static>> {
    let mut tx = db.begin().await?;
    sqlx::query("set local synchronous_commit = off").execute(&mut *tx).await?;
    Ok(tx)
}

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

/// 等空闲连接最多 5 秒（sqlx 默认 30 秒，比 agent 上报的 15 秒超时还长）：池子吃紧时尽快回 503
/// （上报入库按 503 退避重试，见 agent_api::ingest_error），而不是让请求越积越多
pub async fn connect(url: &str) -> anyhow::Result<PgPool> {
    PgPoolOptions::new()
        .max_connections(20)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(url)
        .await
        .with_context(|| "连接 PostgreSQL")
}

/// 库暂时不可用：连接断了、连不上、等不到空闲连接，或库在重启、关停（SQLSTATE 08 类连接异常；57P01 admin_shutdown——
/// 重启时正在跑的查询被断开，57P02 crash_shutdown，57P03 cannot_connect_now——库正在启动或关停）。等库恢复后重做就行，不是这件事本身的错
pub fn unavailable(e: &sqlx::Error) -> bool {
    match e {
        sqlx::Error::Io(_) | sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed => true,
        sqlx::Error::Database(d) => d.code().is_some_and(|c| c.starts_with("08") || matches!(&*c, "57P01" | "57P02" | "57P03")),
        _ => false,
    }
}

/// 同 `unavailable`，看 anyhow 错误链里有没有这样的库错误
pub fn unavailable_in(e: &anyhow::Error) -> bool { e.chain().any(|c| c.downcast_ref::<sqlx::Error>().is_some_and(unavailable)) }

/// 和库的连接断了、连不上，或库在重启、关停（`unavailable` 去掉等不到空闲连接、连接池已关）：中控收不到上报的时段
/// （report_gaps::ReportGaps）只认这些。等不到连接是池子被挤满了——一大批（哪怕没鉴权的）请求就能做到，不能因此把 agent 的离线判定往后推
pub fn connection_lost(e: &sqlx::Error) -> bool { !matches!(e, sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed) && unavailable(e) }

pub fn new_id() -> Uuid { Uuid::now_v7() }

/// 边拼 SQL 边绑定参数：`add` 绑定一个值并返回它的占位符 `$n`，SQL 里的序号和绑定顺序不会对不上。
/// 可选的筛选条件只在给了参数时才拼进 SQL：写成 `($n is null or col = $n)` 的话，PG 对预编译语句改用通用执行计划后
/// 折叠不掉这个条件，索引用不上。拼好后 `sqlx::query_as_with(&sql, args.0)`
#[derive(Default)]
pub struct Args(pub sqlx::postgres::PgArguments);

impl Args {
    pub fn add<'q, T: 'q + sqlx::Encode<'q, Postgres> + sqlx::Type<Postgres>>(&mut self, v: T) -> String {
        // 同 sqlx 自己的 QueryBuilder::push_bind：这里的类型（uuid、文本、整数、时间、数组）编码不会失败
        sqlx::Arguments::add(&mut self.0, v).expect("绑定查询参数");
        format!("${}", sqlx::Arguments::len(&self.0))
    }
}

/// 按天分区的时序表
pub const PARTITIONED: &[&str] = &["host_metric", "entry_traffic", "probe_result", "access_event"];

/// 查分区时只看当前 schema（search_path 的第一个）：同一个库里别的 schema 有同名表时（集成测试每个测试一个 schema）不能算数
const OWN_SCHEMA: &str = "(select oid from pg_namespace where nspname = current_schema())";

fn part_name(table: &str, d: Date) -> String {
    format!("{table}_{:04}{:02}{:02}", d.year(), u8::from(d.month()), d.day())
}

/// 建好今天起往后 `ahead` 天的分区，删掉早于保留期的分区（先过时钟健全检查，见 `drop_before`）：遥测按 `raw_days`
/// （配置文件），访问记录按 `access_days`（面板里设的原始记录保留期，见 settings.rs）。幂等。
pub async fn maintain_partitions(db: &PgPool, raw_days: u32, access_days: u32, ahead: i64) -> anyhow::Result<()> {
    maintain_partitions_at(db, OffsetDateTime::now_utc(), raw_days, access_days, ahead).await
}

/// 同 `maintain_partitions`，本机的「现在」由调用方给（测试模拟本机时钟大幅超前）
pub async fn maintain_partitions_at(db: &PgPool, now: OffsetDateTime, raw_days: u32, access_days: u32, ahead: i64) -> anyhow::Result<()> {
    let today = now.date();
    for t in PARTITIONED {
        // 只补建缺的分区；建分区要父表的重锁，排在长查询后面会堵住所有上报的 insert，
        // 所以限 2 秒，拿不到就记 warn、下次维护再试（上报时缺分区还会按需补建）
        let days: BTreeSet<Date> = (-1..=ahead).map(|i| today + Duration::days(i)).collect();
        let names: Vec<String> = days.iter().map(|d| part_name(t, *d)).collect();
        let existing: Vec<(String,)> = sqlx::query_as(&format!(
            "select c.relname::text from pg_class c join pg_inherits i on i.inhrelid = c.oid join pg_class p on p.oid = i.inhparent
             where p.relname = $1 and c.relname = any($2) and p.relnamespace = {OWN_SCHEMA}"
        ))
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
    }
    // 本机时钟核对：任何一张分区表最近一天里有数据（agent 每 30 秒报一次主机读数，正常时总有）就说明时钟可信。
    // 核对得上时删分区的界只看本机时钟，不用各表最新的数据
    let clock_ok = match recent_data(db, now).await {
        Ok(ok) => ok,
        Err(e) => {
            tracing::warn!("核对本机时钟（查分区表最近一天的数据）失败，这次不删过期分区：{e:#}");
            return Ok(());
        }
    };
    let mut newest: Vec<(&str, Option<OffsetDateTime>)> = Vec::with_capacity(PARTITIONED.len());
    for t in PARTITIONED {
        if clock_ok {
            newest.push((t, None));
            continue;
        }
        match newest_ts(db, t).await {
            Ok(n) => newest.push((t, n)),
            Err(e) => tracing::warn!("查 {t} 最新数据失败，这次不删过期分区：{e:#}"),
        }
    }
    let freshest = newest.iter().filter_map(|(_, n)| *n).max();
    if !clock_ok && freshest.is_some() {
        tracing::warn!(
            "分区表最近一天都没有新数据（最新在 {}；本机时钟可能大幅超前，或很久没收到上报）：过期分区按各表最新的数据减保留期删",
            crate::access::fmt_ts(freshest.unwrap_or(now))
        );
    }
    for (t, n) in newest {
        let keep = if t == "access_event" { access_days } else { raw_days } as i64;
        let Some(before) = drop_before(now, keep, n, clock_ok) else { continue };
        if let Err(e) = drop_expired(db, t, &part_name(t, before)).await {
            tracing::warn!("清理 {t} 过期分区失败，下次再试：{e:#}");
        }
    }
    Ok(())
}

/// 删分区的界：日期早于这天的分区算过期。
/// - 本机时钟核对得上（`clock_ok`：任何一张分区表最近一天里有数据——数据的时间戳来自各 agent 自己的时钟，中控只收比自己快
///   10 分钟以内的）：按本机时钟「今天 − 保留期」，某张表（如访问记录）空闲多久都照期限删；
/// - 核对不了（开机时钟大幅超前——校时之前、虚拟机恢复，或整个中控很久没收到上报）：按这张表最新一条数据减保留期，以数据为锚，
///   保留期内的数据不会因为时钟超前被整批当成过期删掉；表里没有数据时返回 None，这次不删
fn drop_before(now: OffsetDateTime, keep: i64, newest: Option<OffsetDateTime>, clock_ok: bool) -> Option<Date> {
    if clock_ok {
        return Some(now.date() - Duration::days(keep));
    }
    newest.map(|n| (n - Duration::days(keep)).date())
}

/// 有没有哪张分区表在 `now` 前一天以内（或之后）有数据。ts 只有 BRIN 索引，max(ts) 要顺序扫当天整个分区（探测结果一天约 500 万行），
/// 这里只看日期不早于前一天的分区、从日期最新的往前逐个查 exists：预建的未来分区是空的，当天分区读到第一行就满足
async fn recent_data(db: &PgPool, now: OffsetDateTime) -> anyhow::Result<bool> {
    let since = now - Duration::days(1);
    for t in PARTITIONED {
        let parts: Vec<(String,)> = sqlx::query_as(&format!(
            "select c.relname::text from pg_inherits i join pg_class c on c.oid = i.inhrelid
             join pg_class p on p.oid = i.inhparent where p.relname = $1 and c.relname >= $2 and p.relnamespace = {OWN_SCHEMA} order by 1 desc"
        ))
        .bind(t)
        .bind(part_name(t, since.date()))
        .fetch_all(db)
        .await?;
        for (n,) in parts {
            if sqlx::query_scalar(&format!("select exists (select 1 from {n} where ts > $1)")).bind(since).fetch_one(db).await? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// 表里最新一条数据的时间（没有数据为 None）：从日期最新的分区往前找第一个有数据的（预建的未来分区是空的），只在它里面取 max(ts)。
/// 只在本机时钟核对不上时用（见 `recent_data`）
async fn newest_ts(db: &PgPool, t: &str) -> anyhow::Result<Option<OffsetDateTime>> {
    let parts: Vec<(String,)> = sqlx::query_as(&format!(
        "select c.relname::text from pg_inherits i join pg_class c on c.oid = i.inhrelid
         join pg_class p on p.oid = i.inhparent where p.relname = $1 and p.relnamespace = {OWN_SCHEMA} order by 1 desc"
    ))
    .bind(t)
    .fetch_all(db)
    .await?;
    for (n,) in parts {
        let ts: Option<OffsetDateTime> = sqlx::query_scalar(&format!("select max(ts) from {n}")).fetch_one(db).await?;
        if ts.is_some() {
            return Ok(ts);
        }
    }
    Ok(None)
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
    let attached: Vec<(String,)> = sqlx::query_as(&format!(
        "select c.relname::text from pg_inherits i join pg_class c on c.oid = i.inhrelid
         join pg_class p on p.oid = i.inhparent where p.relname = $1 and c.relname < $2 and p.relnamespace = {OWN_SCHEMA} order by 1"
    ))
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
    let orphans: Vec<(String,)> = sqlx::query_as(&format!(
        "select c.relname::text from pg_class c
         where c.relkind = 'r' and not c.relispartition and c.relnamespace = {OWN_SCHEMA}
           and c.relname ~ ('^' || $1 || '_[0-9]{{8}}$') and c.relname < $2"
    ))
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
    let existing: Vec<(String,)> = sqlx::query_as(&format!(
        "select c.relname::text from pg_class c join pg_inherits i on i.inhrelid = c.oid where c.relname = any($1) and c.relnamespace = {OWN_SCHEMA}"
    ))
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
            let again: Option<(String,)> = sqlx::query_as(&format!(
                "select c.relname::text from pg_class c join pg_inherits i on i.inhrelid = c.oid where c.relname = $1 and c.relnamespace = {OWN_SCHEMA}"
            ))
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
/// 整点后最初 10 分钟内的轮次额外重算上一小时：上报在整点前判定属于当前小时（没标脏）、却在新小时第一轮读完数据后才提交，
/// 这几毫秒的竞态会漏掉它；升级那一刻上一小时也没有脏标记。多算几次是幂等的。
pub async fn rollup_dirty<F, Fut>(db: &PgPool, table: &str, roll: F) -> anyhow::Result<()>
where
    F: Fn(Vec<OffsetDateTime>) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    let now = OffsetDateTime::now_utc();
    let cur = floor_hour(now);
    let dirty: Vec<(OffsetDateTime, OffsetDateTime)> = sqlx::query_as(&format!("select hour, touched_at from {table} where hour <= $1 order by hour limit $2"))
        .bind(cur)
        .bind(DIRTY_BATCH)
        .fetch_all(db)
        .await?;
    let mut hours: BTreeSet<OffsetDateTime> = dirty.iter().map(|d| d.0).collect();
    hours.insert(cur);
    if now - cur < Duration::minutes(10) {
        hours.insert(cur - Duration::hours(1));
    }
    // 一次交给 roll（升序）：访问汇总按天分组，同一天只重算一次天汇总
    roll(hours.into_iter().collect()).await?;
    let mut tx = begin_async(db).await?;
    if !dirty.is_empty() {
        let (hs, ts): (Vec<OffsetDateTime>, Vec<OffsetDateTime>) = dirty.into_iter().unzip();
        sqlx::query(&format!("delete from {table} d using unnest($1::timestamptz[], $2::timestamptz[]) u(h, t) where d.hour = u.h and d.touched_at = u.t"))
            .bind(&hs)
            .bind(&ts)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query(&format!(
        "insert into {table} (hour, touched_at) values ($1, clock_timestamp()) on conflict (hour) do update set touched_at = excluded.touched_at"
    ))
    .bind(cur)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// 遥测小时汇总的一轮（脏小时语义见 `rollup_dirty`）。同一小时从原始数据重算并覆盖，幂等。
pub async fn rollup_hours(db: &PgPool) -> anyhow::Result<()> {
    rollup_dirty(db, "rollup_dirty", |hours| async move {
        for h in hours {
            rollup_hour(db, h).await?;
        }
        Ok(())
    })
    .await
}

/// 重算一个整点小时。主机网络流量取相邻样本的正增量之和（lag）。读数明显回到小值（不到前值的一半：旧 agent 重启清零、
/// nic.json 丢失、新计数器）时当前读数就是重置后的增量；只往回跳了一点（持久累计值在异常重启后退回上次存盘的值、文件比上报旧）
/// 记 0，不把整个累计值当增量。每台机器本小时第一个样本的前值 = 它在本小时之前的最后一个样本（不限回看多久，
/// 只受原始数据保留期限制）：agent 离线、中控停机、离线队列淘汰造成的缺口，缺口内的增量整笔记进恢复后的这一小时，不丢；
/// 晚到的样本把下一小时也标脏（agent_api），下一小时的第一个增量跟着重算。前值按唯一索引 (machine_id, ts) 倒序取一行。
/// 每段增量不超过 10 Gbit/s（1.25e9 字节/秒 × 两个样本的间隔），入口流量每小时不超过 10 Gbit/s 跑满一小时（4.5e12 字节）：
/// agent 报个离谱的计数器（0 → i64::MAX）不让汇总、用量求和溢出 bigint（这一小时所有机器的汇总都会失败）
async fn rollup_hour(db: &PgPool, hour: OffsetDateTime) -> anyhow::Result<()> {
    let q = [
        "insert into host_metric_hourly (machine_id, hour, cpu_avg, cpu_max, mem_used_avg, mem_total_max, disk_used_max, disk_total_max, net_rx, net_tx)
         select machine_id, $1, avg(cpu_pct), max(cpu_pct), avg(mem_used)::bigint, max(mem_total), max(disk_used), max(disk_total),
                coalesce(sum(least(case when rx is null or prev_rx is null then 0 when rx >= prev_rx then rx - prev_rx when rx <= prev_rx / 2 then rx else 0 end, cap)), 0)::bigint,
                coalesce(sum(least(case when tx is null or prev_tx is null then 0 when tx >= prev_tx then tx - prev_tx when tx <= prev_tx / 2 then tx else 0 end, cap)), 0)::bigint
         from (select machine_id, ts, cpu_pct, mem_used, mem_total, disk_used, disk_total,
                      net_rx_bytes rx, lag(net_rx_bytes) over w prev_rx, net_tx_bytes tx, lag(net_tx_bytes) over w prev_tx,
                      1.25e9 * extract(epoch from ts - lag(ts) over w) cap
               from (select machine_id, ts, cpu_pct, mem_used, mem_total, disk_used, disk_total, net_rx_bytes, net_tx_bytes
                     from host_metric where ts >= $1 and ts < $1 + interval '1 hour'
                     union all
                     select p.* from (select distinct machine_id from host_metric where ts >= $1 and ts < $1 + interval '1 hour') m
                     cross join lateral (select machine_id, ts, cpu_pct, mem_used, mem_total, disk_used, disk_total, net_rx_bytes, net_tx_bytes
                                         from host_metric h where h.machine_id = m.machine_id and h.ts < $1 order by h.ts desc limit 1) p) s
               window w as (partition by machine_id order by ts)) x
         where ts >= $1 group by machine_id
         on conflict (machine_id, hour) do update set cpu_avg = excluded.cpu_avg, cpu_max = excluded.cpu_max,
           mem_used_avg = excluded.mem_used_avg, mem_total_max = excluded.mem_total_max, disk_used_max = excluded.disk_used_max,
           disk_total_max = excluded.disk_total_max, net_rx = excluded.net_rx, net_tx = excluded.net_tx
         where (host_metric_hourly.cpu_avg, host_metric_hourly.cpu_max, host_metric_hourly.mem_used_avg, host_metric_hourly.mem_total_max,
                host_metric_hourly.disk_used_max, host_metric_hourly.disk_total_max, host_metric_hourly.net_rx, host_metric_hourly.net_tx)
               is distinct from (excluded.cpu_avg, excluded.cpu_max, excluded.mem_used_avg, excluded.mem_total_max,
                                 excluded.disk_used_max, excluded.disk_total_max, excluded.net_rx, excluded.net_tx)",
        "insert into entry_traffic_hourly (entry_id, hour, up_bytes, down_bytes, conns_max)
         select entry_id, $1, least(sum(up_bytes), 4.5e12)::bigint, least(sum(down_bytes), 4.5e12)::bigint, max(conns)
         from entry_traffic where ts >= $1 and ts < $1 + interval '1 hour' group by entry_id
         on conflict (entry_id, hour) do update set up_bytes = excluded.up_bytes, down_bytes = excluded.down_bytes, conns_max = excluded.conns_max
         where (entry_traffic_hourly.up_bytes, entry_traffic_hourly.down_bytes, entry_traffic_hourly.conns_max)
               is distinct from (excluded.up_bytes, excluded.down_bytes, excluded.conns_max)",
        "insert into probe_hourly (entry_id, hour, kind, total, ok, latency_avg)
         select entry_id, $1, kind, count(*), count(*) filter (where ok), avg(latency_ms) filter (where ok)
         from probe_result where ts >= $1 and ts < $1 + interval '1 hour' group by entry_id, kind
         on conflict (entry_id, hour, kind) do update set total = excluded.total, ok = excluded.ok, latency_avg = excluded.latency_avg
         where (probe_hourly.total, probe_hourly.ok, probe_hourly.latency_avg) is distinct from (excluded.total, excluded.ok, excluded.latency_avg)",
        // 客户入口连通性（中控与各探测点，见 reach.rs）：原始记录由中控收到时写、时刻就是收到的时刻，不会迟到；这一小时没有原始记录（早过了保留期）时不动已有的汇总
        "insert into relay_reach_hourly (relay_id, probe_point_id, hour, total, ok, rtt_avg, rtt_p50)
         select relay_id, probe_point_id, $1, count(*), count(*) filter (where ok),
                avg(rtt_ms) filter (where ok), percentile_cont(0.5) within group (order by rtt_ms) filter (where ok)
         from relay_reach where ts >= $1 and ts < $1 + interval '1 hour' group by relay_id, probe_point_id
         on conflict (relay_id, coalesce(probe_point_id, '00000000-0000-0000-0000-000000000000'::uuid), hour) do update
           set total = excluded.total, ok = excluded.ok, rtt_avg = excluded.rtt_avg, rtt_p50 = excluded.rtt_p50
         where (relay_reach_hourly.total, relay_reach_hourly.ok, relay_reach_hourly.rtt_avg, relay_reach_hourly.rtt_p50)
               is distinct from (excluded.total, excluded.ok, excluded.rtt_avg, excluded.rtt_p50)",
    ];
    // 四条一个事务、异步提交：汇总是从原始数据重算的，崩溃丢了下一轮会补
    let mut tx = begin_async(db).await?;
    for s in q {
        sqlx::query(s).bind(hour).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// 保留期清理（每小时一次）：已恢复超 180 天的告警、过期会话、超 1 天的登录失败记录、超 30 天没再登录成功的登录来源、
/// 超 90 天的通知记录、结束超 90 天的静默、超 365 天的审计、作废超 30 天的 agent 令牌（没有外键指向它）、超 [retention] reach_days（默认 2 天）的客户入口连通性原始记录
/// 、超 180 天的探测 / 客户入口连通性 / 软路由指标的小时汇总（读的地方最长看 90 天；entry_traffic_hourly、host_metric_hourly 回收站要算全程流量，一直留着）。访问记录（汇总、来源画像、画像异常告警）按面板里设的保留期删，
/// 见 `access::prune`；三网 PING 在 ping.rs 自己的循环里按 [retention] ping_days / ping_hourly_days 删
pub async fn cleanup_retention(db: &PgPool, r: &crate::config::Retention) -> anyhow::Result<()> {
    let reach = format!("delete from relay_reach where ts < now() - interval '{} days'", r.reach_days);
    let q = [
        ("告警", "delete from incident where state = 'resolved' and resolved_at < now() - interval '180 days'"),
        ("会话", "delete from session where expires_at < now()"),
        ("登录失败", "delete from login_failure where ts < now() - interval '1 day'"),
        ("登录来源", "delete from login_source where last_ok_at < now() - interval '30 days'"),
        ("通知记录", "delete from notify_log where ts < now() - interval '90 days'"),
        ("静默", "delete from silence where ends_at < now() - interval '90 days'"),
        ("审计", "delete from audit_log where ts < now() - interval '365 days'"),
        ("作废的 agent 令牌", "delete from agent_token where revoked_at < now() - interval '30 days'"),
        ("客户入口连通性", reach.as_str()),
        ("探测小时汇总", "delete from probe_hourly where hour < now() - interval '180 days'"),
        ("客户入口连通性小时汇总", "delete from relay_reach_hourly where hour < now() - interval '180 days'"),
        ("软路由指标小时汇总", "delete from router_metric_hourly where hour < now() - interval '180 days'"),
        ("软路由出口指标小时汇总", "delete from router_exit_metric_hourly where hour < now() - interval '180 days'"),
    ];
    for (what, sql) in q {
        let n = sqlx::query(sql).execute(db).await.with_context(|| format!("清理{what}"))?.rows_affected();
        if n > 0 {
            tracing::info!("保留期清理：{what} {n} 条");
        }
    }
    Ok(())
}

/// 彻底删除前等多少天：入口的小时流量最长被流量页回看 `traffic::MAX_DAYS`（92）天（月报、可用率最长 90 天、小时探测留 180 天，都更短），
/// 少于这个天数就删，回看窗口里另一端那台机器的流量页 / 月报数字会变小。多一天是 purge_blocker 的整点和日界余量
pub const PURGE_WAIT_DAYS: i32 = crate::traffic::MAX_DAYS as i32 + 1;

/// 彻底删除会连带删掉这台机器入口的流量（入口另一端的机器拿这些流量算今日/本月/周期已用）和它自己的网卡小时数据（线路机的流量页、月报用），
/// 流量页、月报还能回看到 `PURGE_WAIT_DAYS` 天前：近 `PURGE_WAIT_DAYS` 天里还有这两类数据时不能删（只看有流量的小时行：0 字节的行删了也不改任何统计）。
/// 另一端是不是已退役、入口是不是线路机直出的都不例外：等待有 `PURGE_WAIT_DAYS` 天的时限，互为对端的两台也不会谁都删不掉。
/// 宿主机上还有同机 IP 节点（含已退役的，它们的 host_id 指着它）时也不能删，先删它们。
/// 没做完的改软路由出口作业（`routers_applied` 不空）引用的新入口在这台机器上（它是入口的节点或线路机）时也不能删：作业改回要用那条入口。
/// 返回给用户看的原因；None = 可以删。
pub async fn purge_blocker(db: &PgPool, machine: Uuid) -> sqlx::Result<Option<String>> {
    let ip_nodes: Option<String> = sqlx::query_scalar(
        "select string_agg(display_name(m.kind::text, m.label, t.name, m.number), '、' order by m.id)
         from machine m left join team t on t.id = m.team_id where m.host_id = $1",
    )
    .bind(machine)
    .fetch_one(db)
    .await?;
    if let Some(names) = ip_nodes {
        return Ok(Some(format!("这台机器上还有同机 IP 节点没有彻底删除（含已退役的）：{names}；先删它们")));
    }
    // 没做完的（失败 / 需人工处理的、在重试的）改软路由出口作业还要照记着的新入口改回：新入口所在的节点和线路机先留着
    let pending: Option<Uuid> = sqlx::query_scalar(
        "select j.id from job j,
             jsonb_array_elements(case when jsonb_typeof(j.params->'routers_applied') = 'array' then j.params->'routers_applied' else '[]'::jsonb end) a,
             jsonb_array_elements(case when jsonb_typeof(a->'exits') = 'array' then a->'exits' else '[]'::jsonb end) x,
             entry e
         where j.state not in ('succeeded', 'cancelled') and e.id::text = x->>'new_entry_id' and (e.node_id = $1 or e.relay_id = $1) limit 1",
    )
    .bind(machine)
    .fetch_optional(db)
    .await?;
    if pending.is_some() {
        return Ok(Some("还有没做完的改软路由出口作业（失败或需要人工处理）要照这台机器上的入口改回，处理完那个作业再删".into()));
    }
    let until: Option<Date> = sqlx::query_scalar(
        "select ((max(h.hour) + make_interval(days => $2, hours => 1)) at time zone org_tz())::date + 1
         from (select h.hour from entry e join entry_traffic_hourly h on h.entry_id = e.id
               where (e.node_id = $1 or e.relay_id = $1) and h.hour >= now() - make_interval(days => $2) and h.up_bytes + h.down_bytes > 0
               union all
               select hour from host_metric_hourly
               where machine_id = $1 and hour >= now() - make_interval(days => $2) and coalesce(net_rx, 0) + coalesce(net_tx, 0) > 0) h",
    )
    .bind(machine)
    .bind(PURGE_WAIT_DAYS)
    .fetch_one(db)
    .await?;
    Ok(until.map(|d| format!("这台机器的流量数据（入口业务流量、网卡）在 {d} 前仍能在流量页、月报里回看，届时再彻底删除")))
}

/// 同机 IP 节点的 SS 端口从这里往上分（宿主机自己的 ssserver 固定 meta::SS_PORT）
const IP_NODE_PORT_MIN: u16 = crate::meta::SS_PORT + 1;

/// 宿主机上未退役的同机 IP 节点占着的 SS 端口（`except` 这台不算）。调用方先锁住宿主机行，同一台宿主机上的端口一次只有一个事务在分
pub async fn ip_node_ports(conn: &mut sqlx::PgConnection, host: Uuid, except: Option<Uuid>) -> sqlx::Result<HashSet<u16>> {
    let ports: Vec<i32> = sqlx::query_scalar(
        "select o.port from node_outbound o join machine x on x.id = o.machine_id
         where x.host_id = $1 and x.lifecycle <> 'retired' and x.id is distinct from $2",
    )
    .bind(host)
    .bind(except)
    .fetch_all(conn)
    .await?;
    Ok(ports.into_iter().map(|p| p as u16).collect())
}

/// 不在 `taken` 里的最小 SS 端口
pub fn free_ss_port(taken: &HashSet<u16>) -> Option<u16> { (IP_NODE_PORT_MIN..=u16::MAX).find(|p| !taken.contains(p)) }

pub async fn audit(tx: &mut Tx<'_>, actor: &str, action: &str, target_kind: &str, target_id: Option<Uuid>, detail: serde_json::Value) -> sqlx::Result<()> {
    audit_insert(&mut **tx, actor, action, target_kind, target_id, detail).await.map(|_| ())
}

/// 写一条审计，返回 id。search 列一并写上（界面上看得到的标题与摘要，见 api::audit::audit_search_text）：操作记录的关键词搜索只搜它
pub async fn audit_insert<'e, E: sqlx::PgExecutor<'e>>(
    ex: E,
    actor: &str,
    action: &str,
    target_kind: &str,
    target_id: Option<Uuid>,
    detail: serde_json::Value,
) -> sqlx::Result<i64> {
    let search = crate::api::audit::audit_search_text(action, &detail);
    sqlx::query_scalar("insert into audit_log (actor, action, target_kind, target_id, detail, search) values ($1, $2, $3, $4, $5, $6) returning id")
        .bind(actor)
        .bind(action)
        .bind(target_kind)
        .bind(target_id)
        .bind(detail)
        .bind(search)
        .fetch_one(ex)
        .await
}

/// 给先记下「已开始」的审计补写结果（执行命令、一键部署软路由）：去掉 started、并进 `patch`，搜索文本按新内容重算
pub async fn audit_finish(db: &PgPool, id: i64, patch: serde_json::Value) -> sqlx::Result<()> {
    let mut tx = db.begin().await?;
    let (action, detail): (String, serde_json::Value) =
        sqlx::query_as("update audit_log set detail = (detail - 'started'::text) || $2 where id = $1 returning action, detail")
            .bind(id)
            .bind(patch)
            .fetch_one(&mut *tx)
            .await?;
    sqlx::query("update audit_log set search = $2 where id = $1").bind(id).bind(crate::api::audit::audit_search_text(&action, &detail)).execute(&mut *tx).await?;
    tx.commit().await
}

/// 线路机期望状态 +1，返回新代数。调用方在提交后调用 `AppState::relay_bumped`。
pub async fn bump_relay(tx: &mut Tx<'_>, relay_id: Uuid) -> sqlx::Result<i64> {
    let (g,): (i64,) = sqlx::query_as("update relay set generation = generation + 1, generation_at = now() where machine_id = $1 returning generation")
        .bind(relay_id)
        .fetch_one(&mut **tx)
        .await?;
    Ok(g)
}

/// 期望状态的格式版本：`entries::desired_state` 生成的内容变了（加字段、改算法、改默认值），而库里的数据没变时，**手动 +1**。
/// 线路机只在代数变了时才重拉期望状态，中控升级后数据没变就一直用旧格式的那份；启动时和库里记的比（`sync_desired_format`），
/// 不同就给所有未退役线路机代数 +1
pub const DESIRED_FORMAT: i64 = 2;
/// 库里记 `DESIRED_FORMAT` 的 app_setting 键
const DESIRED_FORMAT_KEY: &str = "desired_format";

/// 中控启动时（迁移之后、开始处理请求之前）调：库里记的期望状态格式版本和 `DESIRED_FORMAT` 不同（含从没记过）就给所有未退役线路机
/// 代数 +1 并记下新值，返回改了几台；相同什么都不做。没启动时没有长轮询在等，不用唤醒，线路机重连后看到代数变了自己来拉
pub async fn sync_desired_format(db: &PgPool) -> sqlx::Result<u64> {
    let mut tx = db.begin().await?;
    let old: Option<i64> =
        sqlx::query_scalar("select (value #>> '{}')::bigint from app_setting where key = $1 for update").bind(DESIRED_FORMAT_KEY).fetch_optional(&mut *tx).await?;
    if old == Some(DESIRED_FORMAT) {
        return Ok(0);
    }
    let n = sqlx::query(
        "update relay r set generation = generation + 1, generation_at = now() from machine m where m.id = r.machine_id and m.lifecycle <> 'retired'",
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    sqlx::query(
        "insert into app_setting (key, value, updated_by) values ($1, to_jsonb($2::bigint), 'fleet-server')
         on conflict (key) do update set value = excluded.value, updated_at = now(), updated_by = excluded.updated_by",
    )
    .bind(DESIRED_FORMAT_KEY)
    .bind(DESIRED_FORMAT)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(n)
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

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::{date, datetime};

    /// 删分区的界按本机时钟算，但不晚于最新数据减保留期；没有数据、最新数据比现在早一天以上都不删
    #[test]
    fn drop_before_checks_the_clock_against_the_data() {
        let now = datetime!(2026-10-22 10:00 UTC);
        // 时钟核对得上（别的表最近一天里有数据）：按本机时钟切，这张表空闲多久、甚至是空的都照期限删
        assert_eq!(drop_before(now, 3, Some(now - Duration::seconds(5)), true), Some(date!(2026 - 10 - 19)));
        assert_eq!(drop_before(now, 3, Some(now - Duration::days(80)), true), Some(date!(2026 - 10 - 19)));
        assert_eq!(drop_before(now, 3, None, true), Some(date!(2026 - 10 - 19)));
        // 核对不了（本机时钟大幅超前，或整个中控很久没收到上报）：按这张表最新的数据减保留期切，表空不删
        assert_eq!(drop_before(now + Duration::days(400), 3, Some(now), false), Some(date!(2026 - 10 - 19)));
        assert_eq!(drop_before(now, 3, Some(now - Duration::days(80)), false), Some(date!(2026 - 07 - 31)));
        assert_eq!(drop_before(now, 3, None, false), None);
    }
}
