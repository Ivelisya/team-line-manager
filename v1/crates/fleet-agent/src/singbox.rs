//! 线路机的 AnyTLS 外壳（sing-box）。官方 Xray 不支持 AnyTLS，sing-box 只做外壳，其余全留在 Xray 里：
//! 每个有 AnyTLS 的入口一个 anytls 入站（公网，入口端口 + `ANYTLS_PORT_OFFSET`），卸掉 AnyTLS 后经 vless 出站连本机 Xray 上
//! 这个入口的回环桥接入站（入口端口 + `ANYTLS_BRIDGE_OFFSET`，见 xray/render.rs 的 bridge_inbound_json）。访问策略、守卫规则、
//! 流量统计、访问日志都还在 Xray 里，按入口算，和 Reality 进来的合在一起。
//!
//! - 配置由 agent 全权生成：`<singbox_dir>/config.json`（root 0600）。systemd 以 root 打开它、经标准输入交给以动态用户运行的 sing-box
//!   （同 fleet-xray），所以证书和私钥内联进配置：动态用户读不到 0600 的 key.pem；
//! - 证书没有（或服务名换了）时用 `sing-box generate tls-keypair <服务名> -m 120` 生成到同一目录（私钥 0600）。自签：客户端一律跳过校验
//!   （链接里 insecure=1、Clash 里 skip-cert-verify），有效期给足 10 年，生成命令默认只有 1 个月；
//! - 配置变了才动：`sing-box check` → 原子换上 → `systemctl restart`。sing-box 没有热更新的接口，重启会断开这台线路机上全部 AnyTLS 连接
//!   （Reality 不受影响），所以内容一样就不重启；单元没在跑时拉起；
//! - 程序不在（还没装）：什么都不做，上报「未安装」，不影响 Xray 那一侧的应用与回报。

use crate::config::Config;
use crate::xray::ctl;
use crate::xray::render::anytls_ports;
use anyhow::{bail, ensure, Context, Result};
use fleet_proto::{tags, DesiredState, SingboxStatus};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::process::Command;

/// 自签证书的有效期（月）
const CERT_MONTHS: &str = "120";
/// 重启后隔这么久再看单元是不是还在跑（Type=simple：刚重启时一定是 active，起来就退出的要等一会儿才看得出来）
const SETTLE: Duration = Duration::from_secs(2);

pub fn inbound_tag(id: uuid::Uuid) -> String { format!("at-in-{}", id.simple()) }
pub fn outbound_tag(id: uuid::Uuid) -> String { format!("at-out-{}", id.simple()) }

/// PEM → 行数组（sing-box 的 certificate / key 字段收行数组）
fn pem_lines(pem: &str) -> Vec<&str> {
    pem.lines().map(str::trim_end).filter(|l| !l.is_empty()).collect()
}

/// 期望状态 → sing-box 配置（纯函数）。有 AnyTLS 的入口（`anytls_ports`，与 Xray 的桥接入站同一份）各一组入站 / 出站 / 规则，
/// 密码就是入口的 client_uuid（轮换凭据时一起变），user 名用入口的 email。最后一条规则拒绝其余一切：
/// 1.13 起没有 block 出站了，按规则动作 reject 兜底（实际每个入站都有自己的规则，兜底只为不让任何流量落到默认出站）
pub fn render(d: &DesiredState, cert: &str, key: &str) -> Value {
    let ports = anytls_ports(&d.entries);
    let tls = json!({"enabled": true, "server_name": d.relay.reality_sni, "certificate": pem_lines(cert), "key": pem_lines(key)});
    let (mut inbounds, mut outbounds, mut rules) = (vec![], vec![], vec![]);
    for e in d.entries.iter().filter(|e| e.enabled) {
        let Some(&(port, bridge)) = ports.get(&e.id) else { continue };
        let (it, ot, pw) = (inbound_tag(e.id), outbound_tag(e.id), e.client_uuid.to_string());
        inbounds.push(json!({
            "type": "anytls", "tag": it, "listen": "0.0.0.0", "listen_port": port,
            "users": [{"name": tags::email(e.id), "password": pw}],
            "tls": tls
        }));
        outbounds.push(json!({"type": "vless", "tag": ot, "server": "127.0.0.1", "server_port": bridge, "uuid": pw}));
        rules.push(json!({"inbound": [it], "action": "route", "outbound": ot}));
    }
    rules.push(json!({"network": ["tcp", "udp"], "action": "reject"}));
    json!({
        "log": {"level": "warn", "timestamp": false},
        "inbounds": inbounds,
        "outbounds": outbounds,
        "route": {"rules": rules}
    })
}

