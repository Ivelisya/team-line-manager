//! 共用的常量、枚举与中文名：只在这里定义一次。中控自己的文案（作业名、告警、通知、报错）、健康阈值都从这里取，
//! `GET /meta` 原样发给网页和 fleetctl，它们不再各写一份。新增枚举值、改叫法只改这里。

use anyhow::Context;
use serde_json::{json, Map, Value};
use sqlx::PgPool;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::RwLock;
use time::{Date, Month, OffsetDateTime, UtcOffset};

// ── 组织时区 ──────────────────────────────────────────────────────────────
// fleet.toml 的 org_timezone：账单时区、供应商时区没填时的兜底，通知、审计、fleetctl 表格里的时间也按它写。
// 库里的视图（sub_template）读 org_setting 表里的同一个值（启动时由 `sync_org_tz` 写入）

/// 配置不写 org_timezone 时的组织时区
pub const DEFAULT_ORG_TZ: &str = "Asia/Shanghai";
/// DEFAULT_ORG_TZ 的 UTC 偏移（秒）：启动时从库里算出组织时区的真实偏移之前用
const DEFAULT_ORG_OFFSET_S: i32 = 8 * 3600;
static ORG_TZ: RwLock<String> = RwLock::new(String::new());
static ORG_OFFSET_S: AtomicI32 = AtomicI32::new(DEFAULT_ORG_OFFSET_S);

/// 组织时区的 IANA 名
pub fn org_tz() -> String {
    let s = ORG_TZ.read().unwrap_or_else(|e| e.into_inner());
    if s.is_empty() { DEFAULT_ORG_TZ.into() } else { s.clone() }
}

/// 组织时区现在的 UTC 偏移（每小时按库里的时区库刷新，夏令时切换后最多晚一小时）
pub fn org_offset() -> UtcOffset { UtcOffset::from_whole_seconds(ORG_OFFSET_S.load(Ordering::Relaxed)).unwrap_or(UtcOffset::UTC) }

/// 时区的叫法：北京时间 / UTC / IANA 名（通知、审计里「（北京时间）」这类括注）
pub fn tz_label(tz: &str) -> String {
    match tz {
        "Asia/Shanghai" => "北京时间".into(),
        "UTC" | "Etc/UTC" => "UTC".into(),
        z => z.into(),
    }
}

pub fn org_tz_label() -> String { tz_label(&org_tz()) }

/// 组织时区的 `2026-10-20 15:04:05`
pub fn org_time(t: OffsetDateTime) -> String {
    let f = time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");
    t.to_offset(org_offset()).format(&f).unwrap_or_default()
}

/// 校验组织时区（PG 要认识）、写进库（org_setting）、算出当前偏移。启动时（失败则不启动）与每小时调用
pub async fn sync_org_tz(db: &PgPool, tz: &str) -> anyhow::Result<()> {
    let off: i32 = sqlx::query_scalar("select extract(epoch from (now() at time zone $1) - (now() at time zone 'UTC'))::int")
        .bind(tz)
        .fetch_one(db)
        .await
        .with_context(|| format!("org_timezone「{tz}」不是 IANA 时区名（如 Asia/Shanghai）"))?;
    sqlx::query("update org_setting set timezone = $1 where timezone <> $1").bind(tz).execute(db).await?;
    *ORG_TZ.write().unwrap_or_else(|e| e.into_inner()) = tz.to_string();
    ORG_OFFSET_S.store(off, Ordering::Relaxed);
    Ok(())
}

/// 时区下拉的常用项（网页账单时区、供应商时区）。不在里面的照样能填，由中控按 PG 的时区库校验
pub const TIMEZONES: &[&str] = &[
    "Asia/Shanghai", "Asia/Hong_Kong", "Asia/Taipei", "Asia/Tokyo", "Asia/Seoul", "Asia/Singapore",
    "Europe/London", "Europe/Berlin", "Europe/Amsterdam", "America/Los_Angeles", "America/Chicago", "America/New_York", "UTC",
];

// ── 币种与计费周期 ────────────────────────────────────────────────────────

