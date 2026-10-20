//! 与 Python `json.dumps` 逐字节一致的序列化。
//!
//! 旧系统所有响应都是 `json.dumps(obj, ensure_ascii=False, separators=(",", ":"))`；
//! 策略草稿指纹是 `json.dumps(v, sort_keys=True, separators=(",", ":"))`（ensure_ascii 默认 True）。
//! 两处差异都在数字和字符串：Python 的浮点数用 repr（最短可往返，指数形式 `1e-05`、`1e+16`，
//! 整数值带 `.0`），整数原样；serde_json::Number 的 is_f64 区分两者（SQLite REAL / JSON 小数 → f64）。
use serde_json::Value;

/// Python `repr(float)`。
pub fn float_repr(x: f64) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    // Rust 的 `{:e}` 给出最短可往返的有效数字（与 Python 的 dtoa mode 0 相同）
    let s = format!("{:e}", x);
    let (neg, s) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.as_str()),
    };
    let (mant, exp) = s.split_once('e').unwrap_or((s, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let digits = if digits.chars().all(|c| c == '0') { "0".to_string() } else { digits };
    let nd = digits.len() as i32;
    let decpt = exp + 1;
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    if x != 0.0 && (decpt <= -4 || decpt > 16) {
        out.push_str(&digits[..1]);
        if nd > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let e = decpt - 1;
        out.push('e');
        out.push(if e < 0 { '-' } else { '+' });
        out.push_str(&format!("{:02}", e.abs()));
    } else if decpt <= 0 {
        out.push_str("0.");
        for _ in 0..(-decpt) {
            out.push('0');
        }
        out.push_str(&digits);
    } else if decpt >= nd {
        out.push_str(&digits);
        for _ in 0..(decpt - nd) {
            out.push('0');
        }
        out.push_str(".0");
    } else {
        out.push_str(&digits[..decpt as usize]);
        out.push('.');
        out.push_str(&digits[decpt as usize..]);
    }
    out
}

fn push_str(out: &mut String, s: &str, ascii: bool) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if ascii && !(' '..='~').contains(&c) => {
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{:04x}", u));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn push_num(out: &mut String, n: &serde_json::Number) {
    if n.is_f64() {
        out.push_str(&float_repr(n.as_f64().unwrap_or(0.0)));
    } else {
        out.push_str(&n.to_string());
    }
}

fn write(out: &mut String, v: &Value, ascii: bool, sort: bool) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => push_num(out, n),
        Value::String(s) => push_str(out, s, ascii),
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write(out, x, ascii, sort);
            }
            out.push(']');
        }
        Value::Object(m) => {
            out.push('{');
            let mut keys: Vec<&String> = m.keys().collect();
            if sort {
                keys.sort();
            }
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                push_str(out, k, ascii);
                out.push(':');
                write(out, &m[k], ascii, sort);
            }
            out.push('}');
        }
    }
}

/// `json.dumps(v, ensure_ascii=False, separators=(",", ":"))`
pub fn dumps(v: &Value) -> String {
    let mut s = String::new();
    write(&mut s, v, false, false);
    s
}

/// `json.dumps(v, sort_keys=True, separators=(",", ":"))`（ensure_ascii=True）
pub fn dumps_sorted_ascii(v: &Value) -> String {
    let mut s = String::new();
    write(&mut s, v, true, true);
    s
}

/// Python `round(x, n)`（n ≥ 0）：按精确二进制值做银行家舍入后取最近浮点数。
pub fn py_round(x: f64, n: usize) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{:.*}", n, x).parse().unwrap_or(x)
}

/// Python `round(x)` → int（银行家舍入）
pub fn py_round_int(x: f64) -> i64 {
    let r = x.round();
    if (x - x.trunc()).abs() == 0.5 {
        let t = x.trunc();
        return if (t as i64) % 2 == 0 { t as i64 } else { r as i64 };
    }
    r as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn floats() {
        for (x, s) in [
            (0.0, "0.0"), (-0.0, "-0.0"), (1.0, "1.0"), (0.1, "0.1"), (1e-5, "1e-05"), (0.0001, "0.0001"),
            (1e16, "1e+16"), (1e15, "1000000000000000.0"), (123456789012345678.0, "1.2345678901234568e+17"),
            (1790148138.4224386, "1790148138.4224386"), (2.5e-7, "2.5e-07"), (1.5, "1.5"), (100.0, "100.0"),
            (0.30000000000000004, "0.30000000000000004"), (-12.25, "-12.25"), (9999999999999998.0, "9999999999999998.0"),
        ] {
            assert_eq!(float_repr(x), s, "{x}");
        }
    }

    #[test]
    fn strings_and_structure() {
        let v = json!({"b": "中\"\n\u{1}", "a": [1, 2.0, null, true]});
        assert_eq!(dumps(&v), "{\"b\":\"中\\\"\\n\\u0001\",\"a\":[1,2.0,null,true]}");
        assert_eq!(dumps_sorted_ascii(&v), "{\"a\":[1,2.0,null,true],\"b\":\"\\u4e2d\\\"\\n\\u0001\"}");
        assert_eq!(dumps_sorted_ascii(&json!("😀")), "\"\\ud83d\\ude00\"");
    }

    #[test]
    fn rounding() {
        assert_eq!(py_round(0.125, 2), 0.12);
        assert_eq!(py_round(2.675, 2), 2.67);
        assert_eq!(py_round(99.99999, 2), 100.0);
        assert_eq!(py_round_int(2.5), 2);
        assert_eq!(py_round_int(3.5), 4);
        assert_eq!(py_round_int(-2.5), -2);
        assert_eq!(py_round_int(2.4), 2);
    }
}