/// `sing-box generate tls-keypair` 的输出：先私钥后证书 → (证书, 私钥)
fn split_keypair(out: &str) -> Option<(String, String)> {
    let i = out.find("-----BEGIN CERTIFICATE-----")?;
    let (key, cert) = (out[..i].trim(), out[i..].trim());
    (key.contains("PRIVATE KEY-----") && cert.ends_with("-----END CERTIFICATE-----")).then(|| (format!("{cert}\n"), format!("{key}\n")))
}

/// `sing-box version 1.14.4\n\nEnvironment: …` → `1.14.4`
fn parse_version(out: &str) -> Option<String> {
    out.lines().next()?.split_whitespace().nth(2).map(String::from)
}

pub struct Singbox {
    cfg: Arc<Config>,
    lock: tokio::sync::Mutex<()>,
    last_error: Mutex<Option<String>>,
    /// (程序的修改时间, 版本)：换了程序才重新问
    version: Mutex<Option<(std::time::SystemTime, String)>>,
}

impl Singbox {
    pub fn new(cfg: Arc<Config>) -> Self {
        Self { cfg, lock: tokio::sync::Mutex::new(()), last_error: Mutex::new(None), version: Mutex::new(None) }
    }

    fn dir(&self) -> &Path { &self.cfg.singbox_dir }
    fn config_path(&self) -> PathBuf { self.dir().join("config.json") }
    fn installed(&self) -> bool { self.cfg.singbox_bin.is_file() }

