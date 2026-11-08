//! 删软路由出口（router_exit_rm，计划见 api/router_ops.rs）：客户不要的出口从路由的 OpenClash 源配置里删掉，连同指向它的分流规则、
//! 策略组里对它的引用（路由上的脚本见 routers::replace 的 REMOVE_SCRIPT）。只有这一步，目标是路由，参数 `exits` 是出口名。
//!
//! 同一台路由同时只有一个作业在改（同「改软路由出口」的 Locks；拿不到就排队）。脚本自己备份、改、重启 OpenClash、确认删掉的出口不在了，
//! 不行整份换回备份；配置里已经没有的出口不算错，所以中途断了（结果没取回）点重试即可：按路由那时的配置再做一遍。
//! 删完把这些出口的「已建立的对应」（router_exit_link）一起删掉，马上重采一次：指着的备用机器不再被扣住，到期后自动回收。

use super::replace::{pinned_stamps, reason, run_script, ssh_failed, Locks, BUSY_RETRY};
use super::*;
use crate::routers::{self, replace as rr};

pub(super) async fn remove_exits(c: &Ctx<'_>) -> anyhow::Result<Outcome> {
    let id = c.target_id()?;
    let names: Vec<String> = c.job.params["exits"].as_array().into_iter().flatten().filter_map(Value::as_str).map(String::from).collect();
    if names.is_empty() {
        bail!("作业参数里没有要删的出口");
    }
    let Some(r) = routers::load(&c.st.db, Some(id)).await?.into_iter().next() else { return Err(remote("这台软路由已经删掉了")) };
    if r.state() != "online" {
        return Err(StepError::err(ErrKind::Network, format!("软路由「{}」现在不在线，一行都没删：等它在线后点重试", r.name)));
    }
    let mut locks = Locks::new(&c.st.db);
    if !locks.take(r.id).await? {
        if !c.job.params["routers_queued"].is_string() {
            c.log(format!("软路由「{}」正在被别的作业改，排队等它改完", r.name)).await;
            c.set_params(json!({"routers_queued": r.name})).await?;
        }
        return Ok(Outcome::Queue(10));
    }
    if c.job.params["routers_queued"].is_string() {
        c.set_params(json!({"routers_queued": null})).await?;
    }
    let t = OffsetDateTime::now_utc();
    let stamp = rr::stamp(t);
    let pins = pinned_stamps(&c.st.db).await?;
    c.log(format!("软路由「{}」：删出口 {}，连同指向它们的分流规则和策略组里的引用（先备份，删完重启 OpenClash）", r.name, names.join("、"))).await;
    c.st.routers_refresh.claims.editing(r.id, c.job.id, &c.job.kind);
    let res = run_script(c, r.id, &r.name, r.tunnel_port, &rr::remove_script(&names, t, &pins), "remove", &stamp, rr::APPLY_TIMEOUT).await;
    c.st.routers_refresh.claims.edited(r.id, c.job.id, res.as_ref().ok().and_then(|o| o.kv("sha")));
    let o = match res {
        Ok(o) => o,
        Err(e) => return Err(StepError::err(kind_of(&e), format!("软路由「{}」：{e:#}（可能已经删了：点重试会按路由现在的配置接着做）", r.name))),
    };
    // 不是本脚本自己的话（fleet： 开头的）都进日志：配置里已经没有的出口
    let notes: Vec<&str> = o.stderr.lines().filter_map(|l| l.strip_prefix("fleet：")).collect();
    if o.code == rr::BUSY {
        c.warn(format!("软路由「{}」：{}，{BUSY_RETRY} 秒后再试", r.name, reason(&o))).await;
        return Ok(Outcome::Wait(BUSY_RETRY));
    }
    if o.code == 255 {
        let why = ssh_failed(c, &r.name, &o).await;
        return Err(StepError::err(o.ssh_kind().unwrap_or(ErrKind::Network), format!("软路由「{}」：{why}（可能已经删了：点重试会按路由现在的配置接着做）", r.name)));
    }
    if !o.ok() {
        let undone = if o.kv("rolled_back") == Some("1") { "；已换回删之前的配置" } else { "" };
        return Err(remote(format!("软路由「{}」：{}{undone}", r.name, reason(&o))));
    }
    if !notes.is_empty() {
        c.log(format!("软路由「{}」：{}", r.name, notes.join("；"))).await;
    }
    sqlx::query("delete from router_exit_link where router_id = $1 and exit = any($2)").bind(r.id).bind(&names).execute(&c.st.db).await?;
    c.st.routers_refresh.now([r.id]).await;
    if o.kv("remove") == Some("none") {
        return Ok(Outcome::Skipped("这些出口已经都不在配置里了，没有改动".into()));
    }
    let n = |k: &str| o.kv(k).and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
    c.log(format!(
        "软路由「{}」删好了：出口 {} 个、分流规则 {} 条、策略组里的引用 {} 处；备份 {}",
        r.name,
        n("removed"),
        n("rules"),
        n("members"),
        o.kv("backup").unwrap_or("?")
    ))
    .await;
    Ok(Outcome::Done(json!({"router": {"id": r.id, "name": r.name}, "exits": names, "removed": n("removed"), "rules": n("rules"), "members": n("members"), "backup": o.kv("backup")})))
}
