//! 按需测速（作业 `speedtest`）：机器自己（节点或线路机，SSH 上去）下载一个北美的公开测速文件，走机器自己的出站路径，
//! 报告平均速率、耗时、首字节时间。对线路机只做本机直测——线路机上经入口出去的探测入站只放行回显服务（agent xray/render.rs 的 probe_matchers），
//! 要经它测节点出口得改 agent，这一版不做。
//!
//! 目标是 Vultr 的 `*-ping` 测速主机（HTTPS、支持 Range），北美机房，写死在下面的列表里（不做成可配置项，也不用欧洲目标）。
//! 100 MB 的文件整个下载，25 / 500 MB 用 Range 只取前面一段（500 MB 取自 1000 MB 的文件）。Cloudflare 的 `__down` 超过 25 MB 会 403，所以不用。
//! 下载走机器的网卡，计入供应商的流量额度，确认文案写明「会用掉约 N MB 流量」。

use serde_json::{json, Value};

/// 可选的下载量（MB，十进制）
pub const SIZES_MB: [u32; 3] = [25, 100, 500];
pub const DEFAULT_MB: u32 = 100;
/// curl 自己的总时限（秒）：到点还没下完就停，按已下载的算速率并标明没下完
pub const MAX_TIME_S: u64 = 300;

pub struct Target {
    pub key: &'static str,
    pub name: &'static str,
    pub host: &'static str,
}

/// 第一个是默认目标
pub const TARGETS: &[Target] = &[
    Target { key: "sjo", name: "圣何塞", host: "sjo-ca-us-ping.vultr.com" },
    Target { key: "lax", name: "洛杉矶", host: "lax-ca-us-ping.vultr.com" },
    Target { key: "il", name: "芝加哥", host: "il-us-ping.vultr.com" },
    Target { key: "nj", name: "新泽西", host: "nj-us-ping.vultr.com" },
];

pub fn target(key: Option<&str>) -> Option<&'static Target> {
    match key {
        None => TARGETS.first(),
        Some(k) => TARGETS.iter().find(|t| t.key == k),
    }
}

/// 要请求的 (地址, Range 的末字节)：下载量 MB 按 10^6 字节算，100 MB 以内取 100MB 的文件，更大的取 1000MB 的
pub fn plan(t: &Target, size_mb: u32) -> (String, u64) {
    let file = if size_mb <= 100 { "100MB" } else { "1000MB" };
    (format!("https://{}/vultr.com.{file}.bin", t.host), size_mb as u64 * 1_000_000 - 1)
}

/// 网页 / meta 用：可选的下载量与目标
pub fn meta() -> Value {
    json!({"sizes_mb": SIZES_MB, "default_mb": DEFAULT_MB, "targets": TARGETS.iter().map(|t| json!({"key": t.key, "name": t.name})).collect::<Vec<_>>()})
}

/// 远端脚本，变量 URL、LAST（Range 末字节）、MAX_TIME。输出 `curl_rc`、`result`（curl -w 的几项）、`curl_err`
pub const SCRIPT: &str = r#"
set -u
command -v curl >/dev/null 2>&1 || { echo "curl_rc=127"; echo "curl_err=这台机器上没有 curl"; exit 0; }
E=$(mktemp); trap 'rm -f "$E"' EXIT
W=$(curl -sS -o /dev/null -r "0-$LAST" --connect-timeout 15 --max-time "$MAX_TIME" -w 'http=%{http_code} bytes=%{size_download} ttfb=%{time_starttransfer} total=%{time_total} ip=%{remote_ip}' "$URL" 2>"$E")
echo "curl_rc=$?"
echo "result=$W"
echo "curl_err=$(head -c 300 "$E" | tr '\r\n' '  ')"
"#;

/// 一次下载的结果（`result` 行里 curl 报的几项）
#[derive(Debug, PartialEq)]
pub struct Raw {
    pub rc: i32,
    pub http: u16,
    pub bytes: u64,
    pub ttfb_s: f64,
    pub total_s: f64,
    pub ip: String,
    pub err: String,
}

