//! 中控 → 线路机的连通性（线路机卡片的心跳格，/probe-beats）。
//!
//! 每 30 秒对每台在用、有在用入口的线路机做一次 TCP 建连：目标是客户连的地址（relay.public_host）+ 它的在用入口端口，
//! 每轮换下一个（N 条入口 N 轮连一遍）——线路机在不在线、客户连的每个口连不连得上（防火墙、供应商封口、单个入站没起来）。
//! 只建连，不做 REALITY 握手。只记通不通，不记延迟：中控在德国，到美国线路机的往返时间不代表客户体验。
//! 同一台线路机最近 3 轮都连不上（整台不通），或同一个端口最近 3 次都连不上（单个口坏了），健康判定报 relay_unreachable
//! （views 的 `RelayInfo::unreachable`；结论变了就发 `Event::Machine`）。
//! 不连 SSH 口：每 30 秒一次未认证就断开的连接会刷 sshd 日志，线路机上若有 fail2ban 还可能把中控封掉。
//! Xray 的 REALITY 入站对只建连、不发数据的连接不记访问日志。
//!
//! 中控自己的网络有问题（刚重启网络没好、出口断了）时不能记成线路机故障，整轮作废、不写：
//! - 对照目标（公共 DNS 的 443 口）一个都连不上；
//! - 或三台以上线路机全部连不上（不会每台都同时坏）。对照目标通时两台同时连不上照记：
//!   生产两台线路机在同一家供应商的同一个 /24，一起出问题并不罕见，正是要报的时候。
//!
//! 结果进 relay_reach，保留 2 天（db::cleanup_retention）。

use crate::state::{AppState, Event};
use std::time::Duration;
use time::OffsetDateTime;
use tokio::net::TcpStream;
use uuid::Uuid;

const EVERY: Duration = Duration::from_secs(30);
/// 同 agent 的 TCP 探测：盖住 SYN 的第三次重发（第 7 秒）
const TIMEOUT: Duration = Duration::from_secs(10);
/// 中控出口的对照目标：任一连上就算中控自己的网络是好的
const CONTROLS: &[&str] = &["1.1.1.1:443", "8.8.8.8:443"];
/// 连续这么多次连不上算「中控连不上」（告警的 `unreachable_sql`、视图入口的 reach.fails 与网页同一口径）
pub const FAILS: i32 = 3;

pub async fn run_forever(st: AppState) {
    // 刚启动（含 systemd 刚拉起）先等一会儿：网络可能还没就绪，第一轮的失败不算数
    tokio::time::sleep(Duration::from_secs(15)).await;
    let mut t = tokio::time::interval(EVERY);
    t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut turn = 0usize;
    loop {
        t.tick().await;
        if let Err(e) = round(&st, CONTROLS, turn).await {
            tracing::warn!("中控 → 线路机连通性：{e:#}");
        }
        turn = turn.wrapping_add(1);
    }
}

/// 线路机 `relay`（SQL 表达式）连不上时的失败原因（去重，带端口），否则 null：
/// - 整台不通：最近 3 轮（5 分钟内）都连不上，取这 3 次的原因；
/// - 单个口坏了：某个在用入口端口最近 3 次都连不上（轮流连，N 个口每 N 轮才轮到一次：看 30 分钟与 3 遍轮换里取长的），取它最近一次的原因。
///
/// 视图的 `RelayInfo::unreachable` 与本轮前后的比对共用
pub(crate) fn unreachable_sql(relay: &str) -> String {
    format!(
        "(with x as (select rr.port, rr.ok, coalesce(rr.detail, '-') detail, rr.ts > now() - interval '5 minutes' fresh,
                            row_number() over (order by rr.ts desc) n, row_number() over (partition by rr.port order by rr.ts desc) np
                     from relay_reach rr where rr.relay_id = {relay} and rr.ts > now() - greatest(interval '30 minutes',
                       interval '90 seconds' * (select count(*) from entry e where e.relay_id = {relay} and e.lifecycle = 'active'))),
               whole as (select count(*) = 3 and not bool_or(ok) down from x where n <= 3 and fresh),
               bad as (select port from x where np <= 3 and port in (select e.port from entry e where e.relay_id = {relay} and e.lifecycle = 'active')
                       group by port having count(*) = 3 and not bool_or(ok))
          select string_agg(distinct detail, '；' order by detail) from x
          where (n <= 3 and fresh and (select down from whole)) or (np = 1 and port in (select port from bad)))"
    )
}

/// 线路机 `relay` 整台连不连得上判不出：有在用入口，5 分钟内的检查却不到 3 轮（中控刚重启、整轮作废）。
/// 视图的 `RelayInfo::reach_unknown`：这时 relay_unreachable 的原因消失不算恢复（中控停机超过 5 分钟后，新记录攒满 3 轮之前）
pub(crate) fn unknown_sql(relay: &str) -> String {
    format!(
        "(exists (select 1 from entry e where e.relay_id = {relay} and e.lifecycle = 'active')
          and (select count(*) from relay_reach rr where rr.relay_id = {relay} and rr.ts > now() - interval '5 minutes') < 3)"
    )
}

