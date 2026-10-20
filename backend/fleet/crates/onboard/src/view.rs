//! 视图：与原型「上架流水线」页（prototype/original-refined/js/pages/onboarding.js，数据来自
//! snapshot.json 的 onboarding.demo_batch）同形的批次 JSON；以及 fleetctl 的文字矩阵。

use crate::config::Config;
use crate::db::{now_ms, Db};
use crate::model::{cell, row as rs, step_name, PHASES, STEPS};
use serde_json::{json, Value};

fn ui_cell_status(s: &str) -> &str {
    match s {
        cell::COMPENSATED => "cancelled",
        "waiting_gate" => "queued",
        x => x,
    }
}
fn ui_row_state(s: &str) -> &str {
    match s {
        rs::READY_FOR_RELAY => rs::IN_RELAY_BATCH,
        x => x,
    }
}

pub fn batch_view(db: &Db, cfg: &Config, id: &str) -> Result<Value, String> {
    let b = db.batch(id)?.ok_or("批次不存在")?;
    let rows = db.rows(id)?;
    let now = now_ms();
    let mut out_rows = vec![];
    let (mut done, mut running, mut attention, mut failed) = (0, 0, 0, 0);
    for r in &rows {
        let steps = db.steps(&r.id)?;
        let asset = db.asset_by_row(&r.id)?;
        let cells: Vec<Value> = steps
            .iter()
            .map(|s| {
                let dur = match (s.started_at, s.finished_at) {
                    (Some(a), Some(z)) => Some(((z - a) as f64 / 1000.0).round() as i64),
                    (Some(a), None) if s.status == cell::RUNNING => Some(((now - a) as f64 / 1000.0).round() as i64),
                    _ => None,
                };
                let mut c = json!({"step": s.step, "status": ui_cell_status(&s.status), "duration_s": dur});
                if let Some(d) = &s.detail {
                    c["detail"] = json!(d);
                }
                if s.attempt > 1 {
                    c["attempt"] = json!(s.attempt);
                }
                c
            })
            .collect();
        let st = ui_row_state(&r.state);
        match st {
            rs::ACTIVE => done += 1,
            rs::WAITING_INPUT | rs::BLOCKED => attention += 1,
            rs::FAILED => {
                attention += 1;
                failed += 1
            }
            rs::CANCELLED => {}
            _ => running += 1,
        }
        let applicable = steps.iter().filter(|s| s.status != cell::SKIPPED).count();
        let ok = steps.iter().filter(|s| s.status == cell::SUCCEEDED).count();
        let mut row = json!({
            "id": r.id, "name": r.spec.short_name, "full_name": r.spec.name, "provider": r.spec.provider, "provider_code": r.spec.provider_code,
            "plan_id": r.spec.plan_id, "plan": r.spec.plan_name, "price": r.spec.price, "currency": r.spec.currency,
            "bandwidth": r.spec.bandwidth_mbps.map(|b| format!("{b} Mbps")), "relay": r.relay,
            "relay_port": asset.as_ref().and_then(|a| a["relay_port"].as_i64()), "team": r.spec.team, "service_id": r.spec.service_id,
            "ip": r.spec.ip, "ssh_port": r.spec.ssh_port, "ss_public_port": r.spec.ss_public_port, "nat": r.spec.nat,
            "mode": "new", "dd": r.spec.dd, "note": r.spec.note, "row_state": st, "current_step": r.current_step, "reason": r.reason,
            "dod": format!("{ok}/{applicable}"), "canary": r.is_canary, "cells": cells,
        });
        if st == rs::BLOCKED {
            row["risk"] = json!({"message": r.reason});
            row["error_class"] = json!(steps.iter().find(|s| s.status == cell::BLOCKED).and_then(|s| s.error_class.clone()));
        }
        out_rows.push(row);
    }
    // 线路机合批：已发车的来自 relay_batches；已就绪还在窗口里的是 collecting
    let window = b.options["relay_window_s"].as_i64().unwrap_or(cfg.relay_window_s as i64);
    let mut groups = vec![];
    for g in db.relay_batches(id)? {
        let state = match g["status"].as_str().unwrap_or("") {
            "merging" => "merging",
            "done" => "done",
            "interrupted" => "partial",
            _ => "partial",
        };
        let rep = &g["report"];
        let members = g["members"].clone();
        let impact = if rep["diff"]["new_inbounds"].as_array().is_some_and(|n| n.is_empty()) && rep["api_skipped_already_present"].as_str().is_some_and(|s| !s.trim().is_empty()) {
            format!("上次中断前已完成（运行态与文件都已有），本次只核对；Xray 重启：{}", if rep["xray"]["restarted"] == true { "是" } else { "否" })
        } else if let Some(n) = rep["diff"]["new_inbounds"].as_array() {
            let ports: Vec<String> = n.iter().filter_map(|t| t.as_str().map(|x| x.trim_start_matches("inbound-").to_string())).collect();
            format!("新增 {} 个入口 {}，经 Xray API 热加；Xray 重启：{}", ports.len(), ports.join("、"), if rep["xray"]["restarted"] == true { "是" } else { "否" })
        } else if g["status"] == "interrupted" {
            "上次执行中断，成员已重新组批（接入按 tag 幂等）".to_string()
        } else {
            g["error"].as_str().unwrap_or("").chars().take(160).collect()
        };
        groups.push(json!({"relay": g["relay"], "batch_no": g["batch_no"], "state": state, "members": members, "xray_reloads": 0,
            "xray_restarts": rep["xray"]["restarted"].as_bool().map(|x| x as i32).unwrap_or(0), "probe_restarts": 0, "impact": impact,
            "config_sha_before": rep["config_sha_before"], "config_sha_after": rep["config_sha_after"]}));
    }
    let mut collecting: std::collections::BTreeMap<String, (Vec<String>, i64)> = Default::default();
    for r in rows.iter().filter(|r| r.state == rs::READY_FOR_RELAY) {
        let e = collecting.entry(r.relay.clone()).or_insert((vec![], i64::MAX));
        e.0.push(r.spec.short_name.clone());
        e.1 = e.1.min(r.ready_since.unwrap_or(now));
    }
    for (relay, (members, since)) in collecting {
        let left = (window * 1000 - (now - since)).max(0) / 1000;
        groups.push(json!({"relay": relay, "batch_no": groups.iter().filter(|g| g["relay"] == relay.as_str()).count() + 1, "state": "collecting",
            "window_left_s": left, "members": members, "xray_reloads": 0, "probe_restarts": 0, "impact": "窗口结束后合并为一次 Xray API 变更（不重启）"}));
    }
    let canary = rows.iter().find(|r| r.is_canary);
    let status = match b.status.as_str() {
        "finished" => "succeeded",
        "running" if b.lease_until.is_some_and(|u| u > now) => "running",
        "running" => "paused", // 执行者已不在（崩溃/退出）：等 resume
        "attention" => "attention",
        "cancelled" => "cancelled",
        _ => "draft",
    };
    let status_word = match status {
        "succeeded" => "成功",
        "running" => "进行中",
        "paused" => "已中断（待 resume）",
        "attention" => "待介入",
        "cancelled" => "已取消",
        _ => "待确认",
    };
    Ok(json!({
        "id": b.id, "demo": false, "title": b.title, "type": "residential", "type_word": "家宽节点",
        "status": status, "status_word": status_word,
        "recipe": format!("shadowsocks-rust ssserver {} · {} :{} · nft inet ss_guard · BBR · sshd 仅密钥{}", "1.25.0", cfg.ss.method, cfg.ss.port, agent_recipe(cfg)),
        "canary": {"enabled": b.options["canary"].as_bool().unwrap_or(false), "passed": canary.is_some_and(|c| c.state == rs::ACTIVE), "node": canary.map(|c| c.spec.short_name.clone())},
        "options": {"relay_strategy": "指定/均衡", "dd_concurrency": 0, "relay_window_s": window, "concurrency": b.options["concurrency"], "dry_run": false, "canary_gate": b.options["canary_gate"]},
        "operator": b.operator, "confirmed_count": rows.len(), "plan_hash": b.plan_hash, "error": b.error,
        "started_at": b.started_at.map(|x| x / 1000), "finished_at": b.finished_at.map(|x| x / 1000),
        "elapsed_s": b.started_at.map(|s| (b.finished_at.unwrap_or(now) - s) / 1000),
        "eta_s": Value::Null,
        "phases": PHASES.iter().map(|(k, n, s)| json!({"key": k, "name": n, "steps": s})).collect::<Vec<_>>(),
        "steps": STEPS.iter().map(|(c, k, n, e)| json!({"code": c, "key": k, "name": n, "estimate": e})).collect::<Vec<_>>(),
        "rows": out_rows, "relay_groups": groups,
        "kpis": {"total": rows.len(), "done": done, "running": running, "attention": attention, "failed": failed, "eta_s": Value::Null},
    }))
}

