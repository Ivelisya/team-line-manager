//! 来源 IP 画像：查单个 / 多个 IP、节点的来源画像分布、离线库状态。

use crate::cmds::{st, Ctx};
use crate::resolve::Want;
use crate::table;
use serde_json::Value;

fn i(v: &Value, ptr: &str) -> String {
    v.pointer(ptr).and_then(Value::as_i64).map(|n| n.to_string()).unwrap_or_else(|| "-".into())
}

fn flags(b: &Value) -> String {
    let mut f = vec![];
    if let Some(n) = b["internal"].as_str() {
        f.push(format!("我方 {n}"));
    }
    if b["private"].as_bool() == Some(true) {
        f.push("非公网".into());
    }
    if f.is_empty() { String::new() } else { f.join("，") }
}

fn asn_text(b: &Value) -> String {
    match b["asn"].as_u64() {
        Some(a) => format!("AS{a} {}", b["as_org"].as_str().unwrap_or("")).trim().to_string(),
        None => "-".into(),
    }
}

/// `fleetctl ip <ip...>`：一个 IP 给详细画像（含判定理由），多个给表格
pub fn ip(ctx: &Ctx, ips: &[String]) -> anyhow::Result<u8> {
    if ips.is_empty() {
        anyhow::bail!("请给出至少一个 IP");
    }
    if ips.len() == 1 {
        let d = ctx.c.get(&format!("/ipintel/{}", ips[0].trim()))?;
        ctx.emit(&d, || {
            let place = [st(&d, "/region"), st(&d, "/city")].into_iter().filter(|s| s != "-").collect::<Vec<_>>().join(" ");
            let mut rows = vec![
                ("地址", st(&d, "/ip")),
                ("运营商", st(&d, "/isp")),
                ("ASN", asn_text(&d)),
                ("国家 / 地点", format!("{} {}", st(&d, "/country"), place).trim().to_string()),
                ("接入方式", format!("{}（置信度 {}）", st(&d, "/type_word"), st(&d, "/type_confidence"))),
                ("判定理由", st(&d, "/type_reason")),
                ("数据来源", d["sources"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("、")).unwrap_or_default()),
            ];
            let fl: Vec<&str> = d["flags"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
            if !fl.is_empty() {
                rows.push(("标记", fl.join("、")));
            }
            if d["degraded"].as_bool() == Some(true) {
                rows.push(("提示", format!("离线库只加载了 {}/5 个，部分字段查不到（fleetctl ipintel-status 看详情）", st(&d, "/loaded"))));
            }
            table::kv(&rows)
        });
        return Ok(0);
    }
    let mut items: Vec<Value> = vec![];
    for chunk in ips.chunks(200) {
        let d = ctx.c.get_q("/ipintel", &[("ips", chunk.iter().map(|s| s.trim()).collect::<Vec<_>>().join(","))])?;
        items.extend(d["items"].as_array().cloned().unwrap_or_default());
    }
    let data = Value::Array(items.clone());
    ctx.emit(&data, || {
        let rows: Vec<Vec<String>> = items
            .iter()
            .map(|it| {
                let b = &it["intel"];
                if b.is_null() {
                    let note = it["error"].as_str().unwrap_or("查不到（离线库没有这个地址，或库未安装）");
                    return vec![st(it, "/ip"), "-".into(), "-".into(), "-".into(), "-".into(), note.to_string()];
                }
                vec![st(it, "/ip"), st(b, "/isp"), st(b, "/place"), st(b, "/conn_label"), st(b, "/confidence"), flags(b)]
            })
            .collect();
        table::render(&["IP", "运营商", "地点", "接入方式", "置信度", "备注"], &rows)
    });
    Ok(0)
}

fn dist_table(d: &Value, title: &str, top: usize) -> String {
    let list = d.as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        return format!("{title}：（无）");
    }
    let rows: Vec<Vec<String>> = list
        .iter()
        .take(top)
        .map(|x| vec![st(x, "/label"), i(x, "/conns"), format!("{}%", x["pct"].as_f64().map(|p| format!("{p:.1}")).unwrap_or_else(|| "-".into())), i(x, "/sources")])
        .collect();
    format!("{title}\n{}", table::render(&["值", "连接", "占比", "来源数"], &rows))
}

