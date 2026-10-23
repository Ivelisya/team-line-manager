//! 访问记录保留期 `access_retention`（app_setting 表，迁移 0028）。汇率、主探测来源、三网 PING 目标等其它设置各在自己的模块里，同样存在 app_setting。
//!
//! 用户要的是：访问监控照常记录，但访问记录只留一段可设置的时间，到期自动删除（监控不该没有期限）。两个天数，各 1–90：
//! - `raw_days`：原始访问记录（access_event，按天分区）保留天数，过期的整天分区删掉（最多多留不到一天）；
//! - `hourly_days`：由原始记录算出来的汇总保留天数——小时汇总、来源小时汇总、目标首次 / 最近出现、来源 IP 画像缓存；
//!   来源画像（node_source_seen）与画像异常告警按 `profile_days`，不短于 ipintel 学习期。
//!   汇总不短于原始记录：迟到数据补算、启动补算都从原始记录重算汇总，原始记录还在的小时，汇总也得还在。
//!
//! 「新目标」「新运营商 / 新城市 / 首次出现的机房来源」= 保留期内没见过：超过保留期没再出现的记录删掉之后，再出现就算新的。
//! 访问页能选的时间范围不超过 `hourly_days`（超了的窗口接口回 400，见 api/access.rs）。删除见 `access::prune` 与 `db::maintain_partitions`，
//! 后台每小时一次，面板改了之后马上一次（`enforce`）。
//! 没设过时：原始记录按配置文件的 `[retention] access_days`，汇总 31 天（访问页最长看 30 天；这一版之前就是这样）。
//!
//! 进程内存里一份：启动时从库里读，面板改的时候更新；上报入库、访问查询、后台清理都读内存，不打库。
//! 所以只能经接口改（直接改库要重启 fleet-server 才生效）。

use crate::config::Retention;
use crate::db::audit;
use crate::error::ApiResult;
use crate::state::AppState;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// 两个天数的取值范围
pub const MIN_DAYS: u32 = 1;
pub const MAX_DAYS: u32 = 90;
/// 没设过时汇总的保留天数：访问页最长看 30 天，多留 1 天
pub const DEFAULT_HOURLY_DAYS: u32 = 31;

const KEY: &str = "access_retention";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessRetention {
    /// 原始访问记录（access_event）保留天数
    pub raw_days: u32,
    /// 汇总保留天数（小时汇总、目标首次出现、来源画像…）
    pub hourly_days: u32,
}

impl AccessRetention {
    /// 没在面板里设过时：原始记录按配置文件的 `[retention] access_days`（收进 1–90），汇总 31 天、不短于原始记录
    pub fn default_for(r: &Retention) -> Self {
        let raw = r.access_days.clamp(MIN_DAYS, MAX_DAYS);
        Self { raw_days: raw, hourly_days: DEFAULT_HOURLY_DAYS.max(raw) }
    }

    /// 校验面板 / API 给的值。收 i64：0、负数、过大的数都走这里的中文提示，而不是反序列化错误
    pub fn checked(raw_days: i64, hourly_days: i64) -> Result<Self, String> {
        let range = MIN_DAYS as i64..=MAX_DAYS as i64;
        for (what, v) in [("原始记录", raw_days), ("汇总", hourly_days)] {
            if !range.contains(&v) {
                return Err(format!("{what}的保留天数只能是 {MIN_DAYS}–{MAX_DAYS} 天（给的是 {v}）"));
            }
        }
        if hourly_days < raw_days {
            return Err(format!("汇总是从原始记录算出来的，要留得和原始记录一样久或更久：原始记录留 {raw_days} 天时，汇总至少 {raw_days} 天"));
        }
        Ok(Self { raw_days: raw_days as u32, hourly_days: hourly_days as u32 })
    }

    /// 来源画像（node_source_seen）与画像异常告警的保留天数：汇总的保留期，但不短于 ipintel 学习期（`learn_days`；ipintel 关着时传 0）。
    /// 学习期比汇总的保留期长时以学习期为准：学习期里学到的基线（见过哪些运营商 / 城市）要留到学习期结束，否则学习期还没完、
    /// 前几天学到的值就删掉了，学完之后它们一出现就被当成「新的」告警，学习期等于白学。多留的只是这几类汇总值（运营商名、城市、
    /// 接入方式、机房 / VPN 来源 IP 与最近一次出现的时间），原始记录与小时汇总照样按 `raw_days` / `hourly_days` 删
    pub fn profile_days(self, learn_days: u32) -> u32 { self.hourly_days.max(learn_days) }
}

impl AppState {
    /// 当前的访问记录保留期（内存里的，不打库）
    pub fn access_retention(&self) -> AccessRetention { *self.access_ret.read().unwrap_or_else(|e| e.into_inner()) }

