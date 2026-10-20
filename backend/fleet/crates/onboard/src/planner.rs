//! plan：CSV → 校验 → 放置线路机 → 线路机影响预览（只读快照）→ 计划哈希。run 之前必须过这一关。

use crate::config::Config;
use crate::db::{now_ms, Batch, Db};
use crate::intake::{self, Catalog, Issue, RowSpec};
use crate::relay;
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub struct PlanOut {
    pub batch_id: String,
    pub rows: Vec<RowSpec>,
    pub issues: Vec<Issue>,
    pub options: Value,
    pub plan_hash: String,
    pub relay_impact: Vec<Value>,
}

impl PlanOut {
    pub fn errors(&self) -> usize {
        self.issues.iter().filter(|i| i.level == "error").count()
    }
    pub fn to_json(&self) -> Value {
        json!({"batch_id": self.batch_id, "plan_hash": self.plan_hash, "options": self.options, "issues": self.issues,
               "destructive_count": self.rows.iter().filter(|r| r.dd != "none").count(),
               "rows": self.rows.iter().map(|r| json!({"seq": r.seq, "name": r.short_name, "full_name": r.name, "provider": r.provider, "ip": r.ip,
                   "ssh_port": r.ssh_port, "ss_public_port": r.ss_public_port, "nat": r.nat, "auth": r.auth, "plan_id": r.plan_id, "plan": r.plan_name,
                   "price": r.price, "currency": r.currency, "team": r.team, "number": r.number, "relay": r.relay, "dd": r.dd})).collect::<Vec<_>>(),
               "relay_impact": self.relay_impact})
    }
}

pub fn default_options(cfg: &Config) -> Value {
    json!({"canary": cfg.canary, "canary_gate": cfg.canary_gate, "concurrency": cfg.concurrency, "relay_window_s": cfg.relay_window_s,
           "relay_batch_max": cfg.relay_batch_max, "dry_run": false})
}

pub async fn plan(cfg: &Config, db: &Db, csv_text: &str, csv_dir: &std::path::Path, overrides: &Value) -> Result<PlanOut, String> {
    let cat = Catalog::load(&cfg.catalog)?;
    let (mut rows, mut issues) = intake::load_rows(cfg, &cat, csv_text, csv_dir);
    let batch_id = intake::batch_id_for(csv_text);
    // 已登记资产的编号（本批自己的除外，保证同一份 CSV 重跑 plan 结果不变）
    for (team, n, row_id) in db.taken_numbers()? {
        for r in rows.iter().filter(|r| r.team == team && r.number as i64 == n) {
            if !row_id.starts_with(&format!("{batch_id}-")) {
                issues.push(Issue { seq: r.seq, level: "error", msg: format!("{} 已被别的批次登记", r.short_name) });
            }
        }
    }
    // 线路机只读快照：现有入口数、端口、已挂的节点
    let mut snaps = BTreeMap::new();
    for rc in &cfg.relays {
        match relay::snapshot(&relay::relay_remote(rc, &cfg.ssh_bin), rc).await {
            Ok(s) => {
                snaps.insert(rc.name.clone(), s);
            }
            Err(e) => issues.push(Issue { seq: 0, level: if rows.iter().any(|r| r.relay.as_deref() == Some(&rc.name)) { "error" } else { "warn" }, msg: format!("读不到 {} 的配置：{}", rc.name, e.msg) }),
        }
    }
    let existing: BTreeMap<String, usize> = snaps.iter().map(|(k, s)| (k.clone(), s.entry_count())).collect();
    issues.extend(intake::place(cfg, &mut rows, &existing));
    for r in &rows {
        if let Some(s) = r.relay.as_ref().and_then(|n| snaps.get(n)) {
            let target = format!("{}:{}", r.ip, r.ss_public_port);
            if let Some(t) = s.outbound_targets().get(&target) {
                let mine = relay::outbound_tag(&r.ip, r.ss_public_port, r.nat);
                if t != &mine || db.asset_by_row(&format!("{batch_id}-{}", r.seq))?.is_none() {
                    issues.push(Issue { seq: r.seq, level: "error", msg: format!("{target} 已挂在 {} 的转发 {t} 上（现有节点）", r.relay.as_deref().unwrap_or("")) });
                }
            }
        }
    }
    let mut relay_impact = vec![];
    for rc in &cfg.relays {
        let members: Vec<&RowSpec> = rows.iter().filter(|r| r.relay.as_deref() == Some(&rc.name)).collect();
        if members.is_empty() {
            continue;
        }
        let (n, maxp) = snaps.get(&rc.name).map(|s| (s.entry_count(), s.file_ports().into_iter().filter(|p| *p >= cfg.ss.relay_port_lo && *p <= cfg.ss.relay_port_hi).max())).unwrap_or((0, None));
        let first = maxp.map(|p| p + 1).unwrap_or(cfg.ss.relay_port_lo);
        relay_impact.push(json!({"relay": rc.name, "entries_now": n, "adding": members.len(),
            "ports_preview": format!("{}–{}（实际以登记时的预留为准）", first, first as usize + members.len() - 1),
            "members": members.iter().map(|r| r.short_name.clone()).collect::<Vec<_>>(),
            "impact": format!("新增 {} 个入口，经 Xray API 热加，不重启 Xray；现有 {n} 个入口不受影响", members.len())}));
    }
    let mut options = default_options(cfg);
    for (k, v) in overrides.as_object().into_iter().flatten() {
        options[k] = v.clone();
    }
    let plan_hash = intake::plan_hash(&rows, &options);
    Ok(PlanOut { batch_id, rows, issues, options, plan_hash, relay_impact })
}

/// 建批次（同一份 CSV 得到同一个批次 id：重复执行是幂等的）。
pub fn ensure_batch(db: &Db, p: &PlanOut, csv_text: &str, csv_path: Option<&str>, title: Option<&str>, operator: &str) -> Result<(String, bool), String> {
    if let Some(b) = db.batch(&p.batch_id)? {
        if b.options != p.options || b.plan_hash != p.plan_hash {
            db.set_batch_options_and_hash(&b.id, &p.options, &p.plan_hash)?;
        }
        return Ok((b.id, false));
    }
    if p.errors() > 0 {
        return Err(format!("计划有 {} 个错误，不能建批次", p.errors()));
    }
    let title = title.map(String::from).unwrap_or_else(|| {
        let mut prov: BTreeMap<&str, usize> = BTreeMap::new();
        for r in &p.rows {
            *prov.entry(r.provider.as_str()).or_default() += 1;
        }
        format!("新机 ×{}（{}）", p.rows.len(), prov.iter().map(|(k, v)| format!("{k} ×{v}")).collect::<Vec<_>>().join(" · "))
    });
    let b = Batch {
        id: p.batch_id.clone(),
        title,
        csv_path: csv_path.map(String::from),
        csv_text: csv_text.to_string(),
        options: p.options.clone(),
        plan_hash: p.plan_hash.clone(),
        status: "validated".into(),
        operator: operator.into(),
        created_at: now_ms(),
        started_at: None,
        finished_at: None,
        lease_owner: None,
        lease_until: None,
        error: None,
    };
    let rows: Vec<(RowSpec, String)> = p.rows.iter().map(|r| (r.clone(), r.relay.clone().unwrap_or_default())).collect();
    db.create_batch(&b, &rows).map_err(|e| if e.contains("UNIQUE") { format!("{e}：有 IP 正在别的批次里上架（同一台机器同时只能有一个运行）") } else { e })?;
    Ok((b.id, true))
}
