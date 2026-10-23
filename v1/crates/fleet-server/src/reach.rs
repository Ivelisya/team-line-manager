//! 线路机客户入口的连通性（线路机卡片的心跳格 /probe-beats、入口表的「连不上」、relay_unreachable 告警）：中控自己的检查，与探测点的检查共用的判定。
//!
//! 测的是客户连的地址（relay.public_host）+ 在用入口端口，每 30 秒一轮，每轮换下一个端口（N 条入口 N 轮连一遍）——
//! 线路机在不在线、客户连的每个口连不连得上（防火墙、供应商封口、单个入站没起来）。只建连，不做 REALITY 握手。
//! 结果进 relay_reach，每行记下是谁测的（probe_point_id：null = 中控，否则是探测点，见 probe_points/）。
//!
//! 两路来源：
//! - 中控（本文件的 `run_forever`）：在德国，到美国线路机的往返时间不代表客户体验，只记通不通（rtt_ms 为 null）。设置里可以关掉
//!   （`reach_source.central`，probe_points::Setting），关了就不再测；
//! - 探测点（`fleet-agent probe`，probe_points/agent.rs 收）：放在靠近客户的地方（如香港），同样的轮换与作废规则，另记建连的往返时间。
//!
//! 判定只看「主探测来源」那一路（probe_points::Effective，设置里选；主来源的探测点离线时退回中控或标灰，见 probe_points/ 的头注释）：
//! 同一台线路机最近 3 轮都连不上（整台不通），或同一个端口最近 3 次都连不上（单个口坏了），健康判定报 relay_unreachable
//! （views 的 `RelayInfo::unreachable`；结论变了就发 `Event::Machine`），摘要写明是哪一路测的（「香港探测点连续 3 次连不上客户入口（…）」）。
//! 不连 SSH 口：每 30 秒一次未认证就断开的连接会刷 sshd 日志，线路机上若有 fail2ban 还可能把中控封掉。
//! Xray 的 REALITY 入站对只建连、不发数据的连接不记访问日志。
//!
//! 测的这一方自己的网络有问题（刚重启网络没好、出口断了）时不能记成线路机故障：对照目标（公共 DNS 的 443 口）一个都连不上，整轮作废、不写（两路同一条规则）。
//! 对照目标通、三台以上线路机却全部连不上（`fleet_proto::all_down`）照记，行上标 all_down：以前当成测的这一方的问题整轮作废，
//! 但生产几台线路机在同一家供应商的同一个 /24，供应商整段故障时就是这样，正是要报的时候；relay_unreachable 的摘要写明
//! 「全部线路机同时连不上（可能是供应商整段故障）」，看的人一眼知道该先查供应商还是测的这一方。
//!
//! 结果进 relay_reach，保留 [retention] reach_days（默认 2 天，db::cleanup_retention）；每小时汇总进 relay_reach_hourly（db::rollup_hour，一直留着），
//! 探测点页与线路机抽屉的 24 小时 / 7 天 / 30 天成功率读它（[`quality`]）。查询都按来源过滤，索引带着来源（迁移 0046）。

use crate::probe_points::Effective;
use crate::state::{AppState, Event};
use std::time::Duration;
use time::OffsetDateTime;
use tokio::net::TcpStream;
use uuid::Uuid;

const EVERY: Duration = Duration::from_secs(30);
/// 同 agent 的 TCP 探测：盖住 SYN 的第三次重发（第 7 秒）
const TIMEOUT: Duration = Duration::from_secs(10);
/// 对照目标：任一连上就算测的这一方自己的网络是好的（中控在德国、探测点在香港，访问这两个都没问题）
pub const CONTROLS: &[&str] = &["1.1.1.1:443", "8.8.8.8:443"];
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
        // 设置里关掉了中控的检查：这一轮不测（每轮读一次，改了不用重启）
        match crate::probe_points::setting(&st.db).await {
            Ok(s) if !s.central => continue,
            Ok(_) => {}
            Err(e) => tracing::warn!("读主探测来源失败，照常检查：{e:#}"),
        }
        if let Err(e) = round(&st, CONTROLS, turn).await {
            tracing::warn!("中控 → 线路机连通性：{e:#}");
        }
        turn = turn.wrapping_add(1);
    }
}