    /// 来源画像实际保留的天数（见 `AccessRetention::profile_days`；ipintel 在配置里关掉时没有学习期）
    pub fn profile_days(&self) -> u32 {
        let learn = if self.ipintel.enabled { self.cfg.ipintel.learn_days } else { 0 };
        self.access_retention().profile_days(learn)
    }

    fn set_access_retention(&self, r: AccessRetention) { *self.access_ret.write().unwrap_or_else(|e| e.into_inner()) = r; }
}

/// 启动时从库里读访问记录保留期放进内存。没设过用配置文件的默认值；库里的值不对（手工改坏了）明确报错，不悄悄换成默认的
pub async fn load(st: &AppState) -> anyhow::Result<AccessRetention> {
    let v: Option<Value> = sqlx::query_scalar("select value from app_setting where key = $1").bind(KEY).fetch_optional(&st.db).await?;
    let r = match v {
        None => AccessRetention::default_for(&st.cfg.retention),
        Some(v) => {
            let n = |k: &str| v[k].as_i64().ok_or_else(|| format!("缺 {k}"));
            n("raw_days").and_then(|raw| AccessRetention::checked(raw, n("hourly_days")?)).map_err(|e| anyhow::anyhow!("app_setting 里的 {KEY} 不对（{v}）：{e}"))?
        }
    };
    st.set_access_retention(r);
    Ok(r)
}

/// 同一时刻只让一个在改：库、审计里的「从几天」与内存里的值不会交错
static SAVING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 改访问记录保留期：写库、写审计（从几天改到几天）、更新内存。返回改之前的值；没有变化时什么都不写
pub async fn save_access_retention(st: &AppState, actor: &str, new: AccessRetention) -> ApiResult<AccessRetention> {
    let _one = SAVING.lock().await;
    let old = st.access_retention();
    if old == new {
        return Ok(old);
    }
    let mut tx = st.db.begin().await?;
    sqlx::query(
        "insert into app_setting (key, value, updated_by) values ($1, $2, $3)
         on conflict (key) do update set value = excluded.value, updated_at = now(), updated_by = excluded.updated_by",
    )
    .bind(KEY)
    .bind(json!(new))
    .bind(actor)
    .execute(&mut *tx)
    .await?;
    audit(&mut tx, actor, "settings.access_retention", "setting", None, json!({"from": old, "to": new})).await?;
    tx.commit().await?;
    st.set_access_retention(new);
    Ok(old)
}

/// 按当前的保留期马上清一次过期的访问记录（面板改了保留期之后在后台调用，不等每小时的维护）：过期的原始记录分区、汇总、来源画像。
/// 清完作废访问查询的缓存：缓存里可能还有刚删掉的数据
pub async fn enforce(st: &AppState) -> anyhow::Result<()> {
    let r = st.access_retention();
    crate::db::maintain_partitions(&st.db, st.cfg.retention.raw_days, r.raw_days, 3).await?;
    let n = crate::access::prune(&st.db, r, st.profile_days()).await?;
    st.access_cache.invalidate();
    tracing::info!("按访问记录保留期（原始记录 {} 天，汇总 {} 天）清理完成，删除汇总与画像 {n} 条", r.raw_days, r.hourly_days);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_follow_config_and_stay_in_range() {
        let r = |access_days| AccessRetention::default_for(&Retention { raw_days: 3, access_days, ..Retention::default() });
        assert_eq!(r(7), AccessRetention { raw_days: 7, hourly_days: 31 });
        // 配置文件里写得比 31 天还长：汇总跟着拉长；超出 1–90 的收进范围
        assert_eq!(r(40), AccessRetention { raw_days: 40, hourly_days: 40 });
        assert_eq!(r(0), AccessRetention { raw_days: 1, hourly_days: 31 });
        assert_eq!(r(400), AccessRetention { raw_days: 90, hourly_days: 90 });
    }

    #[test]
    fn checked_bounds_and_order() {
        assert_eq!(AccessRetention::checked(1, 1), Ok(AccessRetention { raw_days: 1, hourly_days: 1 }));
        assert_eq!(AccessRetention::checked(90, 90), Ok(AccessRetention { raw_days: 90, hourly_days: 90 }));
        for (raw, hourly) in [(0, 31), (7, 0), (91, 91), (7, 91), (-1, 31), (i64::MAX, 31)] {
            assert!(AccessRetention::checked(raw, hourly).unwrap_err().contains("1–90 天"), "{raw} {hourly}");
        }
        assert!(AccessRetention::checked(7, 6).unwrap_err().contains("至少 7 天"));
    }

    #[test]
    fn profile_not_shorter_than_learning() {
        let r = AccessRetention { raw_days: 3, hourly_days: 5 };
        assert_eq!((r.profile_days(7), r.profile_days(0), AccessRetention { raw_days: 7, hourly_days: 31 }.profile_days(7)), (7, 5, 31));
    }
}
