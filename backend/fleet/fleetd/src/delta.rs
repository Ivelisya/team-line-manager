//! overview / health 的增量（给界面按补丁更新，不必每次 SSE 变化都重取整份 200+ KB）。
//!
//! 轮询器每取到一份变化了的 overview / health，就按 SSE 的版本号存一份快照（每个资源最近 8 份）。
//! `GET /v2/api/v1/delta/{overview|health}?since=<版本>`：
//! - since = 当前版本 → `{"version":v,"full":false,"patch":{}}`；
//! - since 在保留的快照里 → `{"version":v,"full":false,"patch":<补丁>}`；
//! - 否则（太旧 / 服务重启过 / 没带 since）→ `{"version":v,"full":true,"data":<整份>}`。
//!
//! 补丁格式（JSON Merge Patch 的变体，见 API.md §12.5）：
//! - 对象：只列出变化的键；值是对象时递归为子补丁（应用到原值上，原值不是对象就当 {}）；其他值（含 null、数组）直接替换；
//!   被删除的键列在 `"$del": [...]`。
//! - 元素全是带字符串 `uuid` 的对象的数组（overview.assets）：`{"$uuid": {"<uuid>": 子补丁或新元素}, "$removed": [...], "$order": [...]}`，
//!   `$order` 只在顺序或成员变化时给出（按它重排）。
use serde_json::{json, Map, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

const KEEP: usize = 8;
/// 补丁缓存上限（(资源, since, 当前版本) → 补丁）
const DIFFS_MAX: usize = 16;

/// 每个资源最近 KEEP 份：旧版本只存紧凑 JSON 文本（一份 overview 约 250 KB；解析成 Value 要大好几倍），
/// 最新一份另存解析好的 Value（做补丁与整份返回用）。求补丁时才把 since 那一份解析回来（float_roundtrip：与原值逐位相同）。
#[derive(Default)]
struct Kept {
    versions: VecDeque<(u64, Arc<str>)>,
    latest: Option<(u64, Arc<Value>)>,
}

#[derive(Default)]
pub struct Snapshots {
    inner: Mutex<HashMap<String, Kept>>,
    diffs: Mutex<HashMap<(String, u64, u64), Arc<Value>>>,
}

impl Snapshots {
    pub fn put(&self, res: &str, version: u64, v: Value) {
        let text: Arc<str> = Arc::from(v.to_string());
        let mut g = self.inner.lock().unwrap();
        let k = g.entry(res.to_string()).or_default();
        if k.versions.back().is_some_and(|x| x.0 >= version) {
            // 版本没前进（例如同一版本重复写）：替换最后一份
            k.versions.pop_back();
        }
        k.versions.push_back((version, text));
        while k.versions.len() > KEEP {
            k.versions.pop_front();
        }
        k.latest = Some((version, Arc::new(v)));
    }

    pub fn latest(&self, res: &str) -> Option<(u64, Arc<Value>)> {
        self.inner.lock().unwrap().get(res).and_then(|k| k.latest.clone())
    }

    /// 内存读数：每个资源保留的份数与字节数
    pub fn mem_stats(&self) -> Value {
        let g = self.inner.lock().unwrap();
        let res: Map<String, Value> = g
            .iter()
            .map(|(r, k)| (r.clone(), json!({"kept": k.versions.len(), "json_bytes": k.versions.iter().map(|x| x.1.len()).sum::<usize>()})))
            .collect();
        let d = self.diffs.lock().unwrap();
        json!({"resources": res, "diffs": d.len(), "diff_json_bytes": d.values().map(|v| v.to_string().len()).sum::<usize>()})
    }

    pub fn clear(&self) {
        self.inner.lock().unwrap().clear();
        self.diffs.lock().unwrap().clear();
    }

    /// (版本, full, 内容)
    pub fn delta(&self, res: &str, since: Option<u64>) -> Option<Value> {
        let (cur, new, old_text) = {
            let g = self.inner.lock().unwrap();
            let k = g.get(res)?;
            let (cv, cval) = k.latest.clone()?;
            let old = since.filter(|s| *s != cv).and_then(|s| k.versions.iter().find(|x| x.0 == s).map(|x| x.1.clone()));
            (cv, cval, old)
        };
        match (since, old_text) {
            (Some(s), _) if s == cur => Some(json!({"version": cur, "full": false, "patch": {}})),
            (Some(s), Some(old_text)) => {
                let key = (res.to_string(), s, cur);
                if let Some(p) = self.diffs.lock().unwrap().get(&key) {
                    return Some(json!({"version": cur, "full": false, "patch": (**p).clone()}));
                }
                let Ok(old) = serde_json::from_str::<Value>(&old_text) else {
                    return Some(json!({"version": cur, "full": true, "data": (*new).clone()}));
                };
                let p = Arc::new(diff(&old, &new).unwrap_or_else(|| json!({})));
                let mut d = self.diffs.lock().unwrap();
                if d.len() >= DIFFS_MAX {
                    // 只留指向当前版本的补丁（旧的当前版本不会再被问到）
                    d.retain(|k, _| k.2 == cur);
                    if d.len() >= DIFFS_MAX {
                        d.clear();
                    }
                }
                d.insert(key, p.clone());
                Some(json!({"version": cur, "full": false, "patch": (*p).clone()}))
            }
            _ => Some(json!({"version": cur, "full": true, "data": (*new).clone()})),
        }
    }
}

fn uuid_array(v: &Value) -> Option<Vec<(&str, &Value)>> {
    let a = v.as_array()?;
    a.iter().map(|x| x.get("uuid").and_then(|u| u.as_str()).map(|u| (u, x))).collect()
}

/// old → new 的补丁；无变化 → None
pub fn diff(old: &Value, new: &Value) -> Option<Value> {
    if old == new {
        return None;
    }
    match (old, new) {
        (Value::Object(o), Value::Object(n)) => {
            let mut p = Map::new();
            for (k, nv) in n {
                match o.get(k) {
                    Some(ov) => {
                        if let Some(d) = diff(ov, nv) {
                            p.insert(k.clone(), d);
                        }
                    }
                    None => {
                        p.insert(k.clone(), nv.clone());
                    }
                }
            }
            let del: Vec<Value> = o.keys().filter(|k| !n.contains_key(*k)).map(|k| json!(k)).collect();
            if !del.is_empty() {
                p.insert("$del".into(), Value::Array(del));
            }
            if p.is_empty() {
                None
            } else {
                Some(Value::Object(p))
            }
        }
        (Value::Array(_), Value::Array(_)) => match (uuid_array(old), uuid_array(new)) {
            (Some(oa), Some(na)) if !na.is_empty() || !oa.is_empty() => {
                let om: HashMap<&str, &Value> = oa.iter().copied().collect();
                let mut up = Map::new();
                for (u, nv) in &na {
                    match om.get(u) {
                        Some(ov) => {
                            if let Some(d) = diff(ov, nv) {
                                up.insert(u.to_string(), d);
                            }
                        }
                        None => {
                            up.insert(u.to_string(), (*nv).clone());
                        }
                    }
                }
                let nset: std::collections::HashSet<&str> = na.iter().map(|x| x.0).collect();
                let removed: Vec<Value> = oa.iter().filter(|x| !nset.contains(x.0)).map(|x| json!(x.0)).collect();
                let mut p = Map::new();
                p.insert("$uuid".into(), Value::Object(up));
                if !removed.is_empty() {
                    p.insert("$removed".into(), Value::Array(removed));
                }
                let oo: Vec<&str> = oa.iter().map(|x| x.0).collect();
                let no: Vec<&str> = na.iter().map(|x| x.0).collect();
                if oo != no {
                    p.insert("$order".into(), json!(no));
                }
                Some(Value::Object(p))
            }
            _ => Some(new.clone()),
        },
        _ => Some(new.clone()),
    }
}

/// 补丁应用（测试与文档用；界面按同样规则实现）
pub fn apply(target: &Value, patch: &Value) -> Value {
    let Some(p) = patch.as_object() else { return patch.clone() };
    if let Some(up) = p.get("$uuid").and_then(|x| x.as_object()) {
        let mut items: Vec<Value> = target.as_array().cloned().unwrap_or_default();
        let removed: Vec<&str> = p.get("$removed").and_then(|x| x.as_array()).map(|a| a.iter().filter_map(|x| x.as_str()).collect()).unwrap_or_default();
        items.retain(|x| !x.get("uuid").and_then(|u| u.as_str()).is_some_and(|u| removed.contains(&u)));
        for (u, sub) in up {
            match items.iter_mut().find(|x| x.get("uuid").and_then(|v| v.as_str()) == Some(u.as_str())) {
                Some(x) => *x = apply(x, sub),
                None => items.push(apply(&json!({}), sub)),
            }
        }
        if let Some(order) = p.get("$order").and_then(|x| x.as_array()) {
            let pos: HashMap<&str, usize> = order.iter().enumerate().filter_map(|(i, u)| u.as_str().map(|u| (u, i))).collect();
            items.sort_by_key(|x| x.get("uuid").and_then(|u| u.as_str()).and_then(|u| pos.get(u).copied()).unwrap_or(usize::MAX));
        }
        return Value::Array(items);
    }
    let mut t = target.as_object().cloned().unwrap_or_default();
    for (k, v) in p {
        if k == "$del" {
            continue;
        }
        let nv = if v.is_object() { apply(t.get(k).filter(|x| x.is_object() || v.get("$uuid").is_some()).unwrap_or(&json!({})), v) } else { v.clone() };
        t.insert(k.clone(), nv);
    }
    if let Some(del) = p.get("$del").and_then(|x| x.as_array()) {
        for k in del.iter().filter_map(|x| x.as_str()) {
            t.remove(k);
        }
    }
    Value::Object(t)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip() {
        let a = json!({"now": 1.0, "assets": [{"uuid": "a", "x": 1, "h": {"cpu": 1, "mem": 2}}, {"uuid": "b", "x": 2}], "relays": {"r1": {"ok": true}}, "gone": 1, "n": null});
        let b = json!({"now": 2.0, "assets": [{"uuid": "b", "x": 3}, {"uuid": "c", "x": 9}, {"uuid": "a", "x": 1, "h": {"cpu": 5}}], "relays": {"r1": {"ok": false}, "r2": {}}, "n": 5, "z": null});
        let p = diff(&a, &b).unwrap();
        assert_eq!(apply(&a, &p), b);
        assert!(p["assets"]["$order"].is_array());
        assert_eq!(p["assets"]["$uuid"]["a"], json!({"h": {"cpu": 5, "$del": ["mem"]}}));
        assert_eq!(p["$del"], json!(["gone"]));
        assert!(diff(&b, &b).is_none());
        let s = Snapshots::default();
        s.put("overview", 3, a.clone());
        s.put("overview", 4, b.clone());
        let d = s.delta("overview", Some(3)).unwrap();
        assert_eq!(d["full"], false);
        assert_eq!(apply(&a, &d["patch"]), b);
        assert_eq!(s.delta("overview", Some(1)).unwrap()["full"], true);
        assert_eq!(s.delta("overview", Some(4)).unwrap()["patch"], json!({}));
    }

    /// 旧版本存成文本后再解析：浮点逐位不变（补丁里不会冒出假变化），超过 KEEP 份退回整份
    #[test]
    fn text_kept_versions_roundtrip() {
        let s = Snapshots::default();
        let mk = |i: u64| json!({"now": 1790190000.923 + i as f64 * 0.1, "x": 0.1 + 0.2, "big": 12345678901234567u64, "neg": -1.5e-7,
            "assets": [{"uuid": "a", "cpu": 3.3333333333333335, "i": i}]});
        for v in 1..=10u64 {
            s.put("overview", v, mk(v));
        }
        let d = s.delta("overview", Some(9)).unwrap();
        assert_eq!(d["full"], false);
        assert_eq!(d["patch"], json!({"now": mk(10)["now"], "assets": {"$uuid": {"a": {"i": 10}}}}), "只有真变化的键");
        assert_eq!(apply(&mk(9), &d["patch"]), mk(10));
        assert_eq!(apply(&mk(3), &s.delta("overview", Some(3)).unwrap()["patch"]), mk(10));
        assert_eq!(s.delta("overview", Some(2)).unwrap()["full"], true, "只留最近 8 份");
        assert_eq!(s.delta("overview", None).unwrap()["data"], mk(10));
        assert_eq!(s.latest("overview").unwrap().0, 10);
    }
}