/// 线路机 `relay`（SQL 表达式）按 `src` 那一路（主探测来源）连不上时的告警摘要「香港探测点连续 3 次连不上客户入口（端口 443：超时 10 秒）」，否则 null。
/// 括号里是失败原因（去重，带端口）：
/// - 整台不通：最近 3 轮（5 分钟内）都连不上，取这 3 次的原因；
/// - 单个口坏了：某个在用入口端口最近 3 次都连不上（轮流连，N 个口每 N 轮才轮到一次：看 30 分钟与 3 遍轮换里取长的），取它最近一次的原因。
///
/// 用到的检查里有全部线路机同时连不上的轮次（all_down）时，后面加「；全部线路机同时连不上（可能是供应商整段故障）」。
///
/// 视图的 `RelayInfo::unreachable` 与写入前后的比对共用
pub(crate) fn unreachable_sql(relay: &str, src: &Effective) -> String {
    if src.state == "none" {
        return "null::text".into();
    }
    format!(
        "(with x as (select rr.port, rr.ok, coalesce(rr.detail, '-') detail, rr.all_down, rr.ts > now() - interval '5 minutes' fresh,
                            row_number() over (order by rr.ts desc) n, row_number() over (partition by rr.port order by rr.ts desc) np
                     from relay_reach rr where rr.relay_id = {relay} and {cond} and rr.ts > now() - greatest(interval '30 minutes',
                       interval '90 seconds' * (select count(*) from entry e where e.relay_id = {relay} and e.lifecycle = 'active'))),
               whole as (select count(*) = 3 and not bool_or(ok) down from x where n <= 3 and fresh),
               bad as (select port from x where np <= 3 and port in (select e.port from entry e where e.relay_id = {relay} and e.lifecycle = 'active')
                       group by port having count(*) = 3 and not bool_or(ok))
          select {label} || '连续 3 次连不上客户入口（' || string_agg(distinct detail, '；' order by detail) || '）'
                 || case when bool_or(all_down) then '；{ALL_DOWN}' else '' end from x
          where (n <= 3 and fresh and (select down from whole)) or (np = 1 and port in (select port from bad)))",
        cond = src.cond("rr"),
        label = src.label_sql()
    )
}

/// relay_unreachable 摘要里全部线路机同时连不上时的说法
pub const ALL_DOWN: &str = "所有线路机同时连不上，可能是供应商整体故障";

/// 线路机 `relay` 整台连不连得上判不出：有在用入口，5 分钟内 `src` 那一路的检查却不到 3 轮（中控 / 探测点刚重启、整轮作废），
/// 或此刻没有可用的来源（标灰）、暂由中控代替离线的探测点（中控连得上不能当成探测点那边恢复了）。
/// 视图的 `RelayInfo::reach_unknown`：这时 relay_unreachable 的原因消失不算恢复
pub(crate) fn unknown_sql(relay: &str, src: &Effective) -> String {
    let active = format!("exists (select 1 from entry e where e.relay_id = {relay} and e.lifecycle = 'active')");
    if src.state == "none" || src.fallback() {
        return format!("({active})");
    }
    format!(
        "({active} and (select count(*) from relay_reach rr where rr.relay_id = {relay} and {cond} and rr.ts > now() - interval '5 minutes') < 3)",
        cond = src.cond("rr")
    )
}

/// 要测的线路机：在用的、有在用入口的，(id, 客户连的地址, 在用入口端口升序)。中控与探测点（取目标的接口）共用
pub(crate) async fn targets(db: &sqlx::PgPool) -> sqlx::Result<Vec<(Uuid, String, Vec<i32>)>> {
    sqlx::query_as(
        "select m.id, r.public_host, array_agg(e.port order by e.port) from machine m join relay r on r.machine_id = m.id
         join entry e on e.relay_id = m.id and e.lifecycle = 'active'
         where m.kind = 'relay' and m.lifecycle = 'active' group by m.id, r.public_host order by m.id",
    )
    .fetch_all(db)
    .await
}

/// 一次检查（写进 relay_reach 的一行）
pub(crate) struct Sample {
    pub relay: Uuid,
    pub port: i32,
    pub ok: bool,
    pub detail: Option<String>,
    pub rtt_ms: Option<f32>,
}

