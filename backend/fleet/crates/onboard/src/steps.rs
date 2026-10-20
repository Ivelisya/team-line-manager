//! 节点步骤 P1–P12、P14–P16（P13 线路机接入是批次级的，在 engine + relay）。
//! 每一步都「先观察后动作」：开头检查目标状态，已经达成就直接记成功——断点续跑、重试、重做都靠这一点。
//! 远端写入一律幂等：写文件先比哈希；nft 用「声明表 → 删表 → 重建」的单事务；authorized_keys 精确行匹配。

use crate::agent::{self, TokenPlan};
use crate::config::{Config, RelayCfg};
use crate::db::{Db, Row};
use crate::model::{cell, err, ErrClass, Outcome, StepErr};
use crate::relay;
use crate::ssh::{self, sq, Remote};
use base64::Engine as _;
use rand::{Rng, RngCore};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub struct SsBin {
    pub path: PathBuf,
    pub sha256: String,
    pub version: String,
}

/// 解包 shadowsocks-rust 官方 musl 包：先按 .sha256 校验 tar.xz，再取出 ssserver 并计算其 sha256。
pub fn prepare_ssserver(cfg: &Config) -> Result<SsBin, String> {
    let tb = &cfg.ss.tarball;
    let data = std::fs::read(tb).map_err(|e| format!("读 {}：{e}", tb.display()))?;
    let sha = hex::encode(Sha256::digest(&data));
    let side = PathBuf::from(format!("{}.sha256", tb.display()));
    let expect = std::fs::read_to_string(&side).map_err(|e| format!("读 {}：{e}", side.display()))?;
    if !expect.split_whitespace().next().is_some_and(|x| x.eq_ignore_ascii_case(&sha)) {
        return Err(format!("{} 的 sha256 与 .sha256 不符", tb.display()));
    }
    let dir = cfg.work_dir.join("bin");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let st = std::process::Command::new("tar")
        .args(["--no-same-owner", "--no-same-permissions", "-xJf", &tb.display().to_string(), "-C", &dir.display().to_string(), "ssserver"])
        .status()
        .map_err(|e| format!("tar：{e}"))?;
    if !st.success() {
        return Err(format!("解包 ssserver 失败（tar 退出码 {:?}；需要 tar + xz，包 {}）", st.code(), tb.display()));
    }
    let p = dir.join("ssserver");
    let b = std::fs::read(&p).map_err(|e| e.to_string())?;
    let version = tb.file_name().and_then(|n| n.to_str()).and_then(|n| n.split('-').nth(1)).unwrap_or("?").trim_start_matches('v').to_string();
    Ok(SsBin { path: p, sha256: hex::encode(Sha256::digest(&b)), version })
}

pub struct Cx {
    pub cfg: Arc<Config>,
    pub db: Db,
    pub row: Row,
    pub rc: RelayCfg,
    pub askpass: PathBuf,
    pub ss: Arc<SsBin>,
    /// P11：已校验的 nodeagent 安装包（没配 [agent] 或没准备好时为 None）
    pub edge: Option<Arc<agent::AgentBin>>,
    /// P11：令牌后端（fleetd 注入的 fleetd-ingest，或独立运行时的命令）
    pub tokens: Option<Arc<dyn agent::AgentTokens>>,
}

pub fn fingerprint(key_line: &str) -> String {
    let b = key_line.split_whitespace().nth(1).unwrap_or("");
    let raw = base64::engine::general_purpose::STANDARD.decode(b).unwrap_or_default();
    format!("SHA256:{}", base64::engine::general_purpose::STANDARD_NO_PAD.encode(Sha256::digest(&raw)))
}

pub fn uuid_v4() -> String {
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex::encode(b);
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

pub fn reality_keypair() -> (String, String) {
    let mut sk = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut sk);
    reality_pub_from(&sk)
}
pub fn reality_pub_from(sk: &[u8; 32]) -> (String, String) {
    let s = x25519_dalek::StaticSecret::from(*sk);
    let p = x25519_dalek::PublicKey::from(&s);
    let e = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    (e.encode(s.to_bytes()), e.encode(p.as_bytes()))
}
pub fn reality_public(private_b64: &str) -> Option<String> {
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(private_b64.trim_end_matches('=')).ok()?;
    let sk: [u8; 32] = raw.try_into().ok()?;
    Some(reality_pub_from(&sk).1)
}

fn rand_password(n: usize) -> String {
    const A: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";
    let mut r = rand::thread_rng();
    (0..n).map(|_| A[r.gen_range(0..A.len())] as char).collect()
}

impl Cx {
    fn t(s: u64) -> Duration {
        Duration::from_secs(s)
    }
    pub fn known_hosts(&self) -> PathBuf {
        self.cfg.work_dir.join("known_hosts").join(&self.row.id)
    }
    pub fn node(&self) -> Remote {
        Remote {
            ssh_bin: self.cfg.ssh_bin.clone(),
            key: self.cfg.ssh_key.clone(),
            known_hosts: self.known_hosts(),
            host: self.row.spec.ip.clone(),
            port: self.row.spec.ssh_port,
            user: "root".into(),
            control_dir: Some(ssh::control_dir()),
        }
    }
    pub fn relay(&self) -> Remote {
        relay::relay_remote(&self.rc, &self.cfg.ssh_bin)
    }
    fn outputs(&self, step: &str) -> Value {
        self.db.step(&self.row.id, step).ok().flatten().map(|s| s.outputs).unwrap_or(json!({}))
    }
    fn dberr(e: String) -> StepErr {
        StepErr { class: ErrClass::Bug, msg: e }
    }
    fn pubkey(&self) -> Result<String, StepErr> {
        std::fs::read_to_string(&self.cfg.ssh_pubkey)
            .map(|s| s.trim().to_string())
            .map_err(|e| StepErr { class: ErrClass::Bug, msg: format!("读上架公钥 {}：{e}", self.cfg.ssh_pubkey.display()) })
    }
    pub fn ss_password(&self) -> Result<String, StepErr> {
        self.db.secret_or_insert(&self.row.id, "ss_password", || rand_password(32)).map_err(Self::dberr)
    }
    pub fn ss_outbound(&self) -> Result<Value, StepErr> {
        let s = &self.row.spec;
        Ok(json!({"tag": relay::outbound_tag(&s.ip, s.ss_public_port, s.nat), "protocol": "shadowsocks",
            "settings": {"servers": [{"address": s.ip, "port": s.ss_public_port, "method": self.cfg.ss.method, "password": self.ss_password()?}]}}))
    }