    async fn run(&self, args: &[&str], timeout: Duration) -> Result<String> {
        let mut cmd = Command::new(&self.cfg.singbox_bin);
        // GOMAXPROCS 同 xray 子进程：在 agent 的 cgroup 里跑，别按核数开一堆线程
        cmd.args(args).env("GOMAXPROCS", "2").stdin(std::process::Stdio::null()).kill_on_drop(true);
        let out = tokio::time::timeout(timeout, cmd.output())
            .await
            .map_err(|_| anyhow::anyhow!("sing-box {} 超时", args.first().copied().unwrap_or("")))?
            .with_context(|| format!("启动 {}", self.cfg.singbox_bin.display()))?;
        if !out.status.success() {
            let mut msg = String::from_utf8_lossy(&out.stderr).to_string();
            msg.push_str(&String::from_utf8_lossy(&out.stdout));
            bail!("sing-box {} 失败（{}）：{}", args.first().copied().unwrap_or(""), out.status, msg.trim().chars().take(600).collect::<String>());
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    /// 应用期望状态（Xray 那一侧应用成功之后、周期对齐时调）。结果记进 `last_error`，随状态上报；不影响 Xray 的回报
    pub async fn apply(&self, d: &DesiredState) {
        let _g = self.lock.lock().await;
        let r = if self.installed() { self.apply_inner(d).await } else { Ok(()) };
        if let Err(e) = &r {
            tracing::error!("AnyTLS（sing-box）应用失败：{e:#}");
        }
        *self.last_error.lock().unwrap() = r.err().map(|e| format!("{e:#}"));
    }

    async fn apply_inner(&self, d: &DesiredState) -> Result<()> {
        let (cert, key) = self.ensure_cert(&d.relay.reality_sni).await?;
        let mut data = serde_json::to_vec_pretty(&render(d, &cert, &key))?;
        data.push(b'\n');
        let path = self.config_path();
        let svc = self.cfg.singbox_service.clone();
        let same = tokio::fs::read(&path).await.ok().is_some_and(|cur| cur == data);
        if same {
            if self.is_active().await {
                return Ok(());
            }
            tracing::warn!("{svc} 没在跑，拉起");
        } else {
            let new = self.dir().join("config.json.new");
            let (n2, d2) = (new.clone(), data);
            tokio::task::spawn_blocking(move || crate::util::write_atomic(&n2, &d2, true)).await.context("写配置任务异常终止")??;
            if let Err(e) = self.run(&["check", "-c", new.to_str().unwrap_or("")], Duration::from_secs(60)).await {
                let _ = std::fs::remove_file(&new);
                return Err(e.context("sing-box check 未通过"));
            }
            std::fs::rename(&new, &path).with_context(|| format!("rename → {}", path.display()))?;
            tracing::info!("AnyTLS 配置有变，重启 {svc}");
        }
        ctl::systemctl(&["restart", &svc]).await?;
        tokio::time::sleep(SETTLE).await;
        ensure!(self.is_active().await, "{svc} 重启后没在跑（journalctl -u {svc} 看原因）");
        Ok(())
    }

    async fn is_active(&self) -> bool {
        ctl::systemctl(&["is-active", "--quiet", &self.cfg.singbox_service]).await.is_ok()
    }

    /// 自签证书：cert.pem / key.pem 在、且生成时的服务名（server_name 文件）没变就用现成的，否则重新生成。返回 (证书, 私钥)
    async fn ensure_cert(&self, server_name: &str) -> Result<(String, String)> {
        let sn = server_name.trim();
        ensure!(!sn.is_empty() && sn.len() <= 253 && sn.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.'), "服务名（reality_sni）不合法：{sn:?}");
        let dir = self.dir().to_path_buf();
        let (cp, kp, np) = (dir.join("cert.pem"), dir.join("key.pem"), dir.join("server_name"));
        let read = |p: &Path| std::fs::read_to_string(p).ok();
        if let (Some(c), Some(k), Some(n)) = (read(&cp), read(&kp), read(&np)) {
            if n.trim() == sn && split_keypair(&format!("{k}{c}")).is_some() {
                return Ok((c, k));
            }
        }
        tracing::info!("生成 AnyTLS 自签证书（{sn}）");
        let out = self.run(&["generate", "tls-keypair", sn, "-m", CERT_MONTHS], Duration::from_secs(30)).await?;
        let (cert, key) = split_keypair(&out).context("sing-box generate tls-keypair 的输出认不出")?;
        let (c2, k2, sn2) = (cert.clone(), key.clone(), format!("{sn}\n"));
        tokio::task::spawn_blocking(move || -> Result<()> {
            use std::os::unix::fs::PermissionsExt;
            std::fs::create_dir_all(&dir).with_context(|| format!("创建 {}", dir.display()))?;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
            // write_atomic 写出来是 0600；服务名最后写：中途失败下次会重新生成
            crate::util::write_atomic(&kp, k2.as_bytes(), true)?;
            crate::util::write_atomic(&cp, c2.as_bytes(), true)?;
            crate::util::write_atomic(&np, sn2.as_bytes(), true)
        })
        .await
        .context("写证书任务异常终止")??;
        Ok((cert, key))
    }

    /// 现在的状态（上报用）：程序在不在、单元在不在跑、版本、最近一次应用的错误
    pub async fn status(&self) -> SingboxStatus {
        let last_error = self.last_error.lock().unwrap().clone();
        if !self.installed() {
            return SingboxStatus { installed: false, running: false, version: None, last_error };
        }
        let running = self.is_active().await;
        let mtime = std::fs::metadata(&self.cfg.singbox_bin).and_then(|m| m.modified()).ok();
        let cached = self.version.lock().unwrap().clone().filter(|(t, _)| Some(*t) == mtime).map(|v| v.1);
        let version = match cached {
            Some(v) => Some(v),
            None => {
                let v = self.run(&["version"], Duration::from_secs(5)).await.ok().and_then(|o| parse_version(&o));
                if let (Some(t), Some(v)) = (mtime, &v) {
                    *self.version.lock().unwrap() = Some((t, v.clone()));
                }
                v
            }
        };
        SingboxStatus { installed: true, running, version, last_error }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xray::render::tests::{desired, direct, entry, with_anytls};
    use uuid::Uuid;

    const CERT: &str = "-----BEGIN CERTIFICATE-----\nMIIB\nAAAA\n-----END CERTIFICATE-----\n";
    const KEY: &str = "-----BEGIN PRIVATE KEY-----\nMC4C\n-----END PRIVATE KEY-----\n";

    #[test]
    fn renders_one_anytls_inbound_per_entry_to_its_bridge() {
        let d = desired(vec![with_anytls(entry(1, 20001, true)), entry(2, 20002, true), with_anytls(direct(3, 20003)), with_anytls(entry(4, 20004, false))]);
        let v = render(&d, CERT, KEY);
        let (id1, id3) = (Uuid::from_u128(1).simple().to_string(), Uuid::from_u128(3).simple().to_string());
        let tls = json!({"enabled": true, "server_name": "www.cloudflare.com", "certificate": ["-----BEGIN CERTIFICATE-----", "MIIB", "AAAA", "-----END CERTIFICATE-----"], "key": ["-----BEGIN PRIVATE KEY-----", "MC4C", "-----END PRIVATE KEY-----"]});
        // 入口 2 没下发 AnyTLS、入口 4 停用：都没有
        assert_eq!(
            v["inbounds"],
            json!([
                {"type": "anytls", "tag": format!("at-in-{id1}"), "listen": "0.0.0.0", "listen_port": 40001,
                 "users": [{"name": format!("e-{id1}"), "password": Uuid::from_u128(1001).to_string()}], "tls": tls},
                {"type": "anytls", "tag": format!("at-in-{id3}"), "listen": "0.0.0.0", "listen_port": 40003,
                 "users": [{"name": format!("e-{id3}"), "password": Uuid::from_u128(1003).to_string()}], "tls": tls}
            ])
        );
        // 出站连 Xray 的桥接入站（入口端口 + 30000），uuid 同入口
        assert_eq!(v["outbounds"][0], json!({"type": "vless", "tag": format!("at-out-{id1}"), "server": "127.0.0.1", "server_port": 50001, "uuid": Uuid::from_u128(1001).to_string()}));
        assert_eq!(v["outbounds"].as_array().unwrap().len(), 2);
        assert_eq!(
            v["route"]["rules"],
            json!([
                {"inbound": [format!("at-in-{id1}")], "action": "route", "outbound": format!("at-out-{id1}")},
                {"inbound": [format!("at-in-{id3}")], "action": "route", "outbound": format!("at-out-{id3}")},
                {"network": ["tcp", "udp"], "action": "reject"}
            ])
        );
        // 没有任何 AnyTLS 入口：空的，兜底规则照留
        let v = render(&desired(vec![entry(1, 20001, true)]), CERT, KEY);
        assert_eq!((v["inbounds"].as_array().unwrap().len(), v["route"]["rules"].as_array().unwrap().len()), (0, 1));
    }

    #[test]
    fn keypair_and_version_parsing() {
        let out = format!("{KEY}\n{CERT}\n");
        assert_eq!(split_keypair(&out), Some((CERT.to_string(), KEY.to_string())));
        assert_eq!(split_keypair(KEY), None);
        assert_eq!(split_keypair(&format!("junk\n{CERT}")), None);
        assert_eq!(parse_version("sing-box version 1.14.4\n\nEnvironment: go1.25.3 linux/amd64\n").as_deref(), Some("1.14.4"));
    }
}
