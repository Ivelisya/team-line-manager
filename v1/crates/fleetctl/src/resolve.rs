//! 把人/AI 写的机器、入口引用解析成确切对象：id、显示名、线路机 label、IP 都认；歧义时报错并列出候选。

use serde_json::Value;

/// 只在哪类机器里找
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Want {
    /// 未退役优先，找不到再看已退役（退役完、还没进回收站的；回收站里的用 fleetctl recycle show）
    Any,
    /// 只要未退役
    Live,
}

pub(crate) fn s<'a>(v: &'a Value, k: &str) -> &'a str { v.get(k).and_then(|x| x.as_str()).unwrap_or("") }

fn is_retired(m: &Value) -> bool { s(m, "lifecycle") == "retired" }

fn describe(m: &Value) -> String {
    let who = if s(m, "name").is_empty() { s(m, "label") } else { s(m, "name") };
    format!("{who}  {}  {}  {}", s(m, "ip"), s(m, "kind"), s(m, "lifecycle"))
}

/// 在机器列表（`GET /machines?scope=all`）里找一台
pub fn machine<'a>(list: &'a [Value], q: &str, want: Want) -> Result<&'a Value, String> {
    let q = q.trim();
    if q.is_empty() {
        return Err("机器参数为空".into());
    }
    let pool = |retired: bool| list.iter().filter(move |m| is_retired(m) == retired).collect::<Vec<_>>();
    let tiers: Vec<Vec<&Value>> = match want {
        Want::Live => vec![pool(false)],
        Want::Any => vec![pool(false), pool(true)],
    };
    for cands in &tiers {
        let hits = find_in(cands, q);
        match hits.len() {
            0 => {}
            1 => return Ok(hits[0]),
            _ => {
                let lines: Vec<String> = hits.iter().map(|m| format!("  {}  (id {})", describe(m), s(m, "id"))).collect();
                return Err(format!("「{q}」匹配到 {} 台机器，请用 id 或更精确的名字指定：\n{}", hits.len(), lines.join("\n")));
            }
        }
    }
    // 没有精确匹配：给出子串候选作提示（不自动选，避免误操作）
    let ql = q.to_lowercase();
    let near: Vec<String> = tiers
        .iter()
        .flatten()
        .filter(|m| s(m, "name").to_lowercase().contains(&ql) || s(m, "label").to_lowercase().contains(&ql) || s(m, "ip").contains(q))
        .take(8)
        .map(|m| format!("  {}  (id {})", describe(m), s(m, "id")))
        .collect();
    let scope = match want {
        Want::Live => "未退役的",
        Want::Any => "",
    };
    if near.is_empty() {
        Err(format!("没有找到{scope}机器「{q}」（可用 id、显示名如 团队1-06、线路机名、IP）"))
    } else {
        Err(format!("没有精确匹配{scope}机器「{q}」，你是不是想找：\n{}", near.join("\n")))
    }
}

/// 同一档内按 id → 显示名 → 线路机 label → IP 逐级精确匹配；某一级有结果就停
fn find_in<'a>(cands: &[&'a Value], q: &str) -> Vec<&'a Value> {
    let ql = q.to_lowercase();
    let by_id: Vec<&Value> = cands.iter().copied().filter(|m| s(m, "id").eq_ignore_ascii_case(q)).collect();
    if !by_id.is_empty() {
        return by_id;
    }
    // id 前缀（至少 6 位十六进制，且只含十六进制和 -）
    if q.len() >= 6 && q.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        let pre: Vec<&Value> = cands.iter().copied().filter(|m| s(m, "id").starts_with(&ql)).collect();
        if !pre.is_empty() {
            return pre;
        }
    }
    let tiers: [&dyn Fn(&Value) -> bool; 3] = [
        &|m| s(m, "name").to_lowercase() == ql,
        &|m| !s(m, "label").is_empty() && s(m, "label").to_lowercase() == ql,
        &|m| s(m, "ip") == q,
    ];
    for t in tiers {
        let hits: Vec<&Value> = cands.iter().copied().filter(|m| t(m)).collect();
        if !hits.is_empty() {
            return hits;
        }
    }
    vec![]
}

