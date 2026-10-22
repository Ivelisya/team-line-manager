//! 中控的 `GET /meta`：枚举的中文名、组织时区、计费周期等只在中控定义一次，这里不再各写一份。
//! 一次命令最多取一次，而且要用到时才取（`--json` 输出原值，一般用不着）；取不到时中文名退回原值、时间按 UTC 写。

use crate::client::{Client, Cred};
use serde_json::Value;
use std::sync::OnceLock;
use time::UtcOffset;

static SOURCE: OnceLock<(String, String)> = OnceLock::new();
static META: OnceLock<Option<Value>> = OnceLock::new();

/// 记下中控地址与密钥（make_ctx 调）；不在这里请求
pub fn init(url: &str, token: &str) { let _ = SOURCE.set((url.to_string(), token.to_string())); }

fn get() -> Option<&'static Value> {
    META.get_or_init(|| {
        let (url, token) = SOURCE.get()?;
        Client::new(url, Cred::Bearer(token.clone())).ok()?.get("/meta").ok()
    })
    .as_ref()
}

/// 单元测试用：和中控默认配置相同的一份（组织时区北京时间）
#[cfg(test)]
pub fn test_init() {
    let _ = META.set(Some(serde_json::json!({
        "org_timezone": "Asia/Shanghai", "org_timezone_label": "北京时间", "org_utc_offset_s": 28800,
        "cycles": [{"key": "monthly", "label": "月", "months": 1}, {"key": "quarterly", "label": "季", "months": 3},
                   {"key": "semiannual", "label": "半年", "months": 6}, {"key": "yearly", "label": "年", "months": 12}],
        "kinds": {"vps": "VPS 节点", "proxy": "家宽代理", "relay": "线路机"},
        "traffic_rules": {"min": "入站和出站取较小值", "max": "入站和出站取较大值", "sum": "入站加出站", "out": "只算出站"},
        "traffic_resets": {"month": "自然月", "billing": "账单日", "rolling30": "滚动 30 天"},
        "entry_states": {"pending": "应用中", "active": "运行中", "disabled": "已停用", "removing": "移除中", "removed": "已移除"},
        "severities": {"critical": {"label": "严重"}, "warning": {"label": "警告"}, "info": {"label": "提示"}},
        "policy_sync_states": {"applied": "已应用", "applying": "进行中", "failed": "失败", "gone": "已退役"},
        "policy_modes": {"observe": "仅观察", "allowlist": "白名单", "denylist": "黑名单", "blocked": "暂停新连接", "inherit": "跟随团队"},
    })));
}

/// `table`（kinds / lifecycles / healths / job_kinds / job_states / step_states / entry_states / traffic_rules / traffic_resets / rules / due_states）里的中文名；
/// 没收录或没取到 meta 时原样
pub fn label<'a>(table: &str, key: &'a str) -> &'a str { get().and_then(|m| m[table][key].as_str()).unwrap_or(key) }

/// 告警级别的中文名（严重 / 警告 / 提示）
pub fn severity(s: &str) -> &str { get().and_then(|m| m["severities"][s]["label"].as_str()).unwrap_or(s) }

/// 计费周期的那个字（月 / 季 / 半年 / 年）
pub fn cycle(c: &str) -> &str { cycles().find(|x| x["key"] == c).and_then(|x| x["label"].as_str()).unwrap_or(c) }

/// 中控认的计费周期 key（monthly / quarterly / …）
pub fn cycles() -> impl Iterator<Item = &'static Value> { get().and_then(|m| m["cycles"].as_array()).into_iter().flatten() }

/// 组织时区现在的 UTC 偏移：表格、续费记录、静默起止都按它写（同网页之外的通知、审计）
pub fn offset() -> UtcOffset {
    get().and_then(|m| m["org_utc_offset_s"].as_i64()).and_then(|s| UtcOffset::from_whole_seconds(s as i32).ok()).unwrap_or(UtcOffset::UTC)
}

/// 组织时区的叫法（北京时间 / IANA 名）；没取到 meta 时是 UTC（时间也按 UTC 写）
pub fn tz_label() -> &'static str { get().and_then(|m| m["org_timezone_label"].as_str()).unwrap_or("UTC") }