    // ── 通用：钉公钥 ───────────────────────────────────────────────────────
    async fn scan_stable(&self, phase: &str) -> Result<(String, String, u32), StepErr> {
        let s = &self.row.spec;
        let mut seen: BTreeMap<String, u32> = BTreeMap::new();
        let mut banner = String::new();
        let n = self.cfg.keyscan_samples.max(1);
        for i in 0..n {
            let (keys, b) = ssh::keyscan(&self.cfg.keyscan_bin, &s.ip, s.ssh_port).await?;
            if !b.is_empty() {
                banner = b;
            }
            for k in keys {
                *seen.entry(k).or_default() += 1;
            }
            if i + 1 < n {
                tokio::time::sleep(Duration::from_millis(self.cfg.keyscan_interval_ms)).await;
            }
        }
        for (k, c) in &seen {
            let _ = self.db.host_key(&self.row.id, phase, "keyscan_control", k, *c as i64);
        }
        match seen.len() {
            0 => err(ErrClass::Transient, format!("{}:{} 没有返回 ed25519 主机公钥", s.ip, s.ssh_port)),
            1 => {
                let (k, c) = seen.into_iter().next().unwrap();
                Ok((k, banner, c))
            }
            m => err(
                ErrClass::Identity,
                format!(
                    "{n} 次扫描里出现了 {m} 把不同的主机公钥（{}）：这个 IP 可能被别的机器占着（ARP 冲突/冒名）。不发密码，请向供应商核实",
                    seen.iter().map(|(k, c)| format!("{} ×{c}", fingerprint(k))).collect::<Vec<_>>().join("、")
                ),
            ),
        }
    }
    fn pinned(&self) -> Option<String> {
        std::fs::read_to_string(self.known_hosts()).ok().and_then(|t| t.lines().next().map(|l| l.split_whitespace().skip(1).collect::<Vec<_>>().join(" ")))
    }
    fn pin(&self, key: &str) -> Result<(), StepErr> {
        let p = self.known_hosts();
        std::fs::create_dir_all(p.parent().unwrap()).map_err(|e| StepErr { class: ErrClass::Bug, msg: e.to_string() })?;
        std::fs::write(&p, ssh::known_hosts_line(&self.row.spec.ip, self.row.spec.ssh_port, key)).map_err(|e| StepErr { class: ErrClass::Bug, msg: e.to_string() })
    }

    async fn key_login_ok(&self) -> bool {
        matches!(self.node().run("true", Self::t(20)).await, Ok(o) if o.ok())
    }

    async fn push_key_with_password(&self) -> Result<(), StepErr> {
        let Some(pw) = self.row.spec.password_file.clone() else {
            return err(ErrClass::Input, "上架密钥登录被拒，这一行也没有给密码文件：把公钥加进 root 的 authorized_keys，或用 resume --row … --password-file 补密码");
        };
        let k = self.pubkey()?;
        let script = format!(
            "umask 077; mkdir -p ~/.ssh; touch ~/.ssh/authorized_keys; grep -qxF {k} ~/.ssh/authorized_keys || echo {k} >> ~/.ssh/authorized_keys; chmod 600 ~/.ssh/authorized_keys; echo ok",
            k = sq(&k)
        );
        let o = self.node().run_password(&self.askpass, std::path::Path::new(&pw), &script, Self::t(40)).await.map_err(|e| match e.class {
            ErrClass::Input => StepErr {
                class: ErrClass::Input,
                msg: "root 密码被拒。先确认主机公钥属于这台机器（P1 已钉住），再从供应商面板取密码；补好后 fleetctl onboard resume <批次> --row <名称> --password-file <文件>".into(),
            },
            _ => e,
        })?;
        if !o.ok() || !o.stdout.contains("ok") {
            return err(ErrClass::Transient, format!("推公钥失败 rc={}：{}", o.code, o.stderr.trim()));
        }
        Ok(())
    }

