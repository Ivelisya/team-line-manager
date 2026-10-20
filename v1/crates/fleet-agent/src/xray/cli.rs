//! 调用 `xray api …` / `xray run -test` / `xray version`。
//! 子进程带超时、kill_on_drop，输出全部读完，保证回收。

use super::render::tag_of;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;
use tokio::process::Command;

const API_TIMEOUT: Duration = Duration::from_secs(15);
const TEST_TIMEOUT: Duration = Duration::from_secs(90);

/// 对 Xray 运行态的一次一次性快照
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Runtime {
    pub inbounds: Vec<String>,
    pub outbounds: Vec<String>,
    /// (ruleTag, 出站 tag)，按匹配顺序
    pub rules: Vec<(String, String)>,
}

/// 一个对 Xray 运行态的原子操作
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    AddOutbound(Value),
    RemoveOutbound(String),
    AddInbound(Value),
    RemoveInbound(String),
    /// 整表替换路由规则（原子，保证顺序）
    SetRules(Vec<Value>),
}

impl Op {
    pub fn describe(&self) -> String {
        match self {
            Op::AddOutbound(v) => format!("ado {}", tag_of(v)),
            Op::RemoveOutbound(t) => format!("rmo {t}"),
            Op::AddInbound(v) => format!("adi {}", tag_of(v)),
            Op::RemoveInbound(t) => format!("rmi {t}"),
            Op::SetRules(r) => format!("adrules ({} 条)", r.len()),
        }
    }
}

#[derive(Clone)]
pub struct XrayCli {
    pub bin: PathBuf,
    pub api: String,
    pub asset_dir: PathBuf,
    pub tmp_dir: PathBuf,
}

// ── 输出解析（纯函数）────────────────────────────────────────────────────────

pub fn parse_tags(out: &str, key: &str) -> Result<Vec<String>> {
    let v: Value = serde_json::from_str(out).with_context(|| format!("解析 {key} 输出"))?;
    Ok(v.get(key)
        .and_then(|a| a.as_array())
        .map(|a| a.iter().filter_map(|x| x.get("tag").and_then(|t| t.as_str())).map(String::from).collect())
        .unwrap_or_default())
}

pub fn parse_rules(out: &str) -> Result<Vec<(String, String)>> {
    let v: Value = serde_json::from_str(out).context("解析 lsrules 输出")?;
    Ok(v.get("rules")
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .map(|x| {
                    let g = |k| x.get(k).and_then(|t| t.as_str()).unwrap_or("").to_string();
                    (g("ruleTag"), g("tag"))
                })
                .collect()
        })
        .unwrap_or_default())
}

/// statsquery：值为 0 的计数器 Xray 会省略 value 字段
pub fn parse_stats(out: &str) -> Result<Vec<(String, u64)>> {
    let v: Value = serde_json::from_str(out).context("解析 statsquery 输出")?;
    Ok(v.get("stat")
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| {
                    let n = x.get("name")?.as_str()?.to_string();
                    let val = x.get("value").and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))).unwrap_or(0);
                    Some((n, val))
                })
                .collect()
        })
        .unwrap_or_default())
}

/// `Xray 26.3.27 (Xray, …)` → `26.3.27`
pub fn parse_version(out: &str) -> Option<String> {
    out.lines().next()?.split_whitespace().nth(1).map(String::from)
}

