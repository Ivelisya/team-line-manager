//! 静默与维护：各告警引擎（机器 alerts.rs、incident.rs 上的软路由 / 探测点 / 来源画像 / 中控）判断某条规则对某个对象是否被静默，
//! 机器视图、软路由、探测点列表判断是否在维护中，共用这一份查询与匹配规则。对象为空 = 全部，`rule` 为空 = 全部规则；
//! 维护（`is_maintenance`）还会暂停机器的健康判定、冻结软路由与探测点开着的告警（incident.rs）。
//!
//! 对象是机器、软路由或探测点之一（silence 表各一列外键）。三张表的 id 都是 UUIDv7、互不重复，所以这里只按 id 匹配，不用分类型。

use sqlx::PgPool;
use uuid::Uuid;

/// 刚结束的维护窗口的对象（见 [`ended_maintenance`]）；`None` = 全局维护，所有对象都算
pub struct Ended(Vec<Option<Uuid>>);

impl Ended {
    pub fn matches(&self, id: Uuid) -> bool { self.0.iter().any(|s| s.is_none_or(|s| s == id)) }
}

/// `within_s` 秒内刚结束的维护：到期的是 ends_at，提前取消的是 deleted_at；开始之前就取消、从没生效过的不算。
/// 维护期间机器的健康判定与告警计时被暂停，结束后要重新攒，用它分辨「刚结束、还没来得及报」
pub async fn ended_maintenance(db: &PgPool, within_s: i64) -> sqlx::Result<Ended> {
    Ok(Ended(
        sqlx::query_scalar(
            "select coalesce(machine_id, router_id, probe_point_id) from silence
             where is_maintenance and starts_at < least(ends_at, coalesce(deleted_at, ends_at))
               and least(ends_at, coalesce(deleted_at, ends_at)) between now() - make_interval(secs => $1) and now()",
        )
        .bind(within_s as f64)
        .fetch_all(db)
        .await?,
    ))
}

/// 此刻生效的全部静默（含维护）：(对象 id, rule, is_maintenance)。同时生效的只有寥寥几条，整份读出来在内存里匹配
pub struct Silences(Vec<(Option<Uuid>, Option<String>, bool)>);

impl Silences {
    pub async fn load(db: &PgPool) -> sqlx::Result<Self> {
        Ok(Self(
            sqlx::query_as(
                "select coalesce(machine_id, router_id, probe_point_id), rule, is_maintenance from silence
                 where deleted_at is null and now() between starts_at and ends_at",
            )
            .fetch_all(db)
            .await?,
        ))
    }

    /// 这条规则在这个对象（机器 / 软路由 / 探测点）上被静默：不开告警、不发通知
    pub fn matches(&self, rule: &str, id: Uuid) -> bool {
        self.0.iter().any(|(s, r, _)| s.is_none_or(|s| s == id) && r.as_deref().is_none_or(|r| r == rule))
    }

    /// 这个对象在维护中（全局维护对所有对象都算）
    pub fn maintenance(&self, id: Uuid) -> bool {
        self.0.iter().any(|(s, _, m)| *m && s.is_none_or(|s| s == id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subject_and_rule_wildcards() {
        let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
        let s = Silences(vec![(Some(a), Some("egress_fail".into()), false), (Some(b), None, true)]);
        assert!(s.matches("egress_fail", a) && !s.matches("tcp_fail", a), "只静默这一条规则");
        assert!(s.matches("tcp_fail", b), "规则为空 = 全部规则");
        assert!(!s.maintenance(a) && s.maintenance(b));
        let all = Silences(vec![(None, None, true)]);
        assert!(all.matches("agent_offline", a) && all.maintenance(a), "全局维护");
    }
}
