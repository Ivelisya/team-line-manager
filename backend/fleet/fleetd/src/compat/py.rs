//! 移植旧系统（Python）读路径时用到的小工具：Python 真值、int/float 保型算术、SQLite 行 → JSON、
//! 参数的 `int()` / `float()` 语义与错误文案。
use rusqlite::types::{Value as SqlVal, ValueRef};
use rusqlite::Connection;
use serde_json::{Map, Number, Value};

/// 旧 Handler 的状态映射：ValueError/TypeError → 400（原文），KeyError → 404，其余 → 500
#[derive(Debug)]
pub enum Err {
    Bad(String),
    NotFound,
    Internal(String),
}

pub type R<T> = Result<T, Err>;

impl From<rusqlite::Error> for Err {
    fn from(e: rusqlite::Error) -> Self {
        Err::Internal(e.to_string())
    }
}

pub fn f(x: f64) -> Value {
    Number::from_f64(x).map(Value::Number).unwrap_or(Value::Null)
}

pub fn i(x: i64) -> Value {
    Value::Number(Number::from(x))
}

pub fn is_num(v: &Value) -> bool {
    matches!(v, Value::Number(_))
}

pub fn is_float(v: &Value) -> bool {
    matches!(v, Value::Number(n) if n.is_f64())
}

pub fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

pub fn int_of(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) if !n.is_f64() => n.as_i64(),
        _ => None,
    }
}

pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|x| x != 0.0).unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(m) => !m.is_empty(),
    }
}

/// Python `a + b`（数字）：两边都是 int 时仍是 int
pub fn add(a: &Value, b: &Value) -> Value {
    match (int_of(a), int_of(b)) {
        (Some(x), Some(y)) => i(x + y),
        _ => f(num(a).unwrap_or(0.0) + num(b).unwrap_or(0.0)),
    }
}

/// Python `sum(values)`（从 int 0 开始、自左向右累加）
pub fn sum<'a>(it: impl IntoIterator<Item = &'a Value>) -> Value {
    it.into_iter().fold(i(0), |acc, v| add(&acc, v))
}

/// Python 数值比较（int 与 float 按数值）
pub fn cmp(a: &Value, b: &Value) -> std::cmp::Ordering {
    match (int_of(a), int_of(b)) {
        (Some(x), Some(y)) => x.cmp(&y),
        _ => num(a).unwrap_or(0.0).partial_cmp(&num(b).unwrap_or(0.0)).unwrap_or(std::cmp::Ordering::Equal),
    }
}

/// Python `max(a, b)`：相等时返回第一个
pub fn max2(a: Value, b: Value) -> Value {
    if cmp(&b, &a) == std::cmp::Ordering::Greater {
        b
    } else {
        a
    }
}

/// `dict.get(key)` → 缺省为 null
pub fn get<'a>(v: &'a Value, key: &str) -> &'a Value {
    static NULL: Value = Value::Null;
    v.get(key).unwrap_or(&NULL)
}

/// `dict.get(key, default)`：键存在（哪怕是 null）就取值
pub fn get_or(v: &Value, key: &str, default: Value) -> Value {
    match v.as_object().and_then(|m| m.get(key)) {
        Some(x) => x.clone(),
        None => default,
    }
}

pub fn obj() -> Map<String, Value> {
    Map::new()
}

fn py_repr_str(s: &str) -> String {
    let q = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::new();
    out.push(q);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == q => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push(q);
    out
}

/// Python `int(str)`（10 进制；允许首尾空白、正负号、数字间单个下划线）
pub fn py_int(s: &str) -> R<i64> {
    let t = s.trim();
    let (neg, body) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let ok = !body.is_empty()
        && body.chars().all(|c| c.is_ascii_digit() || c == '_')
        && !body.starts_with('_')
        && !body.ends_with('_')
        && !body.contains("__");
    let n = if ok { body.replace('_', "").parse::<i64>().ok() } else { None };
    match n {
        Some(n) => Ok(if neg { -n } else { n }),
        None => Err(Err::Bad(format!("invalid literal for int() with base 10: {}", py_repr_str(s)))),
    }
}

/// Python `float(str)`
pub fn py_float(s: &str) -> R<f64> {
    let t = s.trim();
    let low = t.to_ascii_lowercase();
    let v = match low.trim_start_matches(['+', '-']) {
        "inf" | "infinity" => Some(if low.starts_with('-') { f64::NEG_INFINITY } else { f64::INFINITY }),
        "nan" => Some(f64::NAN),
        _ => {
            let clean = if t.contains('_') && !t.contains("__") { t.replace('_', "") } else { t.to_string() };
            clean.parse::<f64>().ok().filter(|_| !t.is_empty())
        }
    };
    v.ok_or_else(|| Err::Bad(format!("could not convert string to float: {}", py_repr_str(s))))
}