/// 写一轮检查（`src`：None = 中控，否则是探测点；`all_down`：这一轮三台以上线路机全部连不上，见 `fleet_proto::all_down`）。这一路正是此刻用来判断的那一路时，
/// 写入后连不上的结论（`unreachable_sql`）或判不判得出（`unknown_sql`）变了的线路机发 `Event::Machine`：视图不用等完整重算
pub(crate) async fn record(st: &AppState, src: Option<Uuid>, ts: OffsetDateTime, samples: &[Sample], all_down: bool) -> anyhow::Result<()> {
    let db = &st.db;
    let eff = crate::probe_points::effective(st).await?;
    let ids: Vec<Uuid> = samples.iter().map(|s| s.relay).collect();
    let judge = eff.uses(src) && !ids.is_empty();
    let unreach = format!("select {}, {} from unnest($1::uuid[]) with ordinality u(id, i) order by i", unreachable_sql("u.id", &eff), unknown_sql("u.id", &eff));
    let before: Vec<(Option<String>, bool)> = if judge { sqlx::query_as(&unreach).bind(&ids).fetch_all(db).await? } else { vec![] };
    sqlx::query(
        "insert into relay_reach (relay_id, ts, ok, detail, port, probe_point_id, rtt_ms, all_down)
         select u.id, $2, u.ok, u.detail, u.port, $6, u.rtt, $8 from unnest($1::uuid[], $3::bool[], $4::text[], $5::int[], $7::real[]) u(id, ok, detail, port, rtt)",
    )
    .bind(&ids)
    .bind(ts)
    .bind(samples.iter().map(|s| s.ok).collect::<Vec<_>>())
    .bind(samples.iter().map(|s| s.detail.clone()).collect::<Vec<_>>())
    .bind(samples.iter().map(|s| s.port).collect::<Vec<_>>())
    .bind(src)
    .bind(samples.iter().map(|s| s.rtt_ms).collect::<Vec<_>>())
    .bind(all_down)
    .execute(db)
    .await?;
    if judge {
        let after: Vec<(Option<String>, bool)> = sqlx::query_as(&unreach).bind(&ids).fetch_all(db).await?;
        for ((id, b), a) in ids.iter().zip(&before).zip(&after) {
            if b != a {
                st.emit(Event::Machine { id: *id });
            }
        }
    }
    Ok(())
}

/// 中控的一轮：每台线路机连它第 `turn`（对入口数取余）个在用入口端口，返回写入的行数（对照目标都连不上、整轮作废为 0）。写入与事件见 `record`
pub async fn round(st: &AppState, controls: &[&str], turn: usize) -> anyhow::Result<usize> {
    let targets = targets(&st.db).await?;
    if targets.is_empty() {
        return Ok(0);
    }
    let ts = OffsetDateTime::now_utc();
    let ports: Vec<u16> = targets.iter().map(|(_, _, ps)| ps[turn % ps.len()] as u16).collect();
    let relays = futures::future::join_all(targets.iter().zip(&ports).map(|((_, host, _), port)| connect(host.clone(), *port)));
    let ctl = futures::future::join_all(controls.iter().map(|a| connect_addr(a)));
    let (res, ctl) = tokio::join!(relays, ctl);
    if !(controls.is_empty() || ctl.iter().any(|r| r.is_ok())) {
        tracing::warn!("中控 → 线路机：对照目标都连不上，中控自己的网络像是有问题，这一轮不记");
        return Ok(0);
    }
    let oks: Vec<bool> = res.iter().map(|r| r.is_ok()).collect();
    // 失败原因带上端口：轮流连的，告警摘要里要看得出是哪个口
    let samples: Vec<Sample> = targets
        .iter()
        .zip(&ports)
        .zip(&res)
        .map(|(((id, _, _), port), r)| Sample { relay: *id, port: i32::from(*port), ok: r.is_ok(), detail: r.as_ref().err().map(|e| format!("端口 {port}：{e}")), rtt_ms: None })
        .collect();
    record(st, None, ts, &samples, fleet_proto::all_down(&oks)).await?;
    Ok(targets.len())
}

/// 成功率统计的时间窗：1h 读原始记录，其余读小时汇总（按整点，含当前这一小时）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    H1,
    H24,
    D7,
    D30,
}