/// 币种下拉的常用项；其它 3 字母代码照样能填
pub const CURRENCIES: &[&str] = &["CNY", "USD", "EUR", "HKD", "JPY", "GBP"];
/// 新建套餐不给币种时、没有套餐的机器的订阅模板（库里 sub_template 同值）
pub const DEFAULT_CURRENCY: &str = "USD";

/// 币种：3 个字母的代码（ISO 4217），转成大写。套餐、订阅、上架共用这一个校验
pub fn currency(s: &str) -> Result<String, String> {
    let u = s.trim().to_ascii_uppercase();
    if u.len() == 3 && u.bytes().all(|b| b.is_ascii_uppercase()) {
        Ok(u)
    } else {
        Err(format!("币种「{}」不对：写 3 个字母的代码，如 USD、CNY", s.trim()))
    }
}

pub struct Cycle {
    pub key: &'static str,
    /// 「每月」「39.46 USD / 月」里的那个字
    pub label: &'static str,
    pub months: u32,
}

/// 计费周期（库里的 billing_cycle 枚举）
pub const CYCLES: &[Cycle] = &[
    Cycle { key: "monthly", label: "月", months: 1 },
    Cycle { key: "quarterly", label: "季", months: 3 },
    Cycle { key: "semiannual", label: "半年", months: 6 },
    Cycle { key: "yearly", label: "年", months: 12 },
];
pub const DEFAULT_CYCLE: &str = "monthly";

pub fn cycle_months(key: &str) -> Option<u32> { CYCLES.iter().find(|c| c.key == key).map(|c| c.months) }

/// 计费周期只认 CYCLES 里的
pub fn check_cycle(key: &str) -> Result<(), String> {
    match cycle_months(key) {
        Some(_) => Ok(()),
        None => Err(format!("计费周期「{key}」不认识：只能是 {}", CYCLES.iter().map(|c| format!("{}（{}）", c.key, c.label)).collect::<Vec<_>>().join(" / "))),
    }
}

/// 加 n 个月（可为负），目标月没有那一天时落到月末（1-31 加一个月是 2-28，闰年 2-29）。
/// 从原日期一次加够，不逐月累加：1-31 加两个月是 3-31。超出日期范围为 None
pub fn add_months(d: Date, n: i64) -> Option<Date> {
    let m = i64::from(d.year()) * 12 + i64::from(u8::from(d.month())) - 1 + n;
    let month = Month::try_from((m.rem_euclid(12) + 1) as u8).ok()?;
    let year = i32::try_from(m.div_euclid(12)).ok()?;
    Date::from_calendar_date(year, month, d.day().min(month.length(year))).ok()
}

/// 月费（分）：按周期折成每月，四舍五入。总览的费用合计、网页的月费列都用它
pub fn monthly_cents(price_cents: i32, cycle: &str) -> i64 {
    let m = i64::from(cycle_months(cycle).unwrap_or(1));
    (i64::from(price_cents) * 2 + m).div_euclid(2 * m)
}

// ── 到期 ──────────────────────────────────────────────────────────────────

/// 订阅的到期状态：overdue = 已过期；soon = 到期前 remind_days 天内（含今天）且不是自动续费；其余 ok。
/// 续费告警（renewal_due / renewal_overdue）、总览的「即将到期」、网页与 fleetctl 都按它
pub fn due_state(due_days: i64, remind_days: i32, auto_renew: bool) -> &'static str {
    if due_days < 0 {
        "overdue"
    } else if due_days <= i64::from(remind_days) && !auto_renew {
        "soon"
    } else {
        "ok"
    }
}
/// 供应商不填提前提醒天数时（库里 provider.remind_days 的列默认值同值）
pub const DEFAULT_REMIND_DAYS: i32 = 7;

// ── 阈值 ──────────────────────────────────────────────────────────────────