pub type Params = std::collections::HashMap<String, String>;

/// `int(params.get(key, default))`
pub fn p_int(p: &Params, key: &str, default: i64) -> R<i64> {
    match p.get(key) {
        Some(s) => py_int(s),
        None => Ok(default),
    }
}

pub fn p_truthy<'a>(p: &'a Params, key: &str) -> Option<&'a str> {
    p.get(key).map(|s| s.as_str()).filter(|s| !s.is_empty())
}

pub fn sql_to_json(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(x) => i(x),
        ValueRef::Real(x) => f(x),
        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::String(String::from_utf8_lossy(b).into_owned()),
    }
}

/// `[dict(r) for r in db.execute(sql, args)]`
pub fn rows(db: &Connection, sql: &str, args: &[SqlVal]) -> R<Vec<Map<String, Value>>> {
    let mut st = db.prepare_cached(sql)?;
    let names: Vec<String> = st.column_names().iter().map(|s| s.to_string()).collect();
    let mut q = st.query(rusqlite::params_from_iter(args.iter()))?;
    let mut out = vec![];
    while let Some(r) = q.next()? {
        let mut m = Map::new();
        for (k, name) in names.iter().enumerate() {
            m.insert(name.clone(), sql_to_json(r.get_ref(k)?));
        }
        out.push(m);
    }
    Ok(out)
}

pub fn scalar(db: &Connection, sql: &str, args: &[SqlVal]) -> R<Value> {
    let mut st = db.prepare_cached(sql)?;
    let mut q = st.query(rusqlite::params_from_iter(args.iter()))?;
    match q.next()? {
        Some(r) => Ok(sql_to_json(r.get_ref(0)?)),
        None => Ok(Value::Null),
    }
}

/// `json.loads(text)`；解析失败按 ValueError（400）
pub fn loads(text: &Value) -> R<Value> {
    match text {
        Value::String(s) => serde_json::from_str(s).map_err(|e| Err::Bad(format!("Expecting value: {e}"))),
        _ => Err(Err::Bad("the JSON object must be str, bytes or bytearray, not NoneType".into())),
    }
}

/// `store.get_meta(key, default)`
pub fn meta(db: &Connection, key: &str, default: Value) -> R<Value> {
    match scalar(db, "SELECT value FROM meta WHERE key=?", &[SqlVal::Text(key.into())])? {
        Value::Null => Ok(default),
        Value::String(s) if s.len() >= 4096 => {
            // 大的 meta（主机快照等）：同一原文只解析一次（按原文哈希缓存解析结果）
            use std::hash::{Hash, Hasher};
            static CACHE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, (u64, usize, Value)>>> = std::sync::OnceLock::new();
            let mut h = std::collections::hash_map::DefaultHasher::new();
            s.hash(&mut h);
            let hv = h.finish();
            let c = CACHE.get_or_init(Default::default);
            if let Some((ch, cl, v)) = c.lock().unwrap().get(key) {
                if *ch == hv && *cl == s.len() {
                    return Ok(v.clone());
                }
            }
            let v = loads(&Value::String(s.clone()))?;
            let mut g = c.lock().unwrap();
            if g.len() > 64 {
                g.clear();
            }
            g.insert(key.to_string(), (hv, s.len(), v.clone()));
            Ok(v)
        }
        v => loads(&v),
    }
}

pub fn sf(x: f64) -> SqlVal {
    SqlVal::Real(x)
}

pub fn si(x: i64) -> SqlVal {
    SqlVal::Integer(x)
}

pub fn st(s: &str) -> SqlVal {
    SqlVal::Text(s.to_string())
}

/// Python `x // n`（浮点）→ int
pub fn floor_div_int(x: &Value, n: i64) -> i64 {
    match int_of(x) {
        Some(v) => v.div_euclid(n),
        None => (num(x).unwrap_or(0.0) / n as f64).floor() as i64,
    }
}

/// Python 的 `str[:n]`（按字符）
pub fn head(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ints() {
        assert_eq!(py_int(" 24 ").unwrap(), 24);
        assert_eq!(py_int("-1_000").unwrap(), -1000);
        match py_int("2.5") {
            Err(Err::Bad(m)) => assert_eq!(m, "invalid literal for int() with base 10: '2.5'"),
            _ => panic!(),
        }
        assert!(py_int("").is_err());
        assert_eq!(py_float("1e3").unwrap(), 1000.0);
    }
    #[test]
    fn arith() {
        assert_eq!(add(&i(1), &i(2)), i(3));
        assert!(is_float(&add(&i(1), &f(2.0))));
        assert_eq!(sum([&i(1), &i(2)]), i(3));
        assert_eq!(max2(i(1), f(1.0)), i(1));
    }
}
