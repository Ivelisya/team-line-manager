//! 人类可读输出：手写对齐的表格与数值格式化（中日韩宽字符按两列算）。

/// 终端显示宽度：全角/CJK 字符占两列
pub fn width(s: &str) -> usize {
    s.chars()
        .map(|c| {
            let u = c as u32;
            let wide = (0x1100..=0x115F).contains(&u)
                || (0x2E80..=0xA4CF).contains(&u)
                || (0xAC00..=0xD7A3).contains(&u)
                || (0xF900..=0xFAFF).contains(&u)
                || (0xFE30..=0xFE6F).contains(&u)
                || (0xFF00..=0xFF60).contains(&u)
                || (0xFFE0..=0xFFE6).contains(&u);
            if wide { 2 } else { 1 }
        })
        .sum()
}

fn pad(s: &str, w: usize) -> String {
    let n = width(s);
    format!("{s}{}", " ".repeat(w.saturating_sub(n)))
}

/// 单行化并按显示宽度截断（超出加 …）
pub fn clip(s: &str, max: usize) -> String {
    let s: String = s.chars().map(|c| if c == '\n' || c == '\r' || c == '\t' { ' ' } else { c }).collect();
    if width(&s) <= max {
        return s;
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = width(&c.to_string());
        if w + cw + 1 > max {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

/// 表头 + 行，列间两个空格；末列不补空格
pub fn render(headers: &[&str], rows: &[Vec<String>]) -> String {
    let cols = headers.len();
    let mut w: Vec<usize> = headers.iter().map(|h| width(h)).collect();
    let rows: Vec<Vec<String>> = rows.iter().map(|r| r.iter().map(|c| clip(c, 80)).collect()).collect();
    for r in &rows {
        for (i, c) in r.iter().enumerate().take(cols) {
            w[i] = w[i].max(width(c));
        }
    }
    let line = |cells: Vec<&str>| -> String {
        let mut s = String::new();
        for (i, c) in cells.iter().enumerate() {
            if i + 1 == cols {
                s.push_str(c);
            } else {
                s.push_str(&pad(c, w[i]));
                s.push_str("  ");
            }
        }
        s.trim_end().to_string()
    };
    let mut out = vec![line(headers.to_vec())];
    for r in &rows {
        out.push(line(r.iter().map(|c| c.as_str()).collect()));
    }
    out.join("\n")
}

/// 「名称  值」两列
pub fn kv(rows: &[(&str, String)]) -> String {
    let w = rows.iter().map(|(k, _)| width(k)).max().unwrap_or(0);
    rows.iter().map(|(k, v)| format!("{}  {}", pad(k, w), v)).collect::<Vec<_>>().join("\n")
}

pub fn bytes(n: f64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n.max(0.0);
    let mut i = 0;
    while v >= 1024.0 && i + 1 < units.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{v:.0} B") } else { format!("{v:.1} {}", units[i]) }
}

/// 速率（bit/s）
pub fn bps(n: f64) -> String {
    let units = ["bps", "Kbps", "Mbps", "Gbps"];
    let mut v = n.max(0.0);
    let mut i = 0;
    while v >= 1000.0 && i + 1 < units.len() {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 { format!("{v:.0} {}", units[0]) } else { format!("{v:.1} {}", units[i]) }
}

/// 金额（分）
pub fn money(cents: i64, cur: &str) -> String { format!("{}.{:02} {cur}", cents / 100, (cents % 100).abs()) }

/// RFC 3339 → `2026-10-20 12:34:56`（UTC，截到秒）
pub fn ts(s: &str) -> String {
    let t = s.replace('T', " ");
    t.chars().take(19).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cjk_width() {
        assert_eq!(width("abc"), 3);
        assert_eq!(width("团队1-06"), 4 + 4);
    }

    #[test]
    fn aligned_table() {
        let t = render(&["名称", "状态"], &[vec!["团队1-06".into(), "ok".into()], vec!["线路机2".into(), "down".into()]]);
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(lines.len(), 3);
        // 第二列起始的显示列一致
        let col2 = |l: &str| {
            let idx = l.find("ok").or_else(|| l.find("down")).or_else(|| l.find("状态")).unwrap();
            width(&l[..idx])
        };
        assert_eq!(col2(lines[0]), col2(lines[1]));
        assert_eq!(col2(lines[1]), col2(lines[2]));
        assert_eq!(lines[1], "团队1-06  ok");
    }

    #[test]
    fn clip_and_format() {
        assert_eq!(clip("a\nb", 10), "a b");
        assert_eq!(clip("abcdefghij", 5), "abcd…");
        assert_eq!(bytes(1536.0), "1.5 KiB");
        assert_eq!(bytes(12.0), "12 B");
        assert_eq!(bps(2_500_000.0), "2.5 Mbps");
        assert_eq!(money(1234, "USD"), "12.34 USD");
        assert_eq!(ts("2026-10-20T12:34:56.789Z"), "2026-10-20 12:34:56");
    }
}