/// 持续高负载告警（%）：最近 5 分钟的每个样本都不低于它才报；网页的 CPU / 内存 / 磁盘读数到它标红
pub const CPU_HIGH_PCT: f32 = 90.0;
pub const MEM_HIGH_PCT: f32 = 90.0;
pub const DISK_HIGH_PCT: f32 = 85.0;
/// 负载（1 分钟负载 / 核数）不告警，网页到它标红
pub const LOAD_HIGH_PCT: f32 = 90.0;
/// 网页资源读数到它标黄（不告警）
pub const USAGE_WARN_PCT: f32 = 75.0;
/// 流量额度：本周期已用到上限的这么多（%）报警告、网页算「额度吃紧」，到 100%（已超限）报严重
pub const QUOTA_WARN_PCT: f64 = 90.0;
pub const QUOTA_OVER_PCT: f64 = 100.0;

// ── 节点出站（Shadowsocks）默认值 ─────────────────────────────────────────

pub const SS_METHOD: &str = "aes-256-gcm";
/// 普通 VPS 节点（宿主机）上 ssserver 固定监听的端口；同机 IP 节点从它往上分
pub const SS_PORT: u16 = 8388;

// ── 枚举的中文名 ──────────────────────────────────────────────────────────

type Labels = &'static [(&'static str, &'static str)];

/// 机器类型（库里的 machine_kind）。同机 IP 节点是加在宿主机上的 vps，叫「同机 IP 节点」；泛说「节点」= VPS 节点 / 家宽代理
pub const KINDS: Labels = &[("vps", "VPS 节点"), ("proxy", "家宽代理"), ("relay", "线路机")];
pub const IP_NODE: &str = "同机 IP 节点";

pub const LIFECYCLES: Labels = &[
    ("draft", "草稿"),
    ("provisioning", "上架中"),
    ("active", "运行中"),
    ("retiring", "退役中"),
    ("retired", "已退役"),
    ("failed", "上架失败"),
];

/// 健康状态（health::Status）：全站只用这几个词，不加「含」「中」之类的前后缀
pub const HEALTHS: Labels = &[("ok", "正常"), ("degraded", "降级"), ("down", "故障"), ("unknown", "未知"), ("maintenance", "维护"), ("inactive", "未启用")];

/// 告警级别：中文名与色调（网页按色调着色）
pub const SEVERITIES: &[(&str, &str, &str)] = &[("critical", "严重", "bad"), ("warning", "警告", "warn"), ("info", "提示", "info")];

pub const JOB_KINDS: Labels = &[
    ("provision_vps", "上架 VPS 节点"),
    ("provision_proxy", "上架家宽代理"),
    ("provision_relay", "上架线路机"),
    ("provision_ip", "上架同机 IP 节点"),
    ("attach_entry", "新增入口"),
    ("move_entry", "换线路机"),
    ("restore_vps", "恢复 VPS 节点"),
    ("restore_proxy", "恢复家宽代理"),
    ("restore_relay", "恢复线路机"),
    ("restore_ip", "恢复同机 IP 节点"),
    ("retire", "退役"),
    ("purge", "彻底删除"),
    ("upgrade_agent", "升级 agent"),
];

pub const JOB_STATES: Labels = &[
    ("queued", "排队中"),
    ("running", "进行中"),
    ("waiting", "等待中"),
    ("cancelling", "取消中"),
    ("succeeded", "已完成"),
    ("failed", "失败"),
    ("cancelled", "已取消"),
];

/// 作业步骤的状态（compensated：作业取消或失败后，做过的步骤被逆序撤销）
pub const STEP_STATES: Labels = &[("pending", "等待"), ("running", "进行中"), ("done", "已完成"), ("skipped", "已跳过"), ("failed", "失败"), ("compensated", "已补偿")];

/// 入口状态：pending / removing 是等线路机应用
pub const ENTRY_STATES: Labels = &[("pending", "应用中"), ("active", "运行中"), ("disabled", "已停用"), ("removing", "移除中"), ("removed", "已移除")];