/// `fleetctl sources <机器> [--window 7d]`
pub fn sources(ctx: &Ctx, machine: &str, window: &str, limit: i64) -> anyhow::Result<u8> {
    let (id, name) = ctx.find_id(machine, Want::Any)?;
    let d = ctx.c.get_q(&format!("/machines/{id}/sources"), &[("window", window.to_string()), ("limit", limit.to_string())])?;
    ctx.emit(&d, || {
        let mut out = vec![];
        let l = &d["learning"];
        let learn = if l["started_at"].is_null() {
            "还没有来源记录".to_string()
        } else if l["done"].as_bool() == Some(true) {
            format!("学习期已结束（自 {} 起 {} 天）", table::ts(&st(l, "/started_at")), i(l, "/learn_days"))
        } else {
            format!("学习中，还剩约 {} 小时（自 {} 起 {} 天；期间只学习不告警）", i(l, "/remaining_hours"), table::ts(&st(l, "/started_at")), i(l, "/learn_days"))
        };
        out.push(format!(
            "{name}  窗口 {}（自 {} 起）：外部来源 {} 个、连接 {}；我方 {}、非公网 {}\n{learn}",
            st(&d, "/window"),
            table::ts(&st(&d, "/window_start")),
            i(&d, "/total_sources"),
            i(&d, "/total_conns"),
            i(&d, "/internal_conns"),
            i(&d, "/private_conns")
        ));
        if d["degraded"].as_bool() == Some(true) {
            out.push("提示：IP 库未安装或不全，画像查不到（在中控运行 lab/install-ipintel.sh；fleetctl ipintel-status 看详情）".into());
        }
        if d["empty"].as_bool() == Some(true) {
            out.push("这段时间没有来源记录".into());
            return out.join("\n");
        }
        for (k, title) in [("isp", "运营商"), ("city", "城市"), ("conn_type", "接入方式")] {
            out.push(dist_table(&d["distribution"][k], title, 10));
        }
        let top: Vec<Vec<String>> = d["top_sources"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|s| {
                let b = &s["intel"];
                vec![st(s, "/src_ip"), i(s, "/conns"), st(b, "/isp"), st(b, "/place"), st(b, "/conn_label"), flags(b)]
            })
            .collect();
        out.push(format!("来源 IP\n{}", table::render(&["IP", "连接", "运营商", "地点", "接入方式", "备注"], &top)));
        let an = d["anomalies"].as_array().cloned().unwrap_or_default();
        if !an.is_empty() {
            let rows: Vec<Vec<String>> = an.iter().map(|a| vec![st(a, "/rule_name"), st(a, "/summary"), table::ts(&st(a, "/opened_at"))]).collect();
            out.push(format!("进行中的画像异常\n{}", table::render(&["类型", "摘要", "开始(UTC)"], &rows)));
        }
        out.join("\n\n")
    });
    Ok(0)
}

/// `fleetctl ipintel-status`
pub fn status(ctx: &Ctx) -> anyhow::Result<u8> {
    let d = ctx.c.get("/ipintel/status")?;
    ctx.emit(&d, || {
        let rows: Vec<Vec<String>> = d["files"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|f| {
                if f["loaded"].as_bool() == Some(true) {
                    let size = f["bytes"].as_f64().map(table::bytes).unwrap_or_default();
                    vec![st(f, "/id"), "已加载".into(), size, st(f, "/build")]
                } else {
                    vec![st(f, "/id"), "未加载".into(), "-".into(), "-".into()]
                }
            })
            .collect();
        let mut out = vec![format!(
            "IP 情报：{}  已加载 {}/{} 个库  缓存 {} 条\n库目录 {}\n版本 {}",
            if d["enabled"].as_bool() == Some(true) { "启用" } else { "已关闭" },
            i(&d, "/loaded"),
            i(&d, "/expected"),
            i(&d, "/cache_rows"),
            st(&d, "/db_dir"),
            st(&d, "/version")
        )];
        out.push(table::render(&["库", "状态", "大小", "版本"], &rows));
        if let Some(e) = d["errors"].as_array().filter(|a| !a.is_empty()) {
            out.push(format!("加载问题：\n  {}", e.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n  ")));
        }
        let ov = &d["overrides"];
        out.push(format!(
            "ASN 清单：{}；覆盖文件 {}（{}）",
            st(&d, "/lists"),
            st(ov, "/path"),
            if let Some(e) = ov["error"].as_str() { format!("无效：{e}") } else if ov["used"].as_bool() == Some(true) { "已生效".into() } else { "未使用".to_string() }
        ));
        if let Some(h) = d["hint"].as_str() {
            out.push(format!("提示：{h}"));
        }
        out.join("\n")
    });
    Ok(0)
}
