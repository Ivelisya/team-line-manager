//! 中控自监控：中控自己出事也要有人知道。
//!
//! - 死人开关（`[selfmon] heartbeat_url`，可选）：告警评估每轮成功后，最多每分钟 GET 一次这个地址（healthchecks.io 之类）。
//!   中控挂了、卡住、告警循环一直失败时那边收不到 ping，由它报警——这时中控自己的通知渠道也发不出去。
//! - 中控自身的告警（规则 `central_` 开头）：告警循环每轮（30 秒）评估完机器后检查一次，开、关、通知照常走 incident 表与通知渠道：
//!   - `central_channel_failing`（严重）：某个启用的通知渠道连续发送失败满 5 分钟（每个渠道一条，subject_id 是渠道 id；不发给它自己）。
//!     发成功一次（含渠道测试）就恢复；渠道停用或删除时关掉、不发恢复
//!   - `central_backup_stale`（警告）：备份目录里最新一份 `fleet-*.tar`（加密的 `.tar.age`）超过 26 小时（每天 03:30 一份，没做成两小时后就报），
//!     或目录里没有备份、读不了。目录不存在（没装备份，如实验室）不检查
//!   - `central_disk_low`（严重）：中控本机 /、PostgreSQL 数据目录、/var/lib/fleet、备份目录所在的文件系统剩余低于 10%（同 df 的可用），
//!     回到 11% 以上才恢复
//!   - `central_ingress`（严重）：有自己 agent 的在用机器里同时离线的超过一半、且至少 3 台（alerts::agents_unreachable）——
//!     中控在跑、库也好，是 agent 到不了中控（边缘机反代、Caddy、DNS）。这期间不新开单机的 agent 离线，这条恢复后还没回来的再按单机报
//!
//!   不去抖（条件本身带了时长或余量），恢复后再出现新开一条；被静默的不开、不发通知；严重的每 4 小时提醒一次（同机器的告警）。
//!   对象是「中控」：subject_id 是全零 uuid（渠道连续失败是渠道 id），接口里这类告警的 subject / subject_id 为 null、subject_name 是「中控」。

use crate::alerts;
use crate::notify::{self, Kind, Notice};
use crate::silence::Silences;
use crate::state::{AppState, Event};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime};
use time::OffsetDateTime;
use uuid::Uuid;

pub const CHANNEL_FAILING: &str = "central_channel_failing";
pub const BACKUP_STALE: &str = "central_backup_stale";
pub const DISK_LOW: &str = "central_disk_low";
pub const INGRESS: &str = "central_ingress";
/// 中控自身告警的对象 id（不是机器；渠道连续失败的告警用渠道 id）
pub const CENTRAL: Uuid = Uuid::nil();
/// 告警、通知里中控的名字
pub const NAME: &str = "中控";

/// 通知渠道连续失败满这么久开告警
const CHANNEL_FAILING_S: i64 = 5 * 60;
/// 最新一份备份超过这么久开告警（每天一份，留 2 小时余量）
const BACKUP_MAX_AGE: Duration = Duration::from_secs(26 * 3600);
/// 磁盘剩余低于这个百分比开告警，回到 DISK_OK_PCT 以上才恢复：留点余量，不在边上反复开关
const DISK_LOW_PCT: u64 = 10;
const DISK_OK_PCT: u64 = 11;
/// 要看剩余空间的目录（系统盘、PostgreSQL 数据、中控自己的数据），另加备份目录；同一个文件系统只看一次，不存在的跳过
const DISK_PATHS: [&str; 3] = ["/", "/var/lib/postgresql", "/var/lib/fleet"];

/// 中控自身的告警（对象不是机器）
pub fn is_central(rule: &str) -> bool { rule.starts_with("central_") }

/// 死人开关：告警循环每轮评估成功后调 `beat`，最多每分钟真去 ping 一次
#[derive(Default)]
pub struct Heartbeat {
    last: Option<Instant>,
}