    // ── P1 预检 ─────────────────────────────────────────────────────────────
    pub async fn p1_preflight(&self) -> Result<Outcome, StepErr> {
        let s = &self.row.spec;
        let addr = format!("{}:{}", s.ip, s.ssh_port);
        let mut reach = false;
        for _ in 0..3 {
            if let Ok(Ok(_)) = tokio::time::timeout(Self::t(5), tokio::net::TcpStream::connect(&addr)).await {
                reach = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        if !reach {
            return err(ErrClass::Transient, format!("{addr} 连不上（3 次，每次 5 秒）"));
        }
        let (key, banner, samples) = self.scan_stable("pre_dd").await?;
        if let Some(p) = self.pinned() {
            if p != key && self.outputs("P6")["pinned"].is_null() {
                return err(ErrClass::Identity, format!("主机公钥变了：已钉住 {}，现在是 {}", fingerprint(&p), fingerprint(&key)));
            }
        }
        // 第二视角：所属线路机做一次 keyscan（能做就做，做不了不挡）
        let second = match self
            .relay()
            .run(&format!("command -v ssh-keyscan >/dev/null || exit 3; ssh-keyscan -T 5 -p {} -t ed25519 {} 2>/dev/null", s.ssh_port, sq(&s.ip)), Self::t(20))
            .await
        {
            Ok(o) if o.ok() => {
                let rk: Vec<String> = o.stdout.lines().filter(|l| !l.starts_with('#')).filter_map(|l| { let v: Vec<_> = l.split_whitespace().collect(); (v.len() >= 3).then(|| format!("{} {}", v[1], v[2])) }).collect();
                if rk.iter().any(|k| k != &key) {
                    let _ = self.db.host_key(&self.row.id, "pre_dd", &format!("keyscan_relay:{}", self.rc.name), &rk[0], 1);
                    return err(ErrClass::Identity, format!("两个视角看到的主机公钥不同（中控 {}，{} {}）", fingerprint(&key), self.rc.name, fingerprint(&rk[0])));
                }
                if rk.is_empty() { "线路机视角无结果".to_string() } else { format!("{} 视角一致", self.rc.name) }
            }
            _ => "线路机没有 ssh-keyscan，跳过第二视角".to_string(),
        };
        // 这个 IP:端口 是否已经挂在所属线路机上
        let snap = relay::snapshot(&self.relay(), &self.rc).await?;
        let target = format!("{}:{}", s.ip, s.ss_public_port);
        let mine = relay::outbound_tag(&s.ip, s.ss_public_port, s.nat);
        if let Some(t) = snap.outbound_targets().get(&target) {
            if t != &mine || self.db.asset_by_row(&self.row.id).ok().flatten().is_none() {
                return err(ErrClass::Conflict, format!("{target} 已经挂在 {} 的转发 {t} 上：这是现有节点，不能再上架", self.rc.name));
            }
        }
        self.pin(&key)?;
        Ok(Outcome::Done(json!({"fingerprint": fingerprint(&key), "samples": samples, "banner": banner, "second_vantage": second})))
    }

    // ── P2 推密钥 ───────────────────────────────────────────────────────────
    pub async fn p2_bootstrap(&self) -> Result<Outcome, StepErr> {
        if self.key_login_ok().await {
            return Ok(Outcome::Done(json!({"method": "already"})));
        }
        if self.row.spec.auth == "key" && self.row.spec.password_file.is_none() {
            return err(ErrClass::Input, "上架密钥登录被拒：把 fleet 上架公钥加进 root 的 authorized_keys 后 resume，或补密码文件");
        }
        self.push_key_with_password().await?;
        if !self.key_login_ok().await {
            return err(ErrClass::Transient, "公钥已写入但密钥登录仍失败");
        }
        Ok(Outcome::Done(json!({"method": "password", "note": "引导密码已用过：请在供应商面板重置 root 密码（M10）"})))
    }

    // ── P3 身份采集 ─────────────────────────────────────────────────────────
    async fn facts(&self) -> Result<Value, StepErr> {
        let o = self
            .node()
            .run_ok(
                "set -u; . /etc/os-release 2>/dev/null; echo \"os=${ID:-?} ${VERSION_ID:-?}\"\n\
                 echo \"uuid=$(cat /sys/class/dmi/id/product_uuid 2>/dev/null)\"\n\
                 echo \"macs=$(ip -o link 2>/dev/null | grep -o 'link/ether [0-9a-f:]*' | awk '{print $2}' | sort -u | tr '\\n' ' ')\"\n\
                 echo \"systemd=$([ -d /run/systemd/system ] && echo 1 || echo 0)\"\n\
                 echo \"systemd_ver=$(systemctl --version 2>/dev/null | head -1 | awk '{print $2}')\"\n\
                 echo \"mem_mb=$(awk '/MemTotal/{print int($2/1024)}' /proc/meminfo)\"\n\
                 echo \"kernel=$(uname -r)\"; echo \"arch=$(uname -m)\"\n\
                 echo \"nft=$(command -v nft || true)\"; echo \"machine_id=$(cat /etc/machine-id 2>/dev/null)\"\n",
                Self::t(30),
                ErrClass::Transient,
            )
            .await?;
        let mut m = serde_json::Map::new();
        for l in o.lines() {
            if let Some((k, v)) = l.split_once('=') {
                m.insert(k.trim().into(), json!(v.trim()));
            }
        }
        Ok(Value::Object(m))
    }
    pub async fn p3_identify(&self) -> Result<Outcome, StepErr> {
        let f = self.facts().await?;
        if f["arch"] != "x86_64" {
            return err(ErrClass::Provider, format!("架构 {} 不是 x86_64（ssserver 包只有 x86_64）", f["arch"]));
        }
        if let Some(mac) = &self.row.spec.mac {
            let macs = f["macs"].as_str().unwrap_or("");
            if !macs.split_whitespace().any(|m| m.eq_ignore_ascii_case(mac)) {
                return err(ErrClass::Identity, format!("MAC 不符：CSV 给的是 {mac}，机器上是 {macs}"));
            }
        }
        Ok(Outcome::Done(f))
    }

    // ── P4–P6 重装（今晚：人工闸门）─────────────────────────────────────────
    // 供应商适配器接口保留：reinstaller(provider) → None 表示没有可用的公开 API（ATTVPS 客户中心、OhnoICU 的
    // VirtFusion 客户面板都没有给客户用的重装 API），只能人工闸门。以后接 bin456789/reinstall（dd + NoCloud）时在这里加。
    pub async fn p4_dd_prepare(&self) -> Result<Outcome, StepErr> {
        if self.row.spec.dd != "manual" {
            return Ok(Outcome::Skipped("不重装（dd=none）".into()));
        }
        if self.outputs("P4")["ack"].as_bool() == Some(true) {
            return Ok(Outcome::Done(json!({"mode": "manual", "ack": true})));
        }
        Ok(Outcome::WaitInput(format!(
            "等待输入：请在供应商面板重装后继续（{} 的服务号 {}）。完成后执行 fleetctl onboard resume <批次> --row {} [--password-file <新 root 密码文件>]",
            self.row.spec.provider,
            self.row.spec.service_id.as_deref().unwrap_or("—"),
            self.row.spec.short_name
        )))
    }
    pub async fn p5_dd_execute(&self) -> Result<Outcome, StepErr> {
        if self.row.spec.dd != "manual" {
            return Ok(Outcome::Skipped("不重装（dd=none）".into()));
        }
        Ok(Outcome::Skipped("人工在供应商面板执行（系统不自动重装任何机器）".into()))
    }
    pub async fn p6_dd_verify(&self) -> Result<Outcome, StepErr> {
        if self.row.spec.dd != "manual" {
            return Ok(Outcome::Skipped("不重装（dd=none）".into()));
        }
        let before = self.outputs("P1")["fingerprint"].as_str().unwrap_or("").to_string();
        let done = self.outputs("P6");
        self.node().close_master().await;
        // 等 SSH 回来
        let addr = format!("{}:{}", self.row.spec.ip, self.row.spec.ssh_port);
        let deadline = tokio::time::Instant::now() + Self::t(self.cfg.dd_wait_timeout_s);
        loop {
            if let Ok(Ok(_)) = tokio::time::timeout(Self::t(5), tokio::net::TcpStream::connect(&addr)).await {
                break;
            }
            if tokio::time::Instant::now() > deadline {
                return err(ErrClass::Provider, format!("重装后 {} 秒 SSH 还没回来：去供应商面板看 VNC", self.cfg.dd_wait_timeout_s));
            }
            tokio::time::sleep(Self::t(5)).await;
        }
        let (key, _, samples) = self.scan_stable("post_dd").await?;
        let fp = fingerprint(&key);
        if fp == before && done["pinned"].as_str() != Some(fp.as_str()) {
            return Ok(Outcome::WaitInput(format!("主机公钥没变（{fp}）：看起来还没重装。重装完成后再 resume")));
        }
        self.pin(&key)?;
        let _ = self.db.step_outputs_merge(&self.row.id, "P6", &json!({"pinned": fp}));
        if !self.key_login_ok().await {
            self.push_key_with_password().await?;
        }
        if !self.key_login_ok().await {
            return err(ErrClass::Transient, "重装后公钥已写入但密钥登录仍失败");
        }
        let now = self.facts().await?;
        let base = self.outputs("P3");
        let (bu, nu) = (base["uuid"].as_str().unwrap_or(""), now["uuid"].as_str().unwrap_or(""));
        let (bm, nm) = (base["macs"].as_str().unwrap_or(""), now["macs"].as_str().unwrap_or(""));
        let anchor = if !bu.is_empty() && !nu.is_empty() {
            if bu != nu {
                return err(ErrClass::Identity, format!("重装前后 DMI product_uuid 不同（{bu} → {nu}）：回来的不是同一台机器"));
            }
            "DMI product_uuid 一致"
        } else if !bm.is_empty() && !nm.is_empty() {
            if !bm.split_whitespace().any(|m| nm.split_whitespace().any(|n| n == m)) {
                return err(ErrClass::Identity, format!("重装前后 MAC 不同（{bm} → {nm}）"));
            }
            "MAC 一致"
        } else {
            "无硬件锚点可比（只认新钉住的主机公钥）"
        };
        Ok(Outcome::Done(json!({"fingerprint_before": before, "fingerprint_after": fp, "samples": samples, "identity": anchor, "os": now["os"], "pinned": fp})))
    }

    // ── P7 系统基线 ─────────────────────────────────────────────────────────
    pub async fn p7_harden(&self) -> Result<Outcome, StepErr> {
        let script = r#"set -eu
command -v nft >/dev/null || { export DEBIAN_FRONTEND=noninteractive; timeout 300 apt-get install -y -q nftables >/dev/null; }
install -d -m 755 /etc/ssh/sshd_config.d
grep -qE '^[[:space:]]*Include[[:space:]]+/etc/ssh/sshd_config\.d/\*\.conf' /etc/ssh/sshd_config || sed -i '1i Include /etc/ssh/sshd_config.d/*.conf' /etc/ssh/sshd_config
F=/etc/ssh/sshd_config.d/00-fleet-keyonly.conf
printf '%s\n' '# fleet onboard：只允许密钥登录（文件名 00- 保证先于 50-cloud-init 等生效）' 'PasswordAuthentication no' 'ChallengeResponseAuthentication no' 'PermitRootLogin prohibit-password' 'PubkeyAuthentication yes' > "$F.tmp"
if ! cmp -s "$F.tmp" "$F" 2>/dev/null; then mv -f "$F.tmp" "$F"; CH=1; else rm -f "$F.tmp"; CH=0; fi
sshd -t
if [ "$CH" = 1 ]; then systemctl reload ssh 2>/dev/null || systemctl reload sshd 2>/dev/null || kill -HUP "$(cat /run/sshd.pid)"; sleep 0.5; fi
echo "==SSHD"; sshd -T 2>/dev/null | grep -E '^(passwordauthentication|kbdinteractiveauthentication|challengeresponseauthentication|permitrootlogin|pubkeyauthentication) '
printf '%s\n' 'net.core.default_qdisc=fq' 'net.ipv4.tcp_congestion_control=bbr' > /etc/sysctl.d/90-fleet-bbr.conf
modprobe tcp_bbr 2>/dev/null || true
sysctl -q -p /etc/sysctl.d/90-fleet-bbr.conf >/dev/null 2>&1 || echo "==SYSCTLFAIL"
echo "==CC"; cat /proc/sys/net/ipv4/tcp_congestion_control
echo "==QDISC"; cat /proc/sys/net/core/default_qdisc 2>/dev/null || echo '?'
"#;
        let out = self.node().run_ok(script, Self::t(360), ErrClass::Transient).await?;
        let sshd: BTreeMap<String, String> = out
            .split("==SSHD").nth(1).unwrap_or("").split("==").next().unwrap_or("")
            .lines().filter_map(|l| l.split_once(' ').map(|(a, b)| (a.to_string(), b.trim().to_string()))).collect();
        if sshd.get("passwordauthentication").map(String::as_str) != Some("no") {
            return err(ErrClass::Bug, format!("sshd -T 仍显示密码登录开启：{sshd:?}"));
        }
        if !matches!(sshd.get("permitrootlogin").map(String::as_str), Some("without-password" | "prohibit-password")) {
            return err(ErrClass::Bug, format!("sshd -T permitrootlogin = {:?}", sshd.get("permitrootlogin")));
        }
        let cc = out.split("==CC").nth(1).and_then(|x| x.lines().nth(1)).unwrap_or("").trim().to_string();
        let qd = out.split("==QDISC").nth(1).and_then(|x| x.lines().nth(1)).unwrap_or("").trim().to_string();
        let mut notes = vec![];
        if cc != "bbr" {
            if self.cfg.require_bbr {
                return err(ErrClass::Provider, format!("BBR 没生效（tcp_congestion_control={cc}）：内核可能没有 tcp_bbr"));
            }
            notes.push(format!("BBR 未生效（{cc}；require_bbr=false，容器演练 /proc/sys 只读）"));
        }
        // 反测：密码登录必须被拒（只对给了密码文件的行）
        let mut pw_denied = Value::Null;
        if let Some(pw) = &self.row.spec.password_file {
            match self.node().run_password(&self.askpass, std::path::Path::new(pw), "echo LOGGED_IN", Self::t(30)).await {
                Err(e) if e.class == ErrClass::Input => pw_denied = json!(true),
                Ok(o) if o.stdout.contains("LOGGED_IN") => return err(ErrClass::Bug, "改成仅密钥后，密码仍能登录"),
                other => pw_denied = json!(format!("{other:?}").chars().take(120).collect::<String>()),
            }
        }
        Ok(Outcome::Done(json!({"sshd": sshd, "tcp_congestion_control": cc, "default_qdisc": qd, "password_login_denied": pw_denied, "notes": notes})))
    }

    // ── P8 防火墙 ───────────────────────────────────────────────────────────
    pub async fn p8_firewall(&self) -> Result<Outcome, StepErr> {
        let s = &self.row.spec;
        let allow = &self.rc.allow_ips;
        if allow.is_empty() {
            return err(ErrClass::Bug, format!("{} 没配 allow_ips", self.rc.name));
        }
        let nft = format!(
            "table inet ss_guard\ndelete table inet ss_guard\ntable inet ss_guard {{\n  set relays {{ type ipv4_addr; elements = {{ {} }} }}\n  chain input {{\n    type filter hook input priority -10; policy accept;\n    tcp dport {p} ip saddr @relays accept\n    udp dport {p} ip saddr @relays accept\n    tcp dport {p} drop\n    udp dport {p} drop\n  }}\n}}\n",
            allow.join(", "),
            p = s.ss_port
        );
        let script = format!(
            "set -eu\nF=/etc/fleet/nft/ss_guard.nft; install -d -m 755 /etc/fleet/nft\n\
             echo {} | base64 -d > \"$F.tmp\"; nft -c -f \"$F.tmp\"; mv -f \"$F.tmp\" \"$F\"; nft -f \"$F\"\n\
             # 持久化：/etc/nftables.conf 在开机时加载（Debian 默认以 flush ruleset 开头，所以 include 放末尾）\n\
             [ -f /etc/nftables.conf ] || printf '#!/usr/sbin/nft -f\\n' > /etc/nftables.conf\n\
             grep -qF 'include \"/etc/fleet/nft/ss_guard.nft\"' /etc/nftables.conf || printf '%s\\n' 'include \"/etc/fleet/nft/ss_guard.nft\"' >> /etc/nftables.conf\n\
             [ -d /run/systemd/system ] && systemctl enable nftables >/dev/null 2>&1 || true\n\
             echo ==LIST; nft list set inet ss_guard relays\n",
            base64::engine::general_purpose::STANDARD.encode(nft.as_bytes())
        );
        let out = self.node().run_ok(&script, Self::t(60), ErrClass::Transient).await?;
        for ip in allow {
            if !out.contains(ip.as_str()) {
                return err(ErrClass::Bug, format!("nft 集合里没有 {ip}"));
            }
        }
        // 正测：所属线路机应能到达（SS 还没起时应很快收到 RST；已起则直接连上）
        let pos = self
            .relay()
            .run(&format!("timeout 4 bash -c 'exec 3<>/dev/tcp/{}/{}' 2>/dev/null; echo rc=$?", s.ip, s.ss_public_port), Self::t(20))
            .await?;
        let prc = pos.stdout.trim().rsplit("rc=").next().unwrap_or("").trim().to_string();
        if prc == "124" {
            return err(ErrClass::Provider, format!("正测失败：{} 连 {}:{} 超时（供应商侧防火墙？）", self.rc.name, s.ip, s.ss_public_port));
        }
        // 反测：不在白名单的视角必须超时
        let vantage = self.cfg.relays.iter().find(|r| r.name != self.rc.name && !allow.contains(&r.public_host));
        let neg = match vantage {
            Some(v) => {
                let o = relay::relay_remote(v, &self.cfg.ssh_bin)
                    .run(&format!("timeout 4 bash -c 'exec 3<>/dev/tcp/{}/{}' 2>/dev/null; echo rc=$?", s.ip, s.ss_public_port), Self::t(20))
                    .await?;
                (v.name.clone(), o.stdout.trim().ends_with("rc=124"))
            }
            None => {
                let r = tokio::time::timeout(Self::t(4), tokio::net::TcpStream::connect(format!("{}:{}", s.ip, s.ss_public_port))).await;
                ("中控".to_string(), r.is_err())
            }
        };
        if !neg.1 {
            return err(ErrClass::Bug, format!("反测失败：{} 不在白名单却能到达 {}:{}（nft 没生效）", neg.0, s.ip, s.ss_public_port));
        }
        Ok(Outcome::Done(json!({"allow": allow, "positive": format!("{} rc={prc}", self.rc.name), "negative": format!("{} 超时（被丢弃）", neg.0), "persist": "/etc/nftables.conf include"})))
    }

    // ── P9 Shadowsocks（shadowsocks-rust ssserver）──────────────────────────
    pub async fn p9_ss(&self) -> Result<Outcome, StepErr> {
        let pw = self.ss_password()?;
        let conf = json!({"server": "0.0.0.0", "server_port": self.row.spec.ss_port, "password": pw, "method": self.cfg.ss.method, "mode": "tcp_and_udp", "timeout": 300});
        let conf_text = serde_json::to_string_pretty(&conf).unwrap() + "\n";
        let conf_sha = hex::encode(Sha256::digest(conf_text.as_bytes()));
        let n = self.node();
        let cur = n
            .run(
                "echo \"bin=$(sha256sum /usr/local/bin/ssserver 2>/dev/null | cut -d' ' -f1)\"; echo \"conf=$(sha256sum /etc/shadowsocks-rust/config.json 2>/dev/null | cut -d' ' -f1)\"",
                Self::t(20),
            )
            .await?;
        let kv0: BTreeMap<&str, &str> = cur.stdout.lines().filter_map(|l| l.split_once('=')).collect();
        let bin_ok = kv0.get("bin").map(|x| x.trim()) == Some(self.ss.sha256.as_str());
        if !bin_ok {
            n.push_file(&self.ss.path, "/usr/local/bin/ssserver", "0755", &self.ss.sha256).await?;
        }
        let conf_ok = kv0.get("conf").map(|x| x.trim()) == Some(conf_sha.as_str());
        let script = format!(
            "set -eu\numask 077\nC=/etc/shadowsocks-rust/config.json; install -d -m 755 /etc/shadowsocks-rust\n\
             CH={ch}\n[ \"$CH\" = 1 ] && {{ echo {conf} | base64 -d > \"$C.tmp\"; chmod 600 \"$C.tmp\"; mv -f \"$C.tmp\" \"$C\"; }}\n\
             if [ -d /run/systemd/system ]; then\n\
               # 固定的系统用户（不用 DynamicUser+LoadCredential：a395 实测在部分环境凭据目录不可读，且 Ubuntu 20.04 的 systemd 245 不支持）\n\
               id fleet-ss >/dev/null 2>&1 || useradd --system --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin fleet-ss\n\
               chgrp fleet-ss \"$C\"; chmod 640 \"$C\"\n\
               EX='ExecStart=/usr/local/bin/ssserver -c /etc/shadowsocks-rust/config.json'; CRED='Group=fleet-ss'; USR='User=fleet-ss'\n\
               U=/etc/systemd/system/fleet-ssserver.service\n\
               printf '%s\\n' '[Unit]' 'Description=fleet shadowsocks-rust ssserver' 'After=network-online.target' 'Wants=network-online.target' '' '[Service]' \"$EX\" \"$CRED\" \"$USR\" 'Restart=always' 'RestartSec=2' 'LimitNOFILE=65536' 'MemoryMax=128M' 'NoNewPrivileges=yes' 'ProtectSystem=strict' 'ProtectHome=yes' 'PrivateTmp=yes' '' '[Install]' 'WantedBy=multi-user.target' > \"$U.tmp\"\n\
               if ! cmp -s \"$U.tmp\" \"$U\"; then mv -f \"$U.tmp\" \"$U\"; systemctl daemon-reload; CH=1; else rm -f \"$U.tmp\"; fi\n\
               systemctl enable fleet-ssserver >/dev/null 2>&1\n\
               if [ \"$CH\" = 1 ] || [ {bin_changed} = 1 ] || ! systemctl is-active -q fleet-ssserver; then systemctl restart fleet-ssserver; fi\n\
               SUP=systemd\n\
             else\n\
               # 无 systemd（容器演练）：自带一个最小的守护方式\n\
               P=$(pgrep -f '^/usr/local/bin/ssserver -c /etc/shadowsocks-rust/config.json' || true)\n\
               if [ -n \"$P\" ] && {{ [ \"$CH\" = 1 ] || [ {bin_changed} = 1 ]; }}; then kill $P; sleep 0.3; P=''; fi\n\
               [ -z \"$P\" ] && {{ setsid /usr/local/bin/ssserver -c /etc/shadowsocks-rust/config.json >> /var/log/fleet-ssserver.log 2>&1 < /dev/null & }}\n\
               SUP=none\n\
             fi\n\
             for n in $(seq 1 30); do T=$(ss -Hltn \"sport = :{port}\" | wc -l); U=$(ss -Hlun \"sport = :{port}\" | wc -l); [ \"$T\" -ge 1 ] && [ \"$U\" -ge 1 ] && break; sleep 0.2; done\n\
             echo \"==RES sup=$SUP tcp=$T udp=$U ver=$(/usr/local/bin/ssserver --version 2>/dev/null | awk '{{print $2}}') mode=$(stat -c %a \"$C\")\"\n",
            ch = if conf_ok { 0 } else { 1 },
            conf = base64::engine::general_purpose::STANDARD.encode(conf_text.as_bytes()),
            bin_changed = if bin_ok { 0 } else { 1 },
            port = self.row.spec.ss_port
        );
        let out = n.run_ok(&script, Self::t(90), ErrClass::Transient).await?;
        let res = out.lines().find_map(|l| l.strip_prefix("==RES ")).unwrap_or("").to_string();
        let kv: BTreeMap<&str, &str> = res.split_whitespace().filter_map(|x| x.split_once('=')).collect();
        if kv.get("tcp").copied().unwrap_or("0") == "0" || kv.get("udp").copied().unwrap_or("0") == "0" {
            return err(ErrClass::Transient, format!("ssserver 没在 {} 上同时监听 tcp 和 udp：{res}", self.row.spec.ss_port));
        }
        Ok(Outcome::Done(json!({"impl": "shadowsocks-rust", "version": kv.get("ver"), "method": self.cfg.ss.method, "port": self.row.spec.ss_port,
            "public_port": self.row.spec.ss_public_port, "supervisor": kv.get("sup"), "config_mode": kv.get("mode"), "run_as": if kv.get("sup") == Some(&"systemd") { "fleet-ss（配置 0640 root:fleet-ss）" } else { "root（无 systemd 的演练容器）" }, "config_sha256": &conf_sha[..16],
            "binary_sha256": &self.ss.sha256[..16], "password_ref": "secrets:ss_password"})))
    }

    // ── P10 登记 ────────────────────────────────────────────────────────────
    pub async fn p10_register(&self) -> Result<Outcome, StepErr> {
        let s = &self.row.spec;
        let snap = relay::snapshot(&self.relay(), &self.rc).await?;
        let port = match self.db.allocate_port(&self.rc.name, &self.row.id, &snap.file_ports(), self.cfg.ss.relay_port_lo, self.cfg.ss.relay_port_hi).map_err(Self::dberr)? {
            Ok(p) => p,
            Err(m) => return err(ErrClass::Conflict, m),
        };
        let uuid = self.db.secret_or_insert(&self.row.id, "client_uuid", uuid_v4).map_err(Self::dberr)?;
        let (sk, _) = reality_keypair();
        let sk = self.db.secret_or_insert(&self.row.id, "reality_private_key", || sk).map_err(Self::dberr)?;
        let pk = reality_public(&sk).ok_or_else(|| StepErr { class: ErrClass::Bug, msg: "REALITY 私钥格式坏了".into() })?;
        let sid = self.db.secret_or_insert(&self.row.id, "short_id", || { let mut b = [0u8; 8]; rand::thread_rng().fill_bytes(&mut b); hex::encode(b) }).map_err(Self::dberr)?;
        // email（Xray 用户统计的键）：沿用 node-NN；这台线路机上已被占用就带团队号
        let emails: Vec<String> = snap.config["inbounds"].as_array().into_iter().flatten()
            .flat_map(|i| i["settings"]["clients"].as_array().cloned().unwrap_or_default())
            .filter_map(|c| c["email"].as_str().map(String::from)).collect();
        let prev = self.db.asset_by_row(&self.row.id).map_err(Self::dberr)?;
        let email = match prev.as_ref().and_then(|a| a["email"].as_str().map(String::from)) {
            Some(e) => e,
            None => {
                let e1 = format!("node-{:02}", s.number);
                if emails.contains(&e1) { format!("t{}-{:02}", s.team.chars().filter(|c| c.is_ascii_digit()).collect::<String>(), s.number) } else { e1 }
            }
        };
        let asset_uuid = prev.as_ref().and_then(|a| a["uuid"].as_str().map(String::from)).unwrap_or_else(uuid_v4);
        let e = relay::build_entry(&self.rc, &self.row.id, &s.name, port, &email, &s.ip, s.ss_public_port, s.nat, &self.cfg.ss.method,
            &relay::EntrySecrets { uuid: &uuid, private_key: &sk, short_id: &sid, ss_password: &self.ss_password()? });
        let asset = json!({"uuid": asset_uuid, "row_id": self.row.id, "name": s.name, "short_name": s.short_name, "team": s.team, "number": s.number,
            "provider": s.provider, "provider_code": s.provider_code, "plan_id": s.plan_id, "price": s.price, "currency": s.currency, "service_id": s.service_id,
            "ip": s.ip, "ssh_port": s.ssh_port, "ss_public_port": s.ss_public_port, "nat": s.nat, "relay": self.rc.name, "relay_port": port,
            "inbound_tag": e.inbound_tag, "outbound_tag": e.outbound_tag, "rule_tag": e.rule_tag, "email": email,
            "ss_impl": format!("shadowsocks-rust {}", self.ss.version), "ss_method": self.cfg.ss.method, "lifecycle": "onboarding"});
        self.db.upsert_asset(&asset).map_err(|m| StepErr { class: ErrClass::Conflict, msg: format!("登记资产失败（编号或端口冲突？）：{m}") })?;
        let _ = self.db.secret_set(&self.row.id, "reality_public_key", &pk);
        Ok(Outcome::Done(json!({"asset_uuid": asset_uuid, "name": s.name, "plan": s.plan_name, "relay": self.rc.name, "relay_port": port,
            "inbound_tag": e.inbound_tag, "outbound_tag": e.outbound_tag, "rule_tag": e.rule_tag, "email": email, "secrets": ["client_uuid", "reality_private_key", "short_id"]})))
    }

    pub fn entry(&self) -> Result<relay::Entry, StepErr> {
        let a = self.db.asset_by_row(&self.row.id).map_err(Self::dberr)?.ok_or_else(|| StepErr { class: ErrClass::Bug, msg: "还没登记（P10）".into() })?;
        let sec = |k: &str| self.db.secret(&self.row.id, k).map_err(Self::dberr)?.ok_or_else(|| StepErr { class: ErrClass::Bug, msg: format!("缺机密 {k}") });
        let s = &self.row.spec;
        Ok(relay::build_entry(&self.rc, &self.row.id, &s.name, a["relay_port"].as_i64().unwrap_or(0) as u16, a["email"].as_str().unwrap_or(""), &s.ip, s.ss_public_port, s.nat, &self.cfg.ss.method,
            &relay::EntrySecrets { uuid: &sec("client_uuid")?, private_key: &sec("reality_private_key")?, short_id: &sec("short_id")?, ss_password: &self.ss_password()? }))
    }

    // ── P11 装 nodeagent（fleet-edge；与 deploy/rollout.sh 的节点安装逐项一致）────────────
    // 预检（只读）→ 令牌（沿用 / 签发）→ 用户与目录 → 二进制（sha256 校验）→ 配置、令牌 0600、单元 → 以服务用户干跑
    // → 切 current、enable、按需 restart → 等第一批被中控接收。每一项都「内容不同才改」，重跑不会重复任何东西。
    pub async fn p11_agent(&self) -> Result<Outcome, StepErr> {
        let Some(a) = self.cfg.agent.as_ref() else {
            return Ok(Outcome::Skipped("onboard.toml 没有 [agent] 这一节：不装 nodeagent（配置见 WIRING.md §5.1）".into()));
        };
        let Some(tokens) = self.tokens.clone() else {
            return Ok(Outcome::Skipped(
                "没有令牌后端：fleetctl 独立运行且没配 [agent].token_cmd，不装 nodeagent。请在 fleetd 里执行，或配置 token_cmd 后从 P11 重试".into(),
            ));
        };
        let bin = match &self.edge {
            Some(b) => b.clone(),
            None => Arc::new(agent::prepare(a).map_err(|m| StepErr { class: ErrClass::Bug, msg: format!("nodeagent 安装包不可用：{m}") })?),
        };
        let host = self.row.spec.short_name.clone();
        let since = crate::db::now_ms() as f64 / 1000.0; // 中控时钟：host_latest.received_at 也是中控时钟
        let n = self.node();
        let rel = bin.rel();

        // 1. 预检（只读）
        let f = agent::kv(&n.run_ok(&agent::preflight_script(a, &rel), Self::t(30), ErrClass::Transient).await?);
        let g = |k: &str| f.get(k).map(String::as_str).unwrap_or("");
        if g("arch") != "x86_64" {
            return err(ErrClass::Provider, format!("架构 {} 不支持（nodeagent 只有 x86_64）", g("arch")));
        }
        if g("systemd_running") != "1" || g("systemd").parse::<u32>().unwrap_or(0) < 239 {
            return err(ErrClass::Provider, format!("需要 systemd ≥ 239 且在运行（systemd={}，运行中={}）", g("systemd"), g("systemd_running")));
        }
        if g("runuser") != "1" {
            return err(ErrClass::Provider, "节点上没有 runuser（util-linux）");
        }
        if g("relayd") == "1" {
            return err(ErrClass::Conflict, "这台机器装着 fleet-relayd（是线路机？）：不在上面装 nodeagent");
        }
        if !g("cur_host").is_empty() && g("cur_host") != host {
            return err(ErrClass::Conflict, format!("这台机器的 /etc/fleet-edge/agent.toml 已登记为「{}」，不是「{host}」：先核实是不是在用的节点", g("cur_host")));
        }
        let (eh, ep) = agent::endpoint_host_port(&a.endpoint);
        if g("out") != "ok" {
            return err(ErrClass::Provider, format!("节点到 {eh}:{ep} 的出站不通（供应商防火墙 / nftables？）：nodeagent 无法上报"));
        }
        let boot = g("boot_id").to_string();
        if boot.is_empty() {
            return err(ErrClass::Transient, "读不到节点的 boot_id");
        }

        // 2. 令牌：节点上已有且仍有效就沿用；否则（先撤销这个 host_id 的孤儿令牌再）签发
        let node_sha = Some(g("token_sha")).filter(|s| s.len() == 64);
        let view = tokens.view(&host).map_err(|m| StepErr { class: ErrClass::Transient, msg: format!("查 fleetd-ingest 失败：{m}") })?;
        let recorded = self.outputs("P11")["token_sha256"].as_str().map(String::from);
        let can_revoke = view.is_some() || a.revoke_cmd.is_some();
        let (token, token_sha, revoked) = match agent::plan_token(&host, node_sha, recorded.as_deref(), view.as_ref(), &boot, since, can_revoke) {
            TokenPlan::Conflict(m) => return err(ErrClass::Conflict, m),
            TokenPlan::Reuse => (None, node_sha.unwrap_or("").to_string(), 0),
            TokenPlan::Issue { revoke_first } => {
                let revoked = if revoke_first {
                    tokens.revoke(&host).map_err(|m| StepErr { class: ErrClass::Transient, msg: format!("撤销旧令牌失败：{m}") })?
                } else {
                    0
                };
                let t = tokens.issue(&host).map_err(|m| StepErr { class: ErrClass::Transient, msg: format!("签发令牌失败：{m}") })?;
                let sha = hex::encode(Sha256::digest(t.as_bytes()));
                // 先记下 sha256（不记明文）：这一步后面失败了，重跑时能认出节点上的令牌是不是这一个
                self.db.step_outputs_merge(&self.row.id, "P11", &json!({"token_sha256": sha, "token_issued_at": since})).map_err(Self::dberr)?;
                (Some(t), sha, revoked)
            }
        };
        let issued = token.is_some();

        // 3. 用户与目录 → 二进制（版本目录已存在且 sha256 相同就不传）
        n.run_ok(&agent::prepare_script(&rel), Self::t(30), ErrClass::Transient).await?;
        let pushed = g("have_bin") != bin.sha256;
        if pushed {
            n.push_file(&bin.path, &format!("{rel}/nodeagent"), "0755", &bin.sha256).await?;
        }

        // 4. 配置、令牌、单元、干跑、启动（一个脚本；令牌只在脚本正文里，经 stdin 进 bash）
        let o = n.run(&agent::install_script(a, &bin, &host, g("cgroup"), token.as_deref()), Self::t(120)).await?;
        drop(token);
        if o.code == 9 {
            let why: String = o.stdout.split("==ONCE").nth(1).unwrap_or("").trim().chars().take(400).collect();
            return err(ErrClass::Bug, format!("以 fleet-edge 身份干跑 nodeagent --once 失败（配置或令牌不可读）：{why}"));
        }
        if !o.ok() {
            let tail: String = o.stderr.trim().chars().rev().take(400).collect::<String>().chars().rev().collect();
            return err(ErrClass::Transient, format!("安装 nodeagent 失败（rc={}）：{tail}", o.code));
        }
        let r = agent::kv(&o.stdout);
        let t0_node: f64 = r.get("t0").and_then(|x| x.parse().ok()).unwrap_or(0.0);
        let svc = r.get("svc").cloned().unwrap_or_default();

        // 5. 等第一批被中控接收
        let deadline = tokio::time::Instant::now() + Self::t(a.first_batch_timeout_s);
        let (received_at, verified_by) = loop {
            tokio::time::sleep(Self::t(3)).await;
            let v = tokens.view(&host).map_err(|m| StepErr { class: ErrClass::Transient, msg: format!("查 fleetd-ingest 失败：{m}") })?;
            if let Some(v) = &v {
                if agent::first_batch_seen(v, since, &boot) {
                    break (v.received_at, "fleetd-ingest host_latest（接收时刻晚于安装开始，boot_id 与节点一致）");
                }
            }
            // 看不到 ingest（命令模式），或 ingest 收到了但 boot_id 对不上（旧样本时刻更晚时 boot_id 不更新）：以节点状态文件为准
            if v.as_ref().is_none_or(|v| v.received_at.is_some_and(|t| t >= since)) {
                let st = n.run(agent::STATUS_SCRIPT, Self::t(20)).await.map(|o| o.stdout).unwrap_or_default();
                if agent::status_last_ok(&st).is_some_and(|t| t >= t0_node) {
                    break (v.and_then(|v| v.received_at), "节点 /var/lib/fleet-nodeagent/status.json 的 last_ok 晚于启动时刻");
                }
            }
            if tokio::time::Instant::now() > deadline {
                let st = n.run(agent::STATUS_SCRIPT, Self::t(20)).await.map(|o| o.stdout).unwrap_or_default();
                let jr = n.run(agent::JOURNAL_SCRIPT, Self::t(20)).await.map(|o| o.stdout).unwrap_or_default();
                let tail: String = jr.lines().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join(" | ");
                return err(
                    ErrClass::Transient,
                    format!("{} 秒内中控没有收到 {host} 的第一批。status={} 日志：{}", a.first_batch_timeout_s, st.trim().chars().take(300).collect::<String>(), tail.chars().take(500).collect::<String>()),
                );
            }
        };
        let took = (crate::db::now_ms() as f64 / 1000.0 - since).round();
        let token_word = if issued { if revoked > 0 { format!("新签发（先撤销孤儿令牌 {revoked} 个）") } else { "新签发".to_string() } } else { "沿用节点上的有效令牌".to_string() };
        Ok(Outcome::Done(json!({
            "host_id": host, "version": bin.version, "binary_sha256": &bin.sha256[..16], "endpoint": a.endpoint,
            "token": if issued { "issued" } else { "reused" }, "token_sha256": token_sha, "token_source": tokens.describe(), "revoked_before_issue": revoked,
            "binary_uploaded": pushed, "service": svc, "systemd": g("systemd"), "cgroup": g("cgroup"), "os": g("os"),
            "first_batch_received_at": received_at, "verified_by": verified_by, "elapsed_s": took,
            "note": format!("nodeagent {} · 首批已被中控接收（{took} 秒）· 令牌{token_word}", bin.version),
        })))
    }

    // ── P12 出站校验（线路机侧，进程独立、UDS、不占端口）──────────────────────
    pub async fn p12_outbound_check(&self) -> Result<Outcome, StepErr> {
        let ob = self.ss_outbound()?;
        let ip1 = relay::check_outbound(&self.relay(), &self.rc, &ob, &self.cfg.egress_check_url).await?;
        if ip1 != self.row.spec.ip {
            // 用另一次独立进程复测，两次一致才定性（避免偶发误判成身份问题）
            let ip2 = relay::check_outbound(&self.relay(), &self.rc, &ob, &self.cfg.egress_check_url).await?;
            if ip2 != self.row.spec.ip {
                return err(ErrClass::Identity, format!("经 SS 的出口 IP 是 {ip2}（两次一致），不是 {}：应答 8388 的可能不是这台机器", self.row.spec.ip));
            }
        }
        Ok(Outcome::Done(json!({"egress_ip": self.row.spec.ip, "via": self.rc.name, "url": self.cfg.egress_check_url})))
    }

    pub async fn p14_probe(&self) -> Result<Outcome, StepErr> {
        Ok(Outcome::Skipped("探测随入口自动派生（relayd 上线后生效）；今晚由 P15 主动验收代替".into()))
    }

    // ── P15 端到端验收：中控起 VLESS REALITY 客户端 → 线路机公网入口 → 节点 → 出口 ──────
    pub async fn p15_e2e(&self) -> Result<Outcome, StepErr> {
        let a = self.db.asset_by_row(&self.row.id).map_err(Self::dberr)?.ok_or_else(|| StepErr { class: ErrClass::Bug, msg: "没有资产".into() })?;
        let sec = |k: &str| self.db.secret(&self.row.id, k).ok().flatten().unwrap_or_default();
        let (host, port) = relay::url_host_port(&self.cfg.egress_check_url);
        let cfg = json!({"log": {"loglevel": "warning"},
            "inbounds": [{"listen": "@@SOCK@@", "protocol": "dokodemo-door", "settings": {"address": host, "port": port, "network": "tcp"}, "tag": "in"}],
            "outbounds": [{"protocol": "vless", "tag": "entry", "settings": {"vnext": [{"address": self.rc.public_host, "port": a["relay_port"],
                "users": [{"id": sec("client_uuid"), "flow": "xtls-rprx-vision", "encryption": "none"}]}]},
                "streamSettings": {"network": "tcp", "security": "reality", "realitySettings": {"fingerprint": "chrome", "serverName": self.rc.sni,
                    "publicKey": sec("reality_public_key"), "shortId": sec("short_id")}}}]});
        let script = format!(
            "set -u\nT=$(mktemp -d); XP=''; trap 'kill $XP 2>/dev/null; rm -rf \"$T\"' EXIT\n\
             echo {} | base64 -d | sed \"s#@@SOCK@@#$T/s.sock#\" > \"$T/c.json\"\n\
             {} run -c \"$T/c.json\" > \"$T/log\" 2>&1 & XP=$!\n\
             for n in $(seq 1 50); do [ -S \"$T/s.sock\" ] && break; sleep 0.1; done\n\
             for i in 1 2 3; do R=$(curl -sS -m 15 --unix-socket \"$T/s.sock\" {} 2>&1) && echo \"$R\" | grep -q '^ip=' && {{ echo \"$R\"; exit 0; }}; sleep 1; done\n\
             echo \"==ERR $R\"; tail -c 300 \"$T/log\"; exit 4\n",
            base64::engine::general_purpose::STANDARD.encode(cfg.to_string()),
            sq(&self.cfg.client_xray.display().to_string()),
            sq(&self.cfg.egress_check_url)
        );
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c").arg(script).kill_on_drop(true);
        let o = tokio::time::timeout(Self::t(90), cmd.output()).await
            .map_err(|_| StepErr { class: ErrClass::Transient, msg: "端到端验收超时".into() })?
            .map_err(|e| StepErr { class: ErrClass::Bug, msg: format!("起本机 xray 客户端失败：{e}") })?;
        let so = String::from_utf8_lossy(&o.stdout);
        let ip = relay::parse_trace_ip(&so);
        match ip {
            Some(ip) if ip == self.row.spec.ip => Ok(Outcome::Done(json!({"egress_ip": ip, "path": format!("中控 → {}:{} → {} → 出口", self.rc.public_host, a["relay_port"], self.row.spec.ip)}))),
            Some(ip) => err(ErrClass::Identity, format!("端到端出口是 {ip}，不是 {}（规则没生效会走线路机 IP）", self.row.spec.ip)),
            None => err(ErrClass::Transient, format!("端到端请求失败：{}", so.trim().chars().take(300).collect::<String>())),
        }
    }

    // ── P16 上线交付 ─────────────────────────────────────────────────────────
    pub async fn p16_activate(&self) -> Result<Outcome, StepErr> {
        let a = self.db.asset_by_row(&self.row.id).map_err(Self::dberr)?.ok_or_else(|| StepErr { class: ErrClass::Bug, msg: "没有资产".into() })?;
        let sec = |k: &str| self.db.secret(&self.row.id, k).ok().flatten().unwrap_or_default();
        let name_enc: String = self.row.spec.name.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect();
        let link = format!(
            "vless://{}@{}:{}?encryption=none&flow=xtls-rprx-vision&security=reality&sni={}&fp=chrome&pbk={}&sid={}&type=tcp#{}",
            sec("client_uuid"), self.rc.public_host, a["relay_port"], self.rc.sni, sec("reality_public_key"), sec("short_id"), name_enc
        );
        self.db.secret_set(&self.row.id, "share_link", &link).map_err(Self::dberr)?;
        self.db.set_asset_lifecycle(&self.row.id, "active").map_err(Self::dberr)?;
        self.db.set_allocation(&self.row.id, "active").map_err(Self::dberr)?;
        Ok(Outcome::Done(json!({"lifecycle": "active", "share_link_ref": "secrets:share_link", "entry": format!("{}:{}", self.rc.public_host, a["relay_port"])})))
    }

    pub async fn exec(&self, step: &str) -> Outcome {
        let r = match step {
            "P1" => self.p1_preflight().await,
            "P2" => self.p2_bootstrap().await,
            "P3" => self.p3_identify().await,
            "P4" => self.p4_dd_prepare().await,
            "P5" => self.p5_dd_execute().await,
            "P6" => self.p6_dd_verify().await,
            "P7" => self.p7_harden().await,
            "P8" => self.p8_firewall().await,
            "P9" => self.p9_ss().await,
            "P10" => self.p10_register().await,
            "P11" => self.p11_agent().await,
            "P12" => self.p12_outbound_check().await,
            "P14" => self.p14_probe().await,
            "P15" => self.p15_e2e().await,
            "P16" => self.p16_activate().await,
            o => err(ErrClass::Bug, format!("步骤 {o} 不由节点执行器执行")),
        };
        r.unwrap_or_else(Outcome::from)
    }

    // ── 补偿（放弃/取消一行时按逆序执行）──────────────────────────────────────
    pub async fn undo(&self, step: &str) -> Result<String, StepErr> {
        match step {
            "P13" => {
                let e = self.entry()?;
                relay::detach(&self.relay(), &self.rc, &e).await.map(|v| format!("已从线路机撤下 {}", v["removed"]))
            }
            "P11" => {
                // 先停服务、删文件（rollout.sh --remove 的节点部分），再撤销令牌：顺序反过来 agent 会在被删前刷 401
                let host = self.row.spec.short_name.clone();
                let remote = self.node().run_ok(agent::REMOVE_SCRIPT, Self::t(60), ErrClass::Transient).await;
                let rev = match &self.tokens {
                    Some(t) => match t.revoke(&host) {
                        Ok(n) => format!("已撤销 {host} 的令牌 {n} 个"),
                        Err(e) => format!("撤销令牌失败：{e}（请手动执行 fleetd agent-token revoke '{host}'）"),
                    },
                    None => format!("没有令牌后端，令牌未撤销：请手动执行 fleetd agent-token revoke '{host}'"),
                };
                match remote {
                    Ok(out) => Ok(format!("已停用并删除 nodeagent（{}）；{rev}", out.trim().replace('\n', " "))),
                    Err(e) => Err(StepErr { class: e.class, msg: format!("节点上卸载 nodeagent 失败：{}；{rev}", e.msg) }),
                }
            }
            "P10" => {
                self.db.delete_asset(&self.row.id).map_err(Self::dberr)?;
                self.db.drop_allocation(&self.row.id).map_err(Self::dberr)?;
                Ok("已删除资产、释放端口预留（端口不复用）".into())
            }
            "P9" => {
                self.node()
                    .run_ok("systemctl disable --now fleet-ssserver 2>/dev/null || true; pkill -f '^/usr/local/bin/ssserver -c /etc/shadowsocks-rust/config.json' || true; rm -f /etc/systemd/system/fleet-ssserver.service /usr/local/bin/ssserver; rm -rf /etc/shadowsocks-rust; [ -d /run/systemd/system ] && systemctl daemon-reload || true", Self::t(30), ErrClass::Transient)
                    .await?;
                Ok("已停用并删除 ssserver".into())
            }
            "P8" => {
                self.node()
                    .run_ok("nft delete table inet ss_guard 2>/dev/null || true; rm -f /etc/fleet/nft/ss_guard.nft; [ -f /etc/nftables.conf ] && sed -i '\\#include \"/etc/fleet/nft/ss_guard.nft\"#d' /etc/nftables.conf || true", Self::t(30), ErrClass::Transient)
                    .await?;
                Ok("已删除 ss_guard 表".into())
            }
            "P2" => {
                // P7 之后机器已是「仅密钥」，删掉上架公钥就失联了；DD 之后的机器本来就归我们。只在 P7 之前撤。
                if self.db.step(&self.row.id, "P7").ok().flatten().is_some_and(|s| s.status == cell::SUCCEEDED) {
                    return Ok("保留上架公钥（已加固为仅密钥，删掉会失联）".into());
                }
                let k = self.pubkey()?;
                self.node().run_ok(&format!("grep -vxF {} ~/.ssh/authorized_keys > ~/.ssh/ak.tmp || true; mv -f ~/.ssh/ak.tmp ~/.ssh/authorized_keys; chmod 600 ~/.ssh/authorized_keys", sq(&k)), Self::t(20), ErrClass::Transient).await?;
                Ok("已删除上架公钥".into())
            }
            _ => Ok(String::new()),
        }
    }
}