impl XrayCli {
    async fn run(&self, args: &[&str], timeout: Duration) -> Result<String> {
        let mut cmd = Command::new(&self.bin);
        cmd.args(args)
            .env("XRAY_LOCATION_ASSET", &self.asset_dir)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        let out = tokio::time::timeout(timeout, cmd.output())
            .await
            .map_err(|_| anyhow::anyhow!("xray {} 超时（{}s）", args.first().copied().unwrap_or(""), timeout.as_secs()))?
            .with_context(|| format!("启动 {}", self.bin.display()))?;
        if !out.status.success() {
            let mut msg = String::from_utf8_lossy(&out.stderr).to_string();
            msg.push_str(&String::from_utf8_lossy(&out.stdout));
            bail!("xray {} 失败（{}）：{}", args.join(" "), out.status, msg.trim().chars().take(600).collect::<String>());
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    fn server_arg(&self) -> String {
        format!("--server={}", self.api)
    }

    async fn api(&self, sub: &str, extra: &[&str]) -> Result<String> {
        let server = self.server_arg();
        let mut args = vec!["api", sub, &server];
        args.extend_from_slice(extra);
        self.run(&args, API_TIMEOUT).await
    }

    pub async fn lsi(&self) -> Result<Vec<String>> {
        parse_tags(&self.api("lsi", &[]).await?, "inbounds")
    }
    pub async fn lso(&self) -> Result<Vec<String>> {
        parse_tags(&self.api("lso", &[]).await?, "outbounds")
    }
    pub async fn lsrules(&self) -> Result<Vec<(String, String)>> {
        parse_rules(&self.api("lsrules", &[]).await?)
    }

    pub async fn runtime(&self) -> Result<Runtime> {
        Ok(Runtime { inbounds: self.lsi().await?, outbounds: self.lso().await?, rules: self.lsrules().await? })
    }

    /// 用户流量计数（只取入口 email 前缀）
    pub async fn user_stats(&self) -> Result<Vec<(String, u64)>> {
        parse_stats(&self.api("statsquery", &["-pattern", "user>>>e-"]).await?)
    }

    pub async fn version(&self) -> Result<String> {
        let out = self.run(&["version"], Duration::from_secs(5)).await?;
        parse_version(&out).context("无法解析 xray version")
    }

    /// `xray run -test -c <file>`
    pub async fn test_config(&self, cfg: &Value) -> Result<()> {
        // 日志路径不属于本次变更，且 -test 会真的去打开它（目录可能还不存在 / 沙箱里只读）→ 副本里关掉
        let mut cfg = cfg.clone();
        cfg["log"] = json!({"access": "none", "error": "none", "loglevel": "none"});
        let f = self.tmp_file("test", &serde_json::to_vec_pretty(&cfg)?)?;
        let r = self.run(&["run", "-test", "-c", f.to_str().unwrap_or("")], TEST_TIMEOUT).await;
        let _ = std::fs::remove_file(&f);
        r.map(|_| ())
    }

    fn tmp_file(&self, kind: &str, data: &[u8]) -> Result<PathBuf> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        std::fs::create_dir_all(&self.tmp_dir)?;
        let p = self.tmp_dir.join(format!("{kind}-{}-{}.json", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        crate::util::write_atomic(&p, data, false)?;
        Ok(p)
    }

    pub async fn exec(&self, op: &Op) -> Result<()> {
        let (sub, extra_json, tag): (&str, Option<Value>, Option<&str>) = match op {
            Op::AddOutbound(v) => ("ado", Some(json!({"outbounds": [v]})), None),
            Op::AddInbound(v) => ("adi", Some(json!({"inbounds": [v]})), None),
            Op::SetRules(r) => ("adrules", Some(json!({"routing": {"rules": r}})), None),
            Op::RemoveOutbound(t) => ("rmo", None, Some(t.as_str())),
            Op::RemoveInbound(t) => ("rmi", None, Some(t.as_str())),
        };
        if let Some(t) = tag {
            self.api(sub, &[t]).await?;
            return Ok(());
        }
        let f = self.tmp_file(sub, &serde_json::to_vec(&extra_json.expect("有内容"))?)?;
        let r = self.api(sub, &[f.to_str().unwrap_or("")]).await;
        let _ = std::fs::remove_file(&f);
        r.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_outputs() {
        let lsi = r#"{"inbounds":[{"proxySettings":{},"tag":"api"},{"tag":"in-1"}]}"#;
        assert_eq!(parse_tags(lsi, "inbounds").unwrap(), vec!["api", "in-1"]);
        assert!(parse_tags("{}", "inbounds").unwrap().is_empty());
        let rules = r#"{"rules":[{"ruleTag":"api","tag":"api"},{"ruleTag":"r","tag":"blocked"}]}"#;
        assert_eq!(parse_rules(rules).unwrap(), vec![("api".into(), "api".into()), ("r".into(), "blocked".into())]);
        let st = r#"{"stat":[{"name":"user>>>e-1>>>traffic>>>uplink","value":12},{"name":"user>>>e-1>>>traffic>>>downlink"}]}"#;
        assert_eq!(
            parse_stats(st).unwrap(),
            vec![("user>>>e-1>>>traffic>>>uplink".into(), 12), ("user>>>e-1>>>traffic>>>downlink".into(), 0)]
        );
        assert_eq!(parse_version("Xray 26.3.27 (Xray, Penetrates Everything.) d2758a0\nA unified").as_deref(), Some("26.3.27"));
    }
}