impl Heartbeat {
    pub fn beat(&mut self, st: &AppState) {
        let Some(url) = st.cfg.selfmon.heartbeat_url.clone() else { return };
        if self.last.is_some_and(|t| t.elapsed() < Duration::from_secs(60)) {
            return;
        }
        self.last = Some(Instant::now());
        // 不等它：心跳服务慢不拖住告警循环。失败只记日志（ping 不到那边自然会报）；地址里带着检查的密钥，不写进日志
        tokio::spawn(async move {
            if let Err(e) = client().get(&url).send().await.and_then(|r| r.error_for_status()) {
                tracing::warn!("死人开关 ping 失败：{}", e.without_url());
            }
        });
    }
}

fn client() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent(concat!("fleet-server/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("构建 HTTP 客户端")
    })
}

/// 此刻成立的一个问题
struct Problem {
    rule: &'static str,
    subject: Uuid,
    severity: &'static str,
    /// 不带「中控：」前缀
    text: String,
}

#[derive(sqlx::FromRow)]
struct Open {
    id: Uuid,
    rule: String,
    subject_id: Uuid,
    severity: String,
    summary: String,
    opened_at: OffsetDateTime,
    notified_at: Option<OffsetDateTime>,
}

/// 检查一轮，把中控自身的告警对齐到 incident 表。`now` 由调用方给（测试可以快进）
pub async fn check_once(st: &AppState, now: OffsetDateTime) -> anyhow::Result<()> {
    let open: Vec<Open> = sqlx::query_as(
        "select id, rule, subject_id, severity::text severity, summary, opened_at, notified_at from incident
         where state = 'firing' and rule like 'central!_%' escape '!'",
    )
    .fetch_all(&st.db)
    .await?;
    let channels: Vec<(Uuid, String, Option<OffsetDateTime>, Option<String>)> =
        sqlx::query_as("select id, name, failing_since, last_error from notify_channel where enabled").fetch_all(&st.db).await?;
    let mut want: Vec<Problem> = channels
        .iter()
        .filter(|(_, _, since, _)| since.is_some_and(|s| (now - s).whole_seconds() >= CHANNEL_FAILING_S))
        .map(|(id, name, _, err)| Problem {
            rule: CHANNEL_FAILING,
            subject: *id,
            severity: "critical",
            text: format!(
                "通知渠道「{name}」连续发送失败超过 5 分钟：{}（发往它的通知排队重试，入队满 30 分钟还发不出的放弃）",
                err.as_deref().unwrap_or("-")
            ),
        })
        .collect();
    let dir = &st.cfg.selfmon.backup_dir;
    if let Some(text) = backup_problem(dir, SystemTime::now()) {
        want.push(Problem { rule: BACKUP_STALE, subject: CENTRAL, severity: "warning", text });
    }
    let pct = if open.iter().any(|o| o.rule == DISK_LOW) { DISK_OK_PCT } else { DISK_LOW_PCT };
    if let Some(text) = disk_problem(&disk_usage(dir), pct) {
        want.push(Problem { rule: DISK_LOW, subject: CENTRAL, severity: "critical", text });
    }
    if let Some((off, total)) = alerts::agents_unreachable(&crate::views::live(st).await?) {
        want.push(Problem { rule: INGRESS, subject: CENTRAL, severity: "critical", text: format!("{off}/{total} 台 agent 同时失联：检查边缘机 / Caddy / DNS") });
    }
    let enabled: HashSet<Uuid> = channels.iter().map(|c| c.0).collect();
    reconcile(st, open, want, &enabled, now).await
}