/// 供应商的网卡计量规则（流量额度怎么量）；没有网卡读数的（家宽代理、同机 IP 节点）按业务流量
pub const TRAFFIC_RULES: Labels = &[("min", "网卡入出取小"), ("max", "网卡入出取大"), ("sum", "网卡入出相加"), ("out", "只算网卡出站")];
pub const DEFAULT_TRAFFIC_RULE: &str = "max";
/// 供应商的流量重置方式：month = 账单时区的自然月（每月 1 号），billing = 每个账单日（到期日几号）
pub const TRAFFIC_RESETS: Labels = &[("month", "自然月"), ("billing", "账单日")];
pub const DEFAULT_TRAFFIC_RESET: &str = "month";

/// 访问策略的模式（团队 / 节点）；inherit 只用于节点：跟随团队
pub const POLICY_MODES: Labels = &[("observe", "仅观察"), ("allowlist", "白名单"), ("denylist", "黑名单"), ("blocked", "暂停新连接"), ("inherit", "跟随团队")];

/// 策略版本在各线路机上的同步状态（gone：线路机已退役）
pub const POLICY_SYNC_STATES: Labels = &[("applied", "已应用"), ("applying", "进行中"), ("failed", "失败"), ("gone", "已退役")];

pub const DUE_STATES: Labels = &[("ok", "未到期"), ("soon", "即将到期"), ("overdue", "已过期")];

/// 告警规则的中文名（告警、通知、静默、网页、fleetctl 同一套）
pub const RULES: Labels = &[
    ("agent_offline", "agent 离线"),
    ("clock_skew", "agent 时钟不准"),
    ("no_entry", "没有在用入口"),
    ("egress_fail", "出口不通"),
    ("tcp_fail", "线路到节点不通"),
    ("renewal_due", "即将到期"),
    ("renewal_overdue", "已过期"),
    ("relay_apply_lag", "配置未应用"),
    ("relay_apply_error", "配置应用失败"),
    ("relay_xray_down", "Xray 未运行"),
    ("host_cpu_high", "CPU 持续偏高"),
    ("host_mem_high", "内存持续偏高"),
    ("host_disk_high", "磁盘空间不足"),
    ("traffic_quota_high", "流量额度吃紧"),
    ("ipintel_new_isp", "来源出现新运营商"),
    ("ipintel_new_city", "来源出现新城市"),
    ("ipintel_datacenter_source", "机房 / VPN 来源"),
    ("policy_blocked", "访问策略暂停"),
    ("central_channel_failing", "通知渠道发送失败"),
    ("central_backup_stale", "备份过期"),
    ("central_disk_low", "中控磁盘空间不足"),
];

/// 查中文名；没收录的原样返回
pub fn label(table: Labels, key: &str) -> &str { table.iter().find(|(k, _)| *k == key).map_or(key, |(_, v)| v) }

pub fn kind_label(k: &str) -> &str { label(KINDS, k) }
pub fn lifecycle_label(l: &str) -> &str { label(LIFECYCLES, l) }
pub fn job_kind_label(k: &str) -> &str { label(JOB_KINDS, k) }
pub fn rule_label(k: &str) -> &str { label(RULES, k) }
/// 告警级别的中文名；不认识的按「提示」
pub fn severity_label(s: &str) -> &'static str { SEVERITIES.iter().find(|x| x.0 == s).map_or("提示", |x| x.1) }

fn map(t: Labels) -> Value { Value::Object(t.iter().map(|(k, v)| (k.to_string(), json!(v))).collect::<Map<_, _>>()) }