pub fn parse(stdout: &str) -> Raw {
    let line = |k: &str| stdout.lines().rev().find_map(|l| l.strip_prefix(k).and_then(|r| r.strip_prefix('='))).unwrap_or("").trim();
    let field = |k: &str| line("result").split_whitespace().find_map(|w| w.strip_prefix(k).and_then(|r| r.strip_prefix('='))).unwrap_or("").to_string();
    Raw {
        rc: line("curl_rc").parse().unwrap_or(-1),
        http: field("http").parse().unwrap_or(0),
        bytes: field("bytes").parse().unwrap_or(0),
        ttfb_s: field("ttfb").parse().unwrap_or(0.0),
        total_s: field("total").parse().unwrap_or(0.0),
        ip: field("ip"),
        err: line("curl_err").to_string(),
    }
}

/// 结果 → 步骤输出；失败返回给用户看的原因。超时（curl 退出码 28）但已经下了一部分的算成功、标明没下完：慢线路 500 MB 在时限内下不完
pub fn summarize(r: &Raw, t: &Target, size_mb: u32, url: &str) -> Result<Value, String> {
    let ok_http = matches!(r.http, 200 | 206);
    if r.rc == 127 || (r.rc != 0 && !(r.rc == 28 && ok_http && r.bytes > 0)) {
        let why = if r.err.is_empty() { format!("curl 退出码 {}", r.rc) } else { r.err.clone() };
        return Err(format!("下载 {} 失败：{why}", t.name));
    }
    if !ok_http || r.bytes == 0 {
        return Err(format!("{} 的测速文件返回 HTTP {}（下载了 {} 字节）：{url}", t.name, r.http, r.bytes));
    }
    // 首字节时间含 DNS、建连和 TLS 握手，速率只算收到第一个字节之后的
    let xfer = if r.total_s - r.ttfb_s > 0.05 { r.total_s - r.ttfb_s } else { r.total_s };
    let mbps = if xfer > 0.0 { r.bytes as f64 * 8.0 / 1e6 / xfer } else { 0.0 };
    Ok(json!({
        "target": t.key, "target_name": t.name, "url": url, "size_mb": size_mb, "bytes": r.bytes, "complete": r.rc == 0,
        "avg_mbps": (mbps * 10.0).round() / 10.0, "avg_mb_s": (mbps / 8.0 * 100.0).round() / 100.0,
        "total_s": (r.total_s * 100.0).round() / 100.0, "ttfb_s": (r.ttfb_s * 1000.0).round() / 1000.0, "remote_ip": r.ip,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_and_summary() {
        let t = target(None).unwrap();
        assert_eq!(t.key, "sjo");
        assert_eq!(plan(t, 25), ("https://sjo-ca-us-ping.vultr.com/vultr.com.100MB.bin".into(), 24_999_999));
        assert_eq!(plan(t, 500).0, "https://sjo-ca-us-ping.vultr.com/vultr.com.1000MB.bin");
        assert!(target(Some("fra")).is_none() && target(Some("lax")).is_some());
        let out = "curl_rc=0\nresult=http=206 bytes=100000000 ttfb=0.500 total=10.500 ip=203.0.113.9\ncurl_err=\n";
        let r = parse(out);
        assert_eq!((r.http, r.bytes, r.ip.as_str()), (206, 100_000_000, "203.0.113.9"));
        let v = summarize(&r, t, 100, "u").unwrap();
        // 100 MB ÷ 10 秒 = 80 Mbit/s
        assert_eq!((v["avg_mbps"].as_f64(), v["complete"].as_bool()), (Some(80.0), Some(true)));
        // 超时但下了一部分：成功、没下完；没下到东西的失败
        let part = parse("curl_rc=28\nresult=http=206 bytes=50000000 ttfb=1.000 total=300.000 ip=1.2.3.4\ncurl_err=Operation timed out\n");
        assert_eq!(summarize(&part, t, 500, "u").unwrap()["complete"], false);
        let none = parse("curl_rc=28\nresult=http=000 bytes=0 ttfb=0.000 total=15.000 ip=\ncurl_err=Connection timed out\n");
        assert!(summarize(&none, t, 100, "u").unwrap_err().contains("Connection timed out"));
        let forbidden = parse("curl_rc=0\nresult=http=403 bytes=190 ttfb=0.1 total=0.2 ip=1.2.3.4\ncurl_err=\n");
        assert!(summarize(&forbidden, t, 100, "u").unwrap_err().contains("HTTP 403"));
    }
}