/// 配了 [agent] 时，配方里加上 nodeagent 版本（只读 VERSION 小文件，不读二进制）。
fn agent_recipe(cfg: &Config) -> String {
    match &cfg.agent {
        Some(a) => {
            let v = std::fs::read_to_string(a.dist.join("VERSION")).map(|s| s.trim().to_string()).unwrap_or_default();
            format!(" · nodeagent {}", if v.is_empty() { "?" } else { v.as_str() })
        }
        None => String::new(),
    }
}

fn glyph(s: &str) -> &'static str {
    match s {
        cell::SUCCEEDED => "✓",
        cell::SKIPPED => "–",
        cell::RUNNING => "◐",
        cell::QUEUED => "·",
        cell::WAITING_INPUT => "⏸",
        cell::BLOCKED => "⛔",
        cell::FAILED => "✗",
        cell::COMPENSATED | "cancelled" => "↺",
        _ => "?",
    }
}

pub fn row_word(s: &str) -> &'static str {
    match s {
        rs::ACTIVE => "已上线",
        rs::RUNNING => "进行中",
        rs::QUEUED => "排队",
        rs::WAITING_GATE => "等待闸门",
        rs::READY_FOR_RELAY => "等合批",
        rs::IN_RELAY_BATCH => "合批中",
        rs::WAITING_INPUT => "等待输入",
        rs::BLOCKED => "已停止（风险）",
        rs::FAILED => "失败",
        rs::CANCELLED => "已取消",
        _ => "?",
    }
}