async fn reconcile(st: &AppState, open: Vec<Open>, want: Vec<Problem>, enabled: &HashSet<Uuid>, now: OffsetDateTime) -> anyhow::Result<()> {
    let silences = Silences::load(&st.db).await?;
    let mut open: HashMap<(String, Uuid), Open> = open.into_iter().map(|o| ((o.rule.clone(), o.subject_id), o)).collect();
    let notice = |kind, id, rule: &str, severity: &str, text: &str, opened_at| Notice {
        kind,
        incident_id: Some(id),
        rule: rule.into(),
        severity: severity.into(),
        machine: NAME.into(),
        summary: text.into(),
        at: now,
        opened_at: Some(opened_at),
    };
    let mut changed = false;
    for p in want {
        let summary = format!("{NAME}：{}", p.text);
        let silenced = silences.matches(p.rule, p.subject);
        if let Some(inc) = open.remove(&(p.rule.to_string(), p.subject)) {
            if !alerts::same_summary(&inc.summary, &summary) {
                sqlx::query("update incident set summary = $2, last_seen_at = now() where id = $1").bind(inc.id).bind(&summary).execute(&st.db).await?;
                changed = true;
            }
            // 开的时候没能写进发送队列的补发「触发」；严重的每 4 小时提醒一次（同机器的告警）
            let kind = match inc.notified_at {
                None => Some(Kind::Firing),
                Some(n) => alerts::repeat_interval_s(p.rule, p.severity).filter(|every| (now - n).whole_seconds() >= *every).map(|_| Kind::Repeat),
            };
            if let Some(kind) = kind.filter(|_| !silenced) {
                notify::enqueue(st, notice(kind, inc.id, p.rule, p.severity, &p.text, inc.opened_at)).await;
            }
            continue;
        }
        if silenced {
            continue;
        }
        let id = crate::db::new_id();
        let res = sqlx::query(
            "insert into incident (id, rule, subject_id, severity, summary) values ($1, $2, $3, $4::severity, $5)
             on conflict (rule, subject_id) where state = 'firing' do nothing",
        )
        .bind(id)
        .bind(p.rule)
        .bind(p.subject)
        .bind(p.severity)
        .bind(&summary)
        .execute(&st.db)
        .await?;
        if res.rows_affected() > 0 {
            changed = true;
            notify::enqueue(st, notice(Kind::Firing, id, p.rule, p.severity, &p.text, now)).await;
        }
    }
    // 问题没了的恢复；渠道停用或删了不是恢复：关掉、不发通知
    for (_, inc) in open {
        let gone = inc.rule == CHANNEL_FAILING && !enabled.contains(&inc.subject_id);
        let detail = if gone { json!({"closed_by": "channel_removed"}) } else { json!({}) };
        sqlx::query("update incident set state = 'resolved', resolved_at = now(), detail = detail || $2 where id = $1")
            .bind(inc.id)
            .bind(detail)
            .execute(&st.db)
            .await?;
        changed = true;
        if !gone && inc.notified_at.is_some() && !silences.matches(&inc.rule, inc.subject_id) {
            let text = notify::strip_machine(&inc.summary, NAME);
            notify::enqueue(st, notice(Kind::Resolved, inc.id, &inc.rule, &inc.severity, text, inc.opened_at)).await;
        }
    }
    if changed {
        st.emit(Event::Incidents);
    }
    Ok(())
}

/// 备份目录里最新一份（fleet-*.tar，配了 age 加密的是 fleet-*.tar.age）的问题：超过 26 小时、一份都没有、目录读不了都算；
/// 目录不存在（没装备份）不检查
fn backup_problem(dir: &Path, now: SystemTime) -> Option<String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => return Some(format!("读不了备份目录 {}（{e}），不知道每日备份做没做", dir.display())),
    };
    let newest = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_str().is_some_and(|n| n.starts_with("fleet-") && (n.ends_with(".tar") || n.ends_with(".tar.age"))))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.file_name())))
        .max_by_key(|(t, _)| *t);
    match newest {
        None => Some(format!("备份目录 {} 里没有备份（fleet-*.tar、fleet-*.tar.age）", dir.display())),
        Some((t, name)) if now.duration_since(t).unwrap_or_default() > BACKUP_MAX_AGE => Some(format!(
            "最新一份备份做于 {}（{}，{}），超过 26 小时没有新的：每日备份没做成，查 systemctl status fleet-backup",
            notify::beijing(OffsetDateTime::from(t)),
            crate::meta::org_tz_label(),
            name.to_string_lossy()
        )),
        Some(_) => None,
    }
}

