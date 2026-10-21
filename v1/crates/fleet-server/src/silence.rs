//! 静默与维护：告警引擎（alerts）、来源画像告警（ipintel）判断某条规则对某台机器是否被静默，机器视图判断是否在维护中，
//! 三处共用这一份查询与匹配规则。`subject_id` 为空 = 全部机器，`rule` 为空 = 全部规则；维护（`is_maintenance`）还会暂停健康判定。

use sqlx::PgPool;
use uuid::Uuid;

/// 此刻生效的全部静默（含维护）：(subject_id, rule, is_maintenance)。同时生效的只有寥寥几条，整份读出来在内存里匹配
pub struct Silences(Vec<(Option<Uuid>, Option<String>, bool)>);

impl Silences {
    pub async fn load(db: &PgPool) -> sqlx::Result<Self> {
        Ok(Self(
            sqlx::query_as("select subject_id, rule, is_maintenance from silence where deleted_at is null and now() between starts_at and ends_at")
                .fetch_all(db)
                .await?,
        ))
    }

    /// 这条规则在这台机器上被静默：不开告警、不发通知
    pub fn matches(&self, rule: &str, id: Uuid) -> bool {
        self.0.iter().any(|(s, r, _)| s.is_none_or(|s| s == id) && r.as_deref().is_none_or(|r| r == rule))
    }

    /// 这台机器在维护中（全局维护对所有机器都算）
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