pub fn text(db: &Db, cfg: &Config, id: &str) -> Result<String, String> {
    let v = batch_view(db, cfg, id)?;
    let mut s = String::new();
    s.push_str(&format!("批次 {} · {} · {}（计划 {}）\n", v["id"].as_str().unwrap(), v["title"].as_str().unwrap(), v["status_word"].as_str().unwrap(), v["plan_hash"].as_str().unwrap_or("")));
    let k = &v["kpis"];
    s.push_str(&format!("成功 {} / {} · 进行中 {} · 待介入 {}\n", k["done"], k["total"], k["running"], k["attention"]));
    s.push_str(&format!("{:<10} {:<16} {:<10} {:<18} {}\n", "节点", "IP", "线路机", "P1 ………… P16", "状态 · 说明"));
    for r in v["rows"].as_array().unwrap() {
        let g: String = r["cells"].as_array().unwrap().iter().map(|c| glyph(c["status"].as_str().unwrap_or(""))).collect();
        let entry = r["relay_port"].as_i64().map(|p| format!(":{p}")).unwrap_or_default();
        let raw = db.row(r["id"].as_str().unwrap())?.map(|x| x.state).unwrap_or_default();
        let reason = r["reason"].as_str().unwrap_or("");
        let reason: String = reason.chars().take(90).collect();
        s.push_str(&format!(
            "{:<10} {:<16} {:<10} {:<18} {}{} {}\n",
            r["name"].as_str().unwrap_or(""),
            format!("{}:{}", r["ip"].as_str().unwrap_or(""), r["ssh_port"]),
            format!("{}{}", r["relay"].as_str().unwrap_or(""), entry),
            g,
            row_word(&raw),
            if r["canary"] == true { "（金丝雀）" } else { "" },
            if raw == rs::ACTIVE { String::new() } else { format!("{} {} {}", r["current_step"].as_str().unwrap_or(""), step_name(r["current_step"].as_str().unwrap_or("")), reason) }
        ));
    }
    for g in v["relay_groups"].as_array().unwrap() {
        s.push_str(&format!("线路机合批 {} 第 {} 批 [{}]：{} —— {}\n", g["relay"].as_str().unwrap_or(""), g["batch_no"], g["state"].as_str().unwrap_or(""),
            g["members"].as_array().map(|m| m.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join("、")).unwrap_or_default(), g["impact"].as_str().unwrap_or("")));
    }
    s.push_str("图例：✓成功 –跳过 ◐进行中 ·排队 ⏸等待输入 ⛔风险停止 ✗失败 ↺已补偿\n");
    Ok(s)
}