impl Window {
    pub fn parse(s: Option<&str>) -> Option<Self> {
        Some(match s.unwrap_or("1h") {
            "1h" => Self::H1,
            "24h" => Self::H24,
            "7d" => Self::D7,
            "30d" => Self::D30,
            _ => return None,
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::H1 => "1h",
            Self::H24 => "24h",
            Self::D7 => "7d",
            Self::D30 => "30d",
        }
    }
    fn hours(self) -> i32 {
        match self {
            Self::H1 => 1,
            Self::H24 => 24,
            Self::D7 => 24 * 7,
            Self::D30 => 24 * 30,
        }
    }
}

/// 一路来源到一台线路机在一个时间窗里的检查：次数、通的次数、通的那些的建连往返时间（中控那一路不记，为 null）
#[derive(Debug, sqlx::FromRow)]
pub struct Quality {
    /// None = 中控
    pub probe_point_id: Option<Uuid>,
    pub relay_id: Uuid,
    pub relay_name: String,
    pub total: i64,
    pub ok: i64,
    pub avg_ms: Option<f64>,
    /// 1h 是真正的中位数；小时汇总的窗口是各小时中位数的中位数（近似）
    pub median_ms: Option<f64>,
}

/// 各路来源到各线路机（`relay` 给了就只算这一台）在 `w` 里的检查统计，按线路机名排。1h 读原始记录（relay_reach），
/// 24h / 7d / 30d 读小时汇总（relay_reach_hourly：原始记录只留几天）。平均延迟按各小时通的次数加权
pub async fn quality(db: &sqlx::PgPool, w: Window, relay: Option<Uuid>) -> sqlx::Result<Vec<Quality>> {
    let sql = if w == Window::H1 {
        "select r.probe_point_id, r.relay_id, display_name(m.kind::text, m.label, t.name, m.number) relay_name, count(*) total, count(*) filter (where r.ok) ok,
                (avg(r.rtt_ms) filter (where r.ok))::float8 avg_ms, (percentile_cont(0.5) within group (order by r.rtt_ms) filter (where r.ok))::float8 median_ms
         from relay_reach r join machine m on m.id = r.relay_id left join team t on t.id = m.team_id
         where r.ts > now() - make_interval(hours => $1) and ($2::uuid is null or r.relay_id = $2)
         group by 1, 2, 3 order by 3, 1 nulls first"
    } else {
        "select h.probe_point_id, h.relay_id, display_name(m.kind::text, m.label, t.name, m.number) relay_name, sum(h.total)::int8 total, sum(h.ok)::int8 ok,
                (sum(h.rtt_avg * h.ok) filter (where h.rtt_avg is not null) / nullif(sum(h.ok) filter (where h.rtt_avg is not null), 0))::float8 avg_ms,
                (percentile_cont(0.5) within group (order by h.rtt_p50))::float8 median_ms
         from relay_reach_hourly h join machine m on m.id = h.relay_id left join team t on t.id = m.team_id
         where h.hour > date_trunc('hour', now()) - make_interval(hours => $1) and ($2::uuid is null or h.relay_id = $2)
         group by 1, 2, 3 order by 3, 1 nulls first"
    };
    sqlx::query_as(sql).bind(w.hours()).bind(relay).fetch_all(db).await
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

/// 测试用：本机一个连上去必定被拒绝的端口。返回的套接字只绑定不监听，测试期间一直拿着：
/// 端口不会被同一进程里并行的测试 bind 到 :0 分走（先监听再关掉的写法有竞态——关掉之后别的测试马上拿到同一个端口，
/// 连上去就成了「连通」或「连接被重置」），也不会被选作出站连接的源端口（自连）。不设 SO_REUSEADDR，别人显式绑也绑不上
#[cfg(test)]
pub(crate) fn refused_port() -> (tokio::net::TcpSocket, u16) {
    let s = tokio::net::TcpSocket::new_v4().unwrap();
    s.bind(([127, 0, 0, 1], 0).into()).unwrap();
    let p = s.local_addr().unwrap().port();
    (s, p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows() {
        assert_eq!(Window::parse(None), Some(Window::H1));
        assert_eq!(Window::parse(Some("7d")).map(Window::as_str), Some("7d"));
        assert_eq!(Window::parse(Some("2d")), None);
        assert_eq!(Window::D30.hours(), 720);
    }

    #[tokio::test]
    async fn connect_reports_refused_in_chinese() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        assert_eq!(connect("127.0.0.1".into(), l.local_addr().unwrap().port()).await, Ok(()));
        let (_held, p) = refused_port();
        assert_eq!(connect("127.0.0.1".into(), p).await, Err("连接被拒绝".into()));
        assert_eq!(connect_addr("nonsense").await, Err("地址无效".into()));
    }
}