/// 入口引用：入口 id（可前缀）、`线路机名:端口`、节点显示名（该节点只有一个入口时）
pub fn entry<'a>(entries: &'a [Value], q: &str) -> Result<&'a Value, String> {
    let q = q.trim();
    let name = |e: &Value, who: &str| e.get(who).and_then(|n| n.get("name")).and_then(|n| n.as_str()).unwrap_or("").to_string();
    let port = |e: &Value| e.get("port").and_then(|p| p.as_i64()).unwrap_or(0);
    let id_of = |e: &Value| s(e, "id").to_lowercase();
    let ql = q.to_lowercase();
    let mut hits: Vec<&Value> = entries.iter().filter(|e| id_of(e) == ql).collect();
    if hits.is_empty() && q.len() >= 6 && q.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        hits = entries.iter().filter(|e| id_of(e).starts_with(&ql)).collect();
    }
    if hits.is_empty() {
        hits = entries.iter().filter(|e| format!("{}:{}", name(e, "relay"), port(e)) == q).collect();
    }
    if hits.is_empty() {
        hits = entries.iter().filter(|e| name(e, "node") == q).collect();
    }
    match hits.len() {
        1 => Ok(hits[0]),
        0 => Err(format!("没有找到入口「{q}」（可用入口 id、`线路机名:端口`、节点显示名；先 fleetctl entries 看列表）")),
        n => {
            let lines: Vec<String> = hits.iter().map(|e| format!("  {}:{}  → {}  (id {})", name(e, "relay"), port(e), name(e, "node"), s(e, "id"))).collect();
            Err(format!("「{q}」匹配到 {n} 个入口，请用入口 id 或 `线路机名:端口` 指定：\n{}", lines.join("\n")))
        }
    }
}

/// 目录项（供应商、套餐）的引用：id、id 前缀（至少 6 位十六进制），再按 `tiers` 逐级精确匹配（不分大小写，某一级有结果就停）。
/// 歧义时报错并用 `label` 列出候选；没有精确匹配时给出子串候选作提示，不自动选（同 `machine`）。`what` 是「供应商」「套餐」
pub fn named<'a>(all: &'a [Value], q: &str, what: &str, tiers: &[&dyn Fn(&Value) -> String], label: &dyn Fn(&Value) -> String) -> Result<&'a Value, String> {
    let q = q.trim();
    if q.is_empty() {
        return Err(format!("{what}参数为空"));
    }
    let ql = q.to_lowercase();
    let id = |v: &Value| s(v, "id").to_lowercase();
    let mut hits: Vec<&Value> = all.iter().filter(|v| id(v) == ql).collect();
    if hits.is_empty() && q.len() >= 6 && q.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        hits = all.iter().filter(|v| id(v).starts_with(&ql)).collect();
    }
    for t in tiers {
        if !hits.is_empty() {
            break;
        }
        hits = all.iter().filter(|v| t(v).to_lowercase() == ql).collect();
    }
    let line = |v: &Value| format!("  {}  (id {})", label(v), s(v, "id"));
    match hits.len() {
        1 => Ok(hits[0]),
        0 => {
            let near: Vec<String> = all.iter().filter(|v| tiers.iter().any(|t| t(v).to_lowercase().contains(&ql))).take(8).map(line).collect();
            if near.is_empty() {
                Err(format!("没有找到{what}「{q}」"))
            } else {
                Err(format!("没有精确匹配的{what}「{q}」，你是不是想找：\n{}", near.join("\n")))
            }
        }
        n => Err(format!("「{q}」匹配到 {n} 个{what}，请用 id 或更精确的写法指定：\n{}", hits.iter().map(|v| line(v)).collect::<Vec<_>>().join("\n"))),
    }
}