/// `GET /meta` 的内容（组织时区取当前值，其余是常量）
pub fn to_json() -> Value {
    let tz = org_tz();
    let mut tzs: Vec<String> = TIMEZONES.iter().map(|z| z.to_string()).collect();
    if !tzs.contains(&tz) {
        tzs.insert(0, tz.clone());
    }
    let usage = |bad: f32| json!({"warn": USAGE_WARN_PCT, "bad": bad});
    json!({
        "org_timezone": tz,
        "org_timezone_label": tz_label(&tz),
        "org_utc_offset_s": org_offset().whole_seconds(),
        "timezones": tzs,
        "currencies": CURRENCIES,
        "cycles": CYCLES.iter().map(|c| json!({"key": c.key, "label": c.label, "months": c.months})).collect::<Vec<_>>(),
        "kinds": map(KINDS),
        "ip_node_label": IP_NODE,
        "lifecycles": map(LIFECYCLES),
        "healths": map(HEALTHS),
        "severities": SEVERITIES.iter().map(|(k, l, t)| (k.to_string(), json!({"label": l, "tone": t}))).collect::<Map<_, _>>(),
        "job_kinds": map(JOB_KINDS),
        "job_states": map(JOB_STATES),
        "step_states": map(STEP_STATES),
        "entry_states": map(ENTRY_STATES),
        "traffic_rules": map(TRAFFIC_RULES),
        "traffic_resets": map(TRAFFIC_RESETS),
        "due_states": map(DUE_STATES),
        "policy_modes": map(POLICY_MODES),
        "policy_sync_states": map(POLICY_SYNC_STATES),
        "rules": map(RULES),
        "thresholds": {
            "traffic_quota_warn_pct": QUOTA_WARN_PCT,
            "traffic_quota_over_pct": QUOTA_OVER_PCT,
            "usage": {"cpu": usage(CPU_HIGH_PCT), "mem": usage(MEM_HIGH_PCT), "disk": usage(DISK_HIGH_PCT), "load": usage(LOAD_HIGH_PCT)},
        },
        "defaults": {
            "currency": DEFAULT_CURRENCY, "cycle": DEFAULT_CYCLE, "remind_days": DEFAULT_REMIND_DAYS,
            "traffic_rule": DEFAULT_TRAFFIC_RULE, "traffic_reset": DEFAULT_TRAFFIC_RESET, "ss_method": SS_METHOD, "ss_port": SS_PORT,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::date;

    #[test]
    fn currency_is_three_letters() {
        assert_eq!(currency(" usd ").unwrap(), "USD");
        for bad in ["", "US", "USDT", "U$D", "人民币"] {
            assert!(currency(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn months_and_monthly_cost() {
        assert_eq!(add_months(date!(2026 - 01 - 31), 1), Some(date!(2026 - 02 - 28)));
        assert_eq!(add_months(date!(2028 - 01 - 31), 1), Some(date!(2028 - 02 - 29)));
        assert_eq!(add_months(date!(2026 - 01 - 31), 2), Some(date!(2026 - 03 - 31)));
        assert_eq!(add_months(date!(2026 - 11 - 30), 3), Some(date!(2027 - 02 - 28)));
        assert_eq!(add_months(date!(2026 - 03 - 31), -1), Some(date!(2026 - 02 - 28)));
        assert_eq!(add_months(date!(2026 - 10 - 25), i64::MAX / 2), None);
        // 四舍五入：100 / 3 = 33.33 → 33，200 / 3 = 66.67 → 67
        assert_eq!((monthly_cents(100, "quarterly"), monthly_cents(200, "quarterly"), monthly_cents(1200, "yearly"), monthly_cents(999, "monthly")), (33, 67, 100, 999));
        assert!(check_cycle("weekly").is_err() && check_cycle("semiannual").is_ok());
    }

    #[test]
    fn due_state_follows_remind_days() {
        assert_eq!(due_state(-1, 7, true), "overdue");
        assert_eq!((due_state(0, 7, false), due_state(10, 10, false), due_state(11, 10, false)), ("soon", "soon", "ok"));
        assert_eq!(due_state(3, 7, true), "ok", "自动续费的到期前不算");
    }

    #[test]
    fn labels_and_meta_shape() {
        assert_eq!((kind_label("vps"), job_kind_label("restore_vps"), rule_label("new_rule")), ("VPS 节点", "恢复 VPS 节点", "new_rule"));
        assert_eq!((severity_label("critical"), severity_label("x")), ("严重", "提示"));
        let m = to_json();
        assert_eq!(m["org_timezone"], DEFAULT_ORG_TZ);
        assert_eq!(m["cycles"][1], json!({"key": "quarterly", "label": "季", "months": 3}));
        assert_eq!(m["thresholds"]["usage"]["disk"]["bad"], 85.0);
        assert!(!m.to_string().contains("落地机"));
    }
}