/// 各文件系统的 (目录, 可用字节, 总字节)：同 df，可用是普通用户还能写的（root 预留的不算），总 = 已用 + 可用。
/// 同一个文件系统只取一次，不存在的目录跳过
fn disk_usage(backup_dir: &Path) -> Vec<(String, u64, u64)> {
    let mut seen = HashSet::new();
    let mut out = vec![];
    for p in DISK_PATHS.iter().map(Path::new).chain([backup_dir]) {
        let Ok(m) = std::fs::metadata(p) else { continue };
        if seen.insert(m.dev()) {
            if let Some((avail, total)) = statvfs(p) {
                out.push((p.display().to_string(), avail, total));
            }
        }
    }
    out
}

// 64 位 Linux 上这几个字段已经是 u64，32 位上是 u32
#[allow(clippy::unnecessary_cast)]
fn statvfs(p: &Path) -> Option<(u64, u64)> {
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY：c 是以 NUL 结尾的路径，s 是调用方给的输出结构
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    let fr = s.f_frsize as u64;
    let avail = s.f_bavail as u64 * fr;
    let used = (s.f_blocks as u64).saturating_sub(s.f_bfree as u64) * fr;
    Some((avail, used + avail))
}

/// 剩余低于 `pct`% 的文件系统拼成告警摘要；都够就是 None
fn disk_problem(usage: &[(String, u64, u64)], pct: u64) -> Option<String> {
    let low: Vec<String> = usage
        .iter()
        .filter(|(_, avail, total)| *total > 0 && avail * 100 < total * pct)
        .map(|(p, avail, total)| format!("{p} 剩余 {}%（{} / {}）", avail * 100 / total, notify::fmt_bytes(*avail as f64), notify::fmt_bytes(*total as f64)))
        .collect();
    (!low.is_empty()).then(|| format!("磁盘空间不足：{}；写满了 PostgreSQL 会停，中控跟着停", low.join("；")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_threshold_and_text() {
        let gib = 1u64 << 30;
        let usage = vec![("/".to_string(), 3 * gib, 40 * gib), ("/var/lib/postgresql".to_string(), 5 * gib, 40 * gib)];
        assert_eq!(disk_problem(&usage, DISK_LOW_PCT).as_deref(), Some("磁盘空间不足：/ 剩余 7%（3 GiB / 40 GiB）；写满了 PostgreSQL 会停，中控跟着停"));
        // 恢复要回到 11% 以上：10.5% 不开新的，但已开的不关
        let edge = vec![("/".to_string(), 42 * gib / 10, 40 * gib)];
        assert!(disk_problem(&edge, DISK_LOW_PCT).is_none());
        assert!(disk_problem(&edge, DISK_OK_PCT).is_some());
        assert!(disk_problem(&[], DISK_LOW_PCT).is_none());
    }

    #[test]
    fn real_disks_are_read() {
        // 根目录总在，读得出来；同一个文件系统只取一次
        let u = disk_usage(Path::new("/nonexistent-backups"));
        assert!(u.iter().any(|(p, _, total)| p == "/" && *total > 0), "{u:?}");
        assert!(u.iter().all(|(_, avail, total)| avail <= total));
    }

    #[test]
    fn central_rules() {
        assert!(is_central(CHANNEL_FAILING) && is_central(BACKUP_STALE) && is_central(DISK_LOW) && is_central(INGRESS));
        assert!(!is_central("agent_offline") && !is_central("ipintel_new_isp"));
    }
}