/// 按 id（可前缀）或名称（不分大小写）找团队
pub fn team<'a>(all: &'a [Value], q: &str) -> Result<&'a Value, String> {
    let name = |t: &Value| s(t, "name").to_string();
    named(all, q, "团队", &[&name], &name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fleet() -> Vec<Value> {
        vec![
            json!({"id": "aaaaaaaa-0000-0000-0000-000000000001", "name": "团队1-06", "label": null, "ip": "10.0.0.6", "kind": "vps", "lifecycle": "active"}),
            json!({"id": "aaaaaaaa-0000-0000-0000-000000000002", "name": "线路机2", "label": "线路机2", "ip": "10.0.0.2", "kind": "relay", "lifecycle": "active"}),
            // 已退役的旧机器占用过同一个显示名
            json!({"id": "bbbbbbbb-0000-0000-0000-000000000003", "name": "团队1-06", "label": null, "ip": "10.0.0.99", "kind": "vps", "lifecycle": "retired"}),
            json!({"id": "cccccccc-0000-0000-0000-000000000004", "name": "团队1-07", "label": null, "ip": "10.0.0.7", "kind": "vps", "lifecycle": "active"}),
            json!({"id": "cccccccc-0000-0000-0000-000000000005", "name": "团队2-01", "label": null, "ip": "10.0.0.7", "kind": "proxy", "lifecycle": "retired"}),
        ]
    }

    fn id_tail(v: &Value) -> String { v["id"].as_str().unwrap()[34..].to_string() }

    #[test]
    fn by_name_prefers_live() {
        let f = fleet();
        assert_eq!(id_tail(machine(&f, "团队1-06", Want::Any).unwrap()), "01");
        assert_eq!(id_tail(machine(&f, "10.0.0.99", Want::Any).unwrap()), "03", "未退役的里没有才看已退役的");
        assert!(machine(&f, "10.0.0.99", Want::Live).is_err());
    }

    #[test]
    fn by_label_ip_id_and_prefix() {
        let f = fleet();
        assert_eq!(id_tail(machine(&f, "线路机2", Want::Live).unwrap()), "02");
        assert_eq!(id_tail(machine(&f, "10.0.0.6", Want::Live).unwrap()), "01");
        assert_eq!(id_tail(machine(&f, "AAAAAAAA-0000-0000-0000-000000000002", Want::Live).unwrap()), "02");
        assert_eq!(id_tail(machine(&f, "bbbbbbbb", Want::Any).unwrap()), "03");
    }

    #[test]
    fn ambiguous_lists_candidates() {
        let f = fleet();
        // aaaaaaaa 前缀同时匹配两台
        let e = machine(&f, "aaaaaaaa", Want::Live).unwrap_err();
        assert!(e.contains("匹配到 2 台") && e.contains("团队1-06") && e.contains("线路机2"), "{e}");
    }

    #[test]
    fn no_fuzzy_pick_but_hints() {
        let f = fleet();
        let e = machine(&f, "团队1-0", Want::Live).unwrap_err();
        assert!(e.contains("你是不是想找") && e.contains("团队1-07"), "{e}");
        let e = machine(&f, "不存在", Want::Any).unwrap_err();
        assert!(e.contains("没有找到"), "{e}");
    }

    /// 供应商：名称那一级优先于缩写；套餐重名时用「供应商/套餐」或 id
    #[test]
    fn catalog_names() {
        let pv = vec![
            json!({"id": "01a10000-0000-7000-8000-000000000001", "name": "ATTVPS", "code": "ATT"}),
            json!({"id": "01a10000-0000-7000-8000-000000000002", "name": "ATT", "code": "AT2"}),
            json!({"id": "01b20000-0000-7000-8000-000000000003", "name": "GOMAMI", "code": "GM"}),
        ];
        let by_name = |v: &Value| s(v, "name").to_string();
        let by_code = |v: &Value| s(v, "code").to_string();
        let label = |v: &Value| format!("{}（{}）", s(v, "name"), s(v, "code"));
        let find = |q: &str| named(&pv, q, "供应商", &[&by_name, &by_code], &label).map(|v| s(v, "name").to_string());
        assert_eq!(find("gomami").unwrap(), "GOMAMI", "不分大小写");
        assert_eq!(find("gm").unwrap(), "GOMAMI", "缩写");
        assert_eq!(find("ATT").unwrap(), "ATT", "名称那一级先匹配上就不再看缩写");
        assert_eq!(find("01b20000").unwrap(), "GOMAMI", "id 前缀");
        let e = find("01a10000").unwrap_err();
        assert!(e.contains("匹配到 2 个供应商") && e.contains("ATTVPS（ATT）") && e.contains("000000000002"), "{e}");
        let e = find("VPS").unwrap_err();
        assert!(e.contains("你是不是想找") && e.contains("ATTVPS"), "{e}");
        assert!(find("不存在").unwrap_err().contains("没有找到供应商「不存在」"));
        assert!(find(" ").is_err());

        let plans = vec![
            json!({"id": "02000000-0000-7000-8000-000000000001", "name": "基础版", "pv": "ATTVPS"}),
            json!({"id": "02000000-0000-7000-8000-000000000002", "name": "基础版", "pv": "GOMAMI"}),
            json!({"id": "02000000-0000-7000-8000-000000000003", "name": "Mini", "pv": "GOMAMI"}),
        ];
        let name = |v: &Value| s(v, "name").to_string();
        let full = |v: &Value| format!("{}/{}", s(v, "pv"), s(v, "name"));
        let find = |q: &str| named(&plans, q, "套餐", &[&name, &full], &full).map(|v| s(v, "id")[34..].to_string());
        let e = find("基础版").unwrap_err();
        assert!(e.contains("匹配到 2 个套餐") && e.contains("ATTVPS/基础版") && e.contains("GOMAMI/基础版"), "{e}");
        assert_eq!(find("gomami/基础版").unwrap(), "02");
        assert_eq!(find("mini").unwrap(), "03");
    }

    #[test]
    fn entries() {
        let es = vec![
            json!({"id": "11111111-0000-0000-0000-000000000001", "port": 20001, "relay": {"name": "线路机2"}, "node": {"name": "团队1-06"}}),
            json!({"id": "11111111-0000-0000-0000-000000000002", "port": 20002, "relay": {"name": "线路机2"}, "node": {"name": "团队1-07"}}),
            json!({"id": "22222222-0000-0000-0000-000000000003", "port": 20001, "relay": {"name": "线路机3"}, "node": {"name": "团队1-07"}}),
        ];
        assert_eq!(entry(&es, "线路机2:20002").unwrap()["port"], 20002);
        assert_eq!(entry(&es, "团队1-06").unwrap()["port"], 20001);
        assert!(entry(&es, "团队1-07").unwrap_err().contains("匹配到 2 个入口"));
        assert_eq!(entry(&es, "222222").unwrap()["relay"]["name"], "线路机3");
        assert!(entry(&es, "11111111").is_err());
    }
}