/// 一轮：每台线路机连它第 `turn`（对入口数取余）个在用入口端口，返回写入的行数（整轮作废为 0）。
/// 写入后连不上的结论（`unreachable_sql`）或判不判得出（`unknown_sql`）变了的线路机发 `Event::Machine`：视图不用等完整重算
pub async fn round(st: &AppState, controls: &[&str], turn: usize) -> anyhow::Result<usize> {
    let db = &st.db;
    let targets: Vec<(Uuid, String, Vec<i32>)> = sqlx::query_as(
        "select m.id, r.public_host, array_agg(e.port order by e.port) from machine m join relay r on r.machine_id = m.id
         join entry e on e.relay_id = m.id and e.lifecycle = 'active'
         where m.kind = 'relay' and m.lifecycle = 'active' group by m.id, r.public_host",
    )
    .fetch_all(db)
    .await?;
    if targets.is_empty() {
        return Ok(0);
    }
    let ts = OffsetDateTime::now_utc();
    let ports: Vec<u16> = targets.iter().map(|(_, _, ps)| ps[turn % ps.len()] as u16).collect();
    let relays = futures::future::join_all(targets.iter().zip(&ports).map(|((_, host, _), port)| connect(host.clone(), *port)));
    let ctl = futures::future::join_all(controls.iter().map(|a| connect_addr(a)));
    let (res, ctl) = tokio::join!(relays, ctl);
    let control_ok = controls.is_empty() || ctl.iter().any(|r| r.is_ok());
    let oks: Vec<bool> = res.iter().map(|r| r.is_ok()).collect();
    if !keep_round(&oks, control_ok) {
        tracing::warn!(control_ok, relays = oks.len(), "中控 → 线路机：中控自己的网络像是有问题，这一轮不记");
        return Ok(0);
    }
    // 失败原因带上端口：轮流连的，告警摘要里要看得出是哪个口
    let details: Vec<Option<String>> = res.iter().zip(&ports).map(|(r, port)| r.as_ref().err().map(|e| format!("端口 {port}：{e}"))).collect();
    let ids: Vec<Uuid> = targets.iter().map(|t| t.0).collect();
    let unreach = format!("select {}, {} from unnest($1::uuid[]) with ordinality u(id, i) order by i", unreachable_sql("u.id"), unknown_sql("u.id"));
    let before: Vec<(Option<String>, bool)> = sqlx::query_as(&unreach).bind(&ids).fetch_all(db).await?;
    sqlx::query(
        "insert into relay_reach (relay_id, ts, ok, detail, port) select u.id, $2, u.ok, u.detail, u.port from unnest($1::uuid[], $3::bool[], $4::text[], $5::int[]) u(id, ok, detail, port)",
    )
    .bind(&ids)
    .bind(ts)
    .bind(&oks)
    .bind(&details)
    .bind(ports.iter().map(|&p| i32::from(p)).collect::<Vec<_>>())
    .execute(db)
    .await?;
    let after: Vec<(Option<String>, bool)> = sqlx::query_as(&unreach).bind(&ids).fetch_all(db).await?;
    for ((id, b), a) in ids.iter().zip(&before).zip(&after) {
        if b != a {
            st.emit(Event::Machine { id: *id });
        }
    }
    Ok(targets.len())
}

/// 这一轮记不记：对照目标都连不上，或三台以上线路机全部失败 → 是中控自己的问题，不记
pub fn keep_round(oks: &[bool], control_ok: bool) -> bool {
    control_ok && (oks.len() < 3 || oks.iter().any(|&o| o))
}

async fn connect(host: String, port: u16) -> Result<(), String> {
    match tokio::time::timeout(TIMEOUT, TcpStream::connect((host.as_str(), port))).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(reason(&e)),
        Err(_) => Err(format!("超时 {} 秒", TIMEOUT.as_secs())),
    }
}

async fn connect_addr(a: &str) -> Result<(), String> {
    match a.rsplit_once(':').and_then(|(h, p)| Some((h.to_string(), p.parse().ok()?))) {
        Some((h, p)) => connect(h, p).await,
        None => Err("地址无效".into()),
    }
}

fn reason(e: &std::io::Error) -> String {
    use std::io::ErrorKind as K;
    match e.kind() {
        K::ConnectionRefused => "连接被拒绝".into(),
        K::ConnectionReset => "连接被重置".into(),
        K::TimedOut => format!("超时 {} 秒", TIMEOUT.as_secs()),
        _ if e.to_string().contains("unreachable") => "网络不可达".into(),
        _ if e.to_string().contains("lookup") || e.to_string().contains("resolve") => "无法解析主机".into(),
        _ => format!("其他：{}", e.to_string().chars().take(80).collect::<String>()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn central_side_failures_discard_the_round() {
        assert!(keep_round(&[true, false], true), "一台不通照记");
        assert!(keep_round(&[false], true), "只有一台线路机：靠对照目标判断");
        assert!(keep_round(&[false, false], true), "两台全不通照记：同一家供应商一起出问题");
        assert!(!keep_round(&[false, false, false], true), "三台以上全不通：中控自己的问题");
        assert!(!keep_round(&[true, true], false), "对照目标都连不上：不记");
        assert!(keep_round(&[], true));
    }

    #[tokio::test]
    async fn connect_reports_refused_in_chinese() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let p = l.local_addr().unwrap().port();
        assert_eq!(connect("127.0.0.1".into(), p).await, Ok(()));
        drop(l);
        assert_eq!(connect("127.0.0.1".into(), p).await, Err("连接被拒绝".into()));
        assert_eq!(connect_addr("nonsense").await, Err("地址无效".into()));
    }
}
