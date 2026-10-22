//! 回收站（recycle_bin）：退役的终点。退役作业收尾时（jobs::steps::finalize，同一个事务）、或每小时的维护（spare.rs `recycle_pass`：
//! 上架取消的、这一版之前就退役了的）把机器写进回收站：一行快照与统计列，机器行设 recycled_at，从所有视图里消失。
//! 机器行连同入口、遥测之后由中控自动物理删除（db::purge_blocker 放行时，见 spare.rs）；回收站这一行一直留着（不受保留期清理影响）。
//! 旧面板的退役机器经 `POST /recycle-bin/import` 导进来（source = legacy），接口在 api/recycle.rs。

use crate::db::{audit, Tx};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

/// 插一行回收站（$1 机器 id，$2 操作人）。名字取进备用前的（spare_from），团队取那个名字的团队部分（「团队2-07」→「团队2」）；
/// 价格、币种、周期取订阅的生效值，没有订阅取模板；退役时刻取进备用的时刻，没进过备用取退役完成的时刻。
/// 流量：入口业务流量的小时汇总合计（节点按自己的入口，线路机按它上面的全部入口）、网卡累计（host_metric_hourly，有网卡读数的才有）。
/// 快照：机器行（不含退役时存的主机公钥）、显示名、团队、宿主机、供应商与套餐、订阅生效值、续费记录、入口历史、出站参数（不含秘密）
const INSERT: &str = "with m as (
    select m.*, display_name(m.kind::text, m.label, t.name, m.number) cur_name, t.name team_name,
           display_name(h.kind::text, h.label, ht.name, h.number) host_name, p.name provider_name, pl.name plan_name
    from machine m left join team t on t.id = m.team_id left join machine h on h.id = m.host_id left join team ht on ht.id = h.team_id
         left join plan pl on pl.id = m.plan_id left join provider p on p.id = coalesce(pl.provider_id, m.provider_id)
    where m.id = $1)
  insert into recycle_bin (source, source_id, machine_id, kind, name, last_name, team, ip, host_name, provider, plan, service_ref,
      price_cents, currency, cycle, activated_at, retired_at, due_date, reason, traffic_up, traffic_down, nic_rx, nic_tx, actor, snapshot)
  select 'fleet', m.id::text, m.id, m.kind::text, coalesce(m.spare_from, m.cur_name), m.cur_name,
      case when m.spare_from is not null then regexp_replace(m.spare_from, '-[0-9]+$', '') else m.team_name end,
      m.primary_ip, m.host_name, m.provider_name, m.plan_name, m.service_ref,
      coalesce(s.price_cents, tp.price_cents), coalesce(s.currency, tp.currency), coalesce(s.cycle::text, tp.cycle::text),
      m.activated_at, coalesce(m.spare_since, m.retired_at, now()), s.due_date, m.lifecycle_reason,
      tr.up, tr.down, nic.rx, nic.tx, $2,
      jsonb_build_object(
        'machine', (select to_jsonb(x) - 'retired_host_keys' from machine x where x.id = m.id),
        'name', m.cur_name, 'team', m.team_name, 'host', m.host_name, 'provider', m.provider_name, 'plan', m.plan_name,
        'subscription', to_jsonb(s),
        'renewals', (select coalesce(jsonb_agg(to_jsonb(r) - 'machine_id' order by r.created_at), '[]') from renewal r where r.machine_id = m.id),
        'entries', (select coalesce(jsonb_agg(jsonb_build_object('relay', coalesce(rm.label, ''), 'node', display_name(nm.kind::text, nm.label, nt.name, nm.number),
                                                                  'port', e.port, 'lifecycle', e.lifecycle, 'created_at', e.created_at, 'removed_at', e.removed_at)
                                               order by e.created_at), '[]')
                    from entry e join machine rm on rm.id = e.relay_id join machine nm on nm.id = e.node_id left join team nt on nt.id = nm.team_id
                    where e.node_id = m.id or e.relay_id = m.id),
        'outbound', (select to_jsonb(o) - 'secret_enc' - 'machine_id' from node_outbound o where o.machine_id = m.id))
  from m join sub_template tp on tp.machine_id = m.id left join subscription_eff s on s.machine_id = m.id
  cross join lateral (select least(coalesce(sum(h.up_bytes), 0), 9e18)::bigint up, least(coalesce(sum(h.down_bytes), 0), 9e18)::bigint down
                      from entry e join entry_traffic_hourly h on h.entry_id = e.id where e.node_id = m.id or e.relay_id = m.id) tr
  cross join lateral (select case when count(net_rx) > 0 then least(sum(net_rx), 9e18)::bigint end rx,
                             case when count(net_tx) > 0 then least(sum(net_tx), 9e18)::bigint end tx
                      from host_metric_hourly where machine_id = m.id) nic
  returning id, name, last_name";

/// 把机器写进回收站并隐藏（调用方的事务里；机器已退役）。`auto`：中控自动做的标记（spare::AUTO / spare::SWEEP），写进审计；
/// 手动「立即回收」为 None。已经回收过的不再写（重跑的退役作业收尾、维护任务），返回 None；否则返回回收站这一行的 id
pub async fn record(tx: &mut Tx<'_>, id: Uuid, actor: &str, auto: Option<&str>) -> sqlx::Result<Option<i64>> {
    let cur: Option<Option<OffsetDateTime>> = sqlx::query_scalar("select recycled_at from machine where id = $1 for update").bind(id).fetch_optional(&mut **tx).await?;
    if !matches!(cur, Some(None)) {
        return Ok(None);
    }
    // recycled_at 和这一行在同一个事务里写、上面锁着机器行：不会已经有这台的一行，有就是数据坏了，直接报错
    let (bin, name, last): (i64, String, Option<String>) = sqlx::query_as(INSERT).bind(id).bind(actor).fetch_one(&mut **tx).await?;
    sqlx::query("update machine set recycled_at = now(), revision = revision + 1, updated_at = now() where id = $1").bind(id).execute(&mut **tx).await?;
    let mut detail = json!({"name": last.as_deref().unwrap_or(&name), "from": name, "recycle_bin_id": bin});
    if let Some(a) = auto {
        detail["auto"] = json!(a);
    }
    audit(tx, actor, "machine.recycle", "machine", Some(id), detail).await?;
    Ok(Some(bin))
}
