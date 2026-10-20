//! P11 装 nodeagent：令牌后端、安装包、远端脚本（与 fleet-edge/deploy/rollout.sh 的节点安装逐项一致）。
//!
//! 令牌后端（[`AgentTokens`]）：
//! - fleetd 进程内：fleetd 把自己的 fleetd-ingest 注入进来（`OnboardApi::with_agent_tokens`），签发/撤销走
//!   `Ingest::issue_token` / `revoke_tokens`，核对（令牌是否有效、第一批是否已接收）用 [`HostView::query`] 读同一个库。
//!   本 crate 自己不打开 ingest.db。
//! - fleetctl 独立运行：`[agent].token_cmd` / `revoke_cmd`（例如 `runuser -u fleetd -- fleetd agent-token issue …`）；
//!   看不到 ingest 库，所以复用令牌只认「上次由本流水线签发的那个」（按 sha256），第一批以节点上
//!   `/var/lib/fleet-nodeagent/status.json` 的 `last_ok` 为准（与 rollout.sh 相同）。两个命令都没配 = P11 跳过并说明。
//!
//! 远端布局（rollout.sh 同款，之后用 rollout.sh 升级/卸载可以无缝接手）：
//! `/opt/fleet-edge/releases/<VERSION>-<sha8>/nodeagent`（0755）、`/opt/fleet-edge/current` → 该目录、
//! `/etc/fleet-edge/agent.toml`（0644）、`/etc/fleet-edge/token`（0600 fleet-edge）、
//! `/etc/systemd/system/fleet-nodeagent.service` + `.service.d/10-local.conf`、系统用户 `fleet-edge`（无 shell）。

use crate::config::AgentCfg;
use crate::ssh::sq;
use base64::Engine as _;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

/// 本流水线签发的令牌在 agent_tokens.name 里的名字（rollout.sh 签发的是 default）。
pub const TOKEN_NAME: &str = "onboard";
pub const UNIT: &str = "fleet-nodeagent.service";
/// 内置单元文件（fleet-edge/deploy/fleet-nodeagent.service 的逐字节拷贝，见测试 `embedded_unit_matches_fleet_edge`）。
pub const UNIT_EMBEDDED: &str = include_str!("../assets/fleet-nodeagent.service");
/// 预检时认为 host_id「正被另一台机器使用」的时间窗：这段时间内有别的 boot_id 的批次被接收。
pub const LIVE_WINDOW_S: f64 = 300.0;

/// fleetd-ingest 里某个 host_id 的现状。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HostView {
    /// 未撤销令牌的 sha256（十六进制小写）
    pub active: Vec<String>,
    /// host_latest.received_at：最近一次接收批次的中控时刻（unix 秒）
    pub received_at: Option<f64>,
    /// host_latest.boot_id：最新样本那一批的 boot_id
    pub boot_id: Option<String>,
}

impl HostView {
    /// 在 fleetd-ingest 的库（agent_tokens / host_latest）上只读查询。fleetd 里这样接：
    /// `ingest.with_db(|c| onboard::agent::HostView::query(c, host))`
    pub fn query(c: &rusqlite::Connection, host: &str) -> Result<HostView, String> {
        use rusqlite::OptionalExtension;
        let mut st = c.prepare("SELECT sha256 FROM agent_tokens WHERE host_id=?1 AND revoked_at IS NULL ORDER BY id").map_err(|e| e.to_string())?;
        let active = st.query_map([host], |r| r.get::<_, String>(0)).map_err(|e| e.to_string())?.collect::<rusqlite::Result<Vec<_>>>().map_err(|e| e.to_string())?;
        let latest: Option<(f64, String)> = c
            .query_row("SELECT received_at, boot_id FROM host_latest WHERE host_id=?1", [host], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()
            .map_err(|e| e.to_string())?;
        Ok(HostView { active, received_at: latest.as_ref().map(|x| x.0), boot_id: latest.map(|x| x.1) })
    }
}

/// 令牌后端。所有调用都很快（本机 SQLite 或一条本机命令），直接在异步步骤里同步调用。
pub trait AgentTokens: Send + Sync {
    /// 为 host_id 签发一个新令牌，返回明文（只此一次；调用方不落盘、不进日志）。
    fn issue(&self, host_id: &str) -> Result<String, String>;
    /// 撤销该 host_id 的全部未撤销令牌，返回撤销个数（撤销 = 下线主机，fleetd 不再对它告警）。
    fn revoke(&self, host_id: &str) -> Result<usize, String>;
    /// fleetd-ingest 里的现状；None = 这个后端看不到（命令模式）。
    fn view(&self, host_id: &str) -> Result<Option<HostView>, String>;
    /// 给人看的来源说明（写进步骤输出）。
    fn describe(&self) -> String;
}

/// fleetctl 独立运行时：用配置里的命令签发 / 撤销。
pub struct CmdTokens {
    pub issue_cmd: String,
    pub revoke_cmd: Option<String>,
}

fn run_cmd(tpl: &str, host: &str) -> Result<(i32, String, String), String> {
    let cmd = tpl.replace("{host_id}", &sq(host));
    let o = std::process::Command::new("sh").arg("-c").arg(&cmd).stdin(std::process::Stdio::null()).output().map_err(|e| format!("执行令牌命令失败：{e}"))?;
    Ok((o.status.code().unwrap_or(-1), String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned()))
}

/// rollout.sh 的格式检查：去掉空白后 ≥ 20 字节、全是可见 ASCII。
pub fn check_token(t: &str) -> Result<String, String> {
    let t: String = t.chars().filter(|c| !c.is_whitespace()).collect();
    if t.len() < 20 || !t.bytes().all(|b| b.is_ascii_graphic()) {
        return Err("签发的令牌格式不对（应为 fe1_ 开头的一行）".into());
    }
    Ok(t)
}

impl AgentTokens for CmdTokens {
    fn issue(&self, host: &str) -> Result<String, String> {
        let (code, out, err) = run_cmd(&self.issue_cmd, host)?;
        if code != 0 {
            return Err(format!("签发令牌命令退出码 {code}：{}", err.trim().chars().take(300).collect::<String>()));
        }
        check_token(&out)
    }
    fn revoke(&self, host: &str) -> Result<usize, String> {
        let Some(c) = &self.revoke_cmd else { return Err("没有配置 [agent].revoke_cmd".into()) };
        let (code, out, err) = run_cmd(c, host)?;
        // fleetd agent-token revoke：0 = 撤销了若干个，3 = 没有可撤销的
        match code {
            0 => Ok(out.split_whitespace().find_map(|w| w.parse::<usize>().ok()).unwrap_or(1)),
            3 => Ok(0),
            _ => Err(format!("撤销令牌命令退出码 {code}：{}", err.trim().chars().take(300).collect::<String>())),
        }
    }
    fn view(&self, _host: &str) -> Result<Option<HostView>, String> {
        Ok(None)
    }
    fn describe(&self) -> String {
        "fleetctl 独立运行：[agent].token_cmd".into()
    }
}

/// 按配置得出独立运行时的后端（fleetd 里会被注入的后端覆盖）。
pub fn from_config(a: Option<&AgentCfg>) -> Option<std::sync::Arc<dyn AgentTokens>> {
    let a = a?;
    let issue_cmd = a.token_cmd.clone()?;
    Some(std::sync::Arc::new(CmdTokens { issue_cmd, revoke_cmd: a.revoke_cmd.clone() }))
}

/// 已校验的 nodeagent 安装包。
#[derive(Debug, Clone)]
pub struct AgentBin {
    pub path: PathBuf,
    pub sha256: String,
    /// rollout.sh 的版本目录名：`<VERSION>-<sha256 前 8 位>`
    pub version: String,
    pub unit: String,
}

impl AgentBin {
    pub fn rel(&self) -> String {
        format!("/opt/fleet-edge/releases/{}", self.version)
    }
}

/// 读 `<dist>/nodeagent`，按 `<dist>/SHA256SUMS` 校验；版本号取 `<dist>/VERSION`（缺省 0.1.0，与 rollout.sh 相同）。
pub fn prepare(a: &AgentCfg) -> Result<AgentBin, String> {
    let p = a.dist.join("nodeagent");
    let data = std::fs::read(&p).map_err(|e| format!("读 {}：{e}（把 fleet-edge/dist/current/ 拷到 [agent].dist）", p.display()))?;
    let sha = hex::encode(Sha256::digest(&data));
    let sums_p = a.dist.join("SHA256SUMS");
    let sums = std::fs::read_to_string(&sums_p).map_err(|e| format!("读 {}：{e}", sums_p.display()))?;
    let want = sums.lines().find_map(|l| {
        let mut it = l.split_whitespace();
        let h = it.next()?;
        let n = it.next()?.trim_start_matches('*');
        (n == "nodeagent").then(|| h.to_ascii_lowercase())
    });
    match want {
        None => return Err(format!("{} 里没有 nodeagent 这一行", sums_p.display())),
        Some(w) if w != sha => return Err(format!("{} 的 sha256 是 {sha}，与 SHA256SUMS 的 {w} 不符", p.display())),
        _ => {}
    }
    if data.len() < 4 || &data[..4] != b"\x7fELF" {
        return Err(format!("{} 不是 ELF 可执行文件", p.display()));
    }
    let ver = std::fs::read_to_string(a.dist.join("VERSION")).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| "0.1.0".into());
    if !ver.chars().all(|c| c.is_ascii_alphanumeric() || ".-_+".contains(c)) {
        return Err(format!("VERSION 内容不合法：{ver}"));
    }
    let unit = match &a.unit_file {
        Some(u) => std::fs::read_to_string(u).map_err(|e| format!("读单元文件 {}：{e}", u.display()))?,
        None => UNIT_EMBEDDED.to_string(),
    };
    if !unit.contains("ExecStart=/opt/fleet-edge/current/nodeagent") {
        return Err("单元文件的 ExecStart 不是 /opt/fleet-edge/current/nodeagent".into());
    }
    Ok(AgentBin { path: p, version: format!("{ver}-{}", &sha[..8]), sha256: sha, unit })
}

/// TOML 基本字符串转义（普通 host_id 输出与 rollout.sh 的 `"$HOST_ID"` 逐字节相同）。
fn toml_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => o.push_str(&format!("\\u{:04X}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// /etc/fleet-edge/agent.toml。与 rollout.sh（节点）生成的逐字节相同——连首行注释也一样，
/// 这样之后用 rollout.sh 升级时 cmp 相同、不会无谓重启。
pub fn conf_text(a: &AgentCfg, host_id: &str) -> String {
    format!(
        "# 由 fleet-edge/deploy/rollout.sh 生成（内容不变就不重启）\nendpoint = {}\nhost_id = {}\ntoken_file = \"/etc/fleet-edge/token\"\nsample_interval_s = {}\nbatch_samples = {}\n",
        toml_str(&a.endpoint),
        toml_str(host_id),
        a.sample_interval_s,
        a.batch_samples
    )
}

/// `10-local.conf`：与 rollout.sh 相同（cgroup v1 的主机补 MemoryLimit，v2 只有注释行）。
pub fn dropin_text(cgroup: &str) -> String {
    let mut d = String::from("# 由 rollout.sh 生成");
    if cgroup != "cgroup2fs" {
        d.push_str("\n[Service]\nMemoryLimit=24M");
    }
    d.push('\n');
    d
}

/// 上报地址的 host 与端口（预检出站用；rollout.sh 固定查 443）。
pub fn endpoint_host_port(ep: &str) -> (String, u16) {
    let (scheme, rest) = ep.split_once("://").unwrap_or(("https", ep));
    let hp = rest.split('/').next().unwrap_or("");
    let dflt = if scheme == "http" { 80 } else { 443 };
    if let Some(v6) = hp.strip_prefix('[') {
        let (h, tail) = v6.split_once(']').unwrap_or((v6, ""));
        let port = tail.strip_prefix(':').and_then(|p| p.parse().ok()).unwrap_or(dflt);
        return (h.to_string(), port);
    }
    match hp.rsplit_once(':') {
        Some((h, p)) if p.parse::<u16>().is_ok() => (h.to_string(), p.parse().unwrap()),
        _ => (hp.to_string(), dflt),
    }
}

/// 只读预检（rollout.sh 的 PRE 段 + 本流水线额外的身份检查）。
pub fn preflight_script(a: &AgentCfg, rel: &str) -> String {
    let (h, p) = endpoint_host_port(&a.endpoint);
    format!(
        r#"set -u
echo "arch=$(uname -m)"
echo "systemd=$(systemctl --version 2>/dev/null | awk 'NR==1{{print $2}}')"
echo "systemd_running=$([ -d /run/systemd/system ] && echo 1 || echo 0)"
echo "cgroup=$(stat -fc %T /sys/fs/cgroup 2>/dev/null)"
echo "os=$(. /etc/os-release 2>/dev/null; echo ${{ID:-?}}-${{VERSION_ID:-?}})"
if timeout 6 bash -c 'exec 3<>/dev/tcp/{h}/{p}' 2>/dev/null; then echo out=ok; else echo out=fail; fi
echo "token_sha=$([ -s /etc/fleet-edge/token ] && tr -d '[:space:]' < /etc/fleet-edge/token | sha256sum | cut -c1-64)"
echo "have_bin=$([ -x {rel}/nodeagent ] && sha256sum {rel}/nodeagent | cut -c1-64)"
echo "boot_id=$(cat /proc/sys/kernel/random/boot_id 2>/dev/null)"
echo "cur_host=$(sed -n 's/^host_id = "\(.*\)"$/\1/p' /etc/fleet-edge/agent.toml 2>/dev/null | head -1)"
echo "relayd=$([ -e /etc/systemd/system/fleet-relayd.service ] && echo 1 || echo 0)"
echo "runuser=$(command -v runuser >/dev/null && echo 1 || echo 0)"
echo "mem_avail_mb=$(awk '/MemAvailable/{{print int($2/1024)}}' /proc/meminfo)"
"#,
        h = h,
        p = p,
        rel = rel
    )
}

/// 用户与目录（rollout.sh 原样）+ 本次版本目录（0755，fleet-edge 用户要能执行）。
pub fn prepare_script(rel: &str) -> String {
    format!(
        r#"set -eu
getent group fleet-edge >/dev/null || groupadd --system fleet-edge
id fleet-edge >/dev/null 2>&1 || useradd --system --gid fleet-edge --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin fleet-edge
install -d -m 0755 -o root -g root /opt/fleet-edge /opt/fleet-edge/releases /etc/fleet-edge
install -d -m 0755 -o root -g root {rel}
"#
    )
}

/// 「内容不同才替换，并标记需要重启」（rollout.sh 的 cmp + mv + .restart）。内容用 base64 进脚本。
fn replace_if_changed(path: &str, content: &str, mode: &str) -> String {
    let b = base64::engine::general_purpose::STANDARD.encode(content.as_bytes());
    format!(
        "printf '%s' '{b}' | base64 -d > {p}.new && chmod {mode} {p}.new && if cmp -s {p}.new {p}; then rm -f {p}.new; else mv -f {p}.new {p}; echo changed > /etc/fleet-edge/.restart; fi\n",
        p = path
    )
}

/// 配置、令牌（只在新签发时写）、单元、drop-in、以服务用户干跑、切换 current 并启动（rollout.sh 同序同语义）。
/// 输出 `t0=<节点时刻>`、`svc=restarted|unchanged`；干跑失败退出码 9。
pub fn install_script(a: &AgentCfg, bin: &AgentBin, host_id: &str, cgroup: &str, token: Option<&str>) -> String {
    let rel = bin.rel();
    let mut s = String::from("set -eu\numask 022\n");
    s.push_str("# 配置\n");
    s.push_str(&replace_if_changed("/etc/fleet-edge/agent.toml", &conf_text(a, host_id), "0644"));
    if let Some(t) = token {
        // printf 是 bash 内建：令牌只经 ssh 的 stdin 进 bash，不上任何命令行
        s.push_str("# 令牌（0600 fleet-edge）\n");
        s.push_str(&format!(
            "( umask 077; printf '%s\\n' {} > /etc/fleet-edge/token.new && chown fleet-edge:fleet-edge /etc/fleet-edge/token.new && chmod 0600 /etc/fleet-edge/token.new && mv -f /etc/fleet-edge/token.new /etc/fleet-edge/token && echo changed > /etc/fleet-edge/.restart )\n",
            sq(t)
        ));
    }
    s.push_str("# 单元与 drop-in\n");
    s.push_str(&replace_if_changed(&format!("/etc/systemd/system/{UNIT}"), &bin.unit, "0644"));
    s.push_str(&format!("install -d -m 0755 /etc/systemd/system/{UNIT}.d\n"));
    s.push_str(&replace_if_changed(&format!("/etc/systemd/system/{UNIT}.d/10-local.conf"), &dropin_text(cgroup), "0644"));
    s.push_str(&format!(
        r#"# 以服务用户干跑一次（校验配置、令牌可读、采样正常）
E=$(mktemp)
set +e
runuser -u fleet-edge -- {rel}/nodeagent --once --config /etc/fleet-edge/agent.toml >/dev/null 2>"$E"; rc=$?
set -e
if [ $rc -ne 0 ] || ! grep -q '令牌文件可读' "$E"; then echo "==ONCE"; cat "$E"; rm -f "$E"; exit 9; fi
rm -f "$E"
echo "t0=$(date +%s)"
cur=$(readlink /opt/fleet-edge/current 2>/dev/null || true)
if [ "$cur" != "{rel}" ]; then ln -sfn {rel} /opt/fleet-edge/current.new && mv -Tf /opt/fleet-edge/current.new /opt/fleet-edge/current; echo changed > /etc/fleet-edge/.restart; fi
systemctl daemon-reload
systemctl enable {UNIT} >/dev/null 2>&1
if [ -f /etc/fleet-edge/.restart ] || ! systemctl is-active --quiet {UNIT}; then systemctl restart {UNIT}; echo svc=restarted; else echo svc=unchanged; fi
rm -f /etc/fleet-edge/.restart
# 只留最近 3 个版本
ls -1dt /opt/fleet-edge/releases/* 2>/dev/null | tail -n +4 | grep -v "^{rel}\$" | xargs -r rm -rf
echo "active=$(systemctl is-active {UNIT} 2>/dev/null)"
"#
    ));
    s
}

/// 节点上 nodeagent 的状态文件与最近日志（等第一批、超时诊断用）。
pub const STATUS_SCRIPT: &str = "cat /var/lib/fleet-nodeagent/status.json 2>/dev/null; echo; echo \"==ACTIVE $(systemctl is-active fleet-nodeagent.service 2>/dev/null)\"\n";
pub const JOURNAL_SCRIPT: &str = "journalctl -u fleet-nodeagent.service -n 15 --no-pager 2>/dev/null | tail -n 15\n";

/// 卸载（rollout.sh --remove 的节点部分）。装着 relayd 的机器（线路机）不删共用的 /opt/fleet-edge、/etc/fleet-edge 和用户。
pub const REMOVE_SCRIPT: &str = r#"set -u
systemctl disable --now fleet-nodeagent.service >/dev/null 2>&1 || true
rm -f /etc/systemd/system/fleet-nodeagent.service
rm -rf /etc/systemd/system/fleet-nodeagent.service.d /var/lib/fleet-nodeagent /var/lib/private/fleet-nodeagent
[ -d /run/systemd/system ] && systemctl daemon-reload || true
systemctl reset-failed fleet-nodeagent.service >/dev/null 2>&1 || true
if [ -e /etc/systemd/system/fleet-relayd.service ]; then
  rm -f /etc/fleet-edge/agent.toml
  echo "kept=relayd"
else
  rm -rf /opt/fleet-edge /etc/fleet-edge
  id fleet-edge >/dev/null 2>&1 && userdel fleet-edge 2>/dev/null || true
  getent group fleet-edge >/dev/null && groupdel fleet-edge 2>/dev/null || true
fi
echo "remaining=$(ls -d /opt/fleet-edge /etc/fleet-edge /var/lib/fleet-nodeagent 2>/dev/null | wc -l) unit=$(systemctl list-unit-files fleet-nodeagent.service --no-legend 2>/dev/null | wc -l)"
"#;

/// 令牌怎么处理。
#[derive(Debug, PartialEq)]
pub enum TokenPlan {
    /// 节点上的令牌仍然有效（fleetd 能核对），或是上次由本流水线签发的（命令模式）：沿用，不改节点上的令牌。
    Reuse,
    /// 签发新令牌；revoke_first = 先撤销这个 host_id 的旧令牌（它们不在这台机器上，留着只会是孤儿）。
    Issue { revoke_first: bool },
    /// host_id 正被另一台机器使用：风险停止。
    Conflict(String),
}

pub fn plan_token(host_id: &str, node_sha: Option<&str>, recorded_sha: Option<&str>, view: Option<&HostView>, node_boot: &str, now: f64, can_revoke: bool) -> TokenPlan {
    match view {
        Some(v) => {
            if node_sha.is_some_and(|s| v.active.iter().any(|a| a == s)) {
                return TokenPlan::Reuse;
            }
            let live_elsewhere = !v.active.is_empty()
                && v.received_at.is_some_and(|t| now - t < LIVE_WINDOW_S)
                && v.boot_id.as_deref().is_some_and(|b| b != node_boot);
            if live_elsewhere {
                return TokenPlan::Conflict(format!(
                    "host_id「{host_id}」有 {} 个有效令牌，且 {:.0} 秒前还收到过另一台机器（boot_id {}）的上报，而这台机器上没有其中任何一个令牌：\
                     编号可能被在用的节点占着。确认那台已下线后执行 fleetd agent-token revoke '{host_id}'，再从 P11 重试",
                    v.active.len(),
                    now - v.received_at.unwrap_or(now),
                    v.boot_id.as_deref().unwrap_or("?").chars().take(8).collect::<String>()
                ));
            }
            TokenPlan::Issue { revoke_first: !v.active.is_empty() }
        }
        None => {
            if node_sha.is_some() && node_sha == recorded_sha {
                return TokenPlan::Reuse;
            }
            TokenPlan::Issue { revoke_first: can_revoke }
        }
    }
}

/// 第一批是否已被接收（fleetd 模式）：received_at 不早于本次安装开始，且来自这台机器（boot_id 相同）。
pub fn first_batch_seen(v: &HostView, since: f64, node_boot: &str) -> bool {
    v.received_at.is_some_and(|t| t >= since) && v.boot_id.as_deref() == Some(node_boot)
}

/// 命令模式：节点 status.json 的 last_ok 不早于节点上的 t0（rollout.sh 的判据）。
pub fn status_last_ok(status: &str) -> Option<f64> {
    let line = status.lines().find(|l| l.trim_start().starts_with('{'))?;
    serde_json::from_str::<serde_json::Value>(line).ok()?["last_ok"].as_f64()
}

pub fn kv(out: &str) -> std::collections::BTreeMap<String, String> {
    out.lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> AgentCfg {
        AgentCfg {
            dist: PathBuf::from("/nonexistent"),
            endpoint: "https://38.58.57.22".into(),
            sample_interval_s: 5,
            batch_samples: 2,
            first_batch_timeout_s: 90,
            unit_file: None,
            token_cmd: None,
            revoke_cmd: None,
        }
    }

    #[test]
    fn conf_matches_rollout() {
        // rollout.sh：CONF=$(cat <<EOF … EOF) 再 printf '%s\n'
        let want = "# 由 fleet-edge/deploy/rollout.sh 生成（内容不变就不重启）\nendpoint = \"https://38.58.57.22\"\nhost_id = \"团队1-37\"\ntoken_file = \"/etc/fleet-edge/token\"\nsample_interval_s = 5\nbatch_samples = 2\n";
        assert_eq!(conf_text(&cfg(), "团队1-37"), want);
        assert!(conf_text(&cfg(), "a\"b").contains("host_id = \"a\\\"b\""));
    }

    #[test]
    fn dropin_matches_rollout() {
        assert_eq!(dropin_text("cgroup2fs"), "# 由 rollout.sh 生成\n");
        assert_eq!(dropin_text("tmpfs"), "# 由 rollout.sh 生成\n[Service]\nMemoryLimit=24M\n");
    }

    #[test]
    fn embedded_unit_matches_fleet_edge() {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../fleet-edge/deploy/fleet-nodeagent.service");
        match std::fs::read_to_string(&p) {
            Ok(u) => assert_eq!(u, UNIT_EMBEDDED, "assets/fleet-nodeagent.service 与 {} 不一致：重新拷一份", p.display()),
            Err(_) => eprintln!("跳过：没有 {}（构建目录里没同步 fleet-edge/deploy）", p.display()),
        }
    }

    #[test]
    fn endpoint_parse() {
        assert_eq!(endpoint_host_port("https://38.58.57.22"), ("38.58.57.22".into(), 443));
        assert_eq!(endpoint_host_port("https://38.58.57.22/"), ("38.58.57.22".into(), 443));
        assert_eq!(endpoint_host_port("http://127.0.0.1:25911"), ("127.0.0.1".into(), 25911));
        assert_eq!(endpoint_host_port("https://[::1]:8443"), ("::1".into(), 8443));
    }

    #[test]
    fn validate_endpoint() {
        let mut c = cfg();
        assert!(c.validate().is_ok());
        c.endpoint = "http://38.58.57.22".into();
        assert!(c.validate().is_err());
        c.endpoint = "http://127.0.0.1:25911".into();
        assert!(c.validate().is_ok());
        c.token_cmd = Some("fleetd agent-token issue".into());
        assert!(c.validate().is_err());
    }

    #[test]
    fn token_plans() {
        let v = |active: &[&str], recv: Option<f64>, boot: Option<&str>| HostView { active: active.iter().map(|s| s.to_string()).collect(), received_at: recv, boot_id: boot.map(String::from) };
        let now = 10_000.0;
        // fleetd 模式
        assert_eq!(plan_token("h", Some("a"), None, Some(&v(&["a"], Some(now - 5.0), Some("B"))), "B", now, true), TokenPlan::Reuse);
        assert_eq!(plan_token("h", None, None, Some(&v(&[], None, None)), "B", now, true), TokenPlan::Issue { revoke_first: false });
        // 上次签发了但没写到节点：孤儿令牌，先撤销再签发
        assert_eq!(plan_token("h", None, Some("x"), Some(&v(&["x"], None, None)), "B", now, true), TokenPlan::Issue { revoke_first: true });
        // 节点上的令牌已被撤销
        assert_eq!(plan_token("h", Some("old"), None, Some(&v(&[], Some(now - 5.0), Some("B"))), "B", now, true), TokenPlan::Issue { revoke_first: false });
        // 另一台机器正在用这个 host_id
        assert!(matches!(plan_token("h", None, None, Some(&v(&["z"], Some(now - 20.0), Some("OTHER"))), "B", now, true), TokenPlan::Conflict(_)));
        // 另一台很久没报了：当孤儿处理
        assert_eq!(plan_token("h", None, None, Some(&v(&["z"], Some(now - 4000.0), Some("OTHER"))), "B", now, true), TokenPlan::Issue { revoke_first: true });
        // 命令模式：只认自己上次签发的
        assert_eq!(plan_token("h", Some("a"), Some("a"), None, "B", now, true), TokenPlan::Reuse);
        assert_eq!(plan_token("h", Some("a"), Some("b"), None, "B", now, false), TokenPlan::Issue { revoke_first: false });
        assert_eq!(plan_token("h", None, None, None, "B", now, true), TokenPlan::Issue { revoke_first: true });
    }

    #[test]
    fn first_batch() {
        let v = HostView { active: vec![], received_at: Some(100.0), boot_id: Some("B".into()) };
        assert!(first_batch_seen(&v, 99.0, "B"));
        assert!(!first_batch_seen(&v, 101.0, "B"));
        assert!(!first_batch_seen(&v, 99.0, "C"));
        assert_eq!(status_last_ok("{\"last_ok\": 1727150000.25, \"seq\": 3}\n==ACTIVE active"), Some(1727150000.25));
        assert_eq!(status_last_ok("\n==ACTIVE inactive"), None);
    }

    #[test]
    fn check_tokens() {
        assert_eq!(check_token("fe1_0123456789abcdef0123\n").unwrap(), "fe1_0123456789abcdef0123");
        assert!(check_token("short").is_err());
    }

    #[test]
    fn prepare_verifies_sha() {
        let d = std::env::temp_dir().join(format!("onb-agent-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let bin = b"\x7fELF-fake-nodeagent".to_vec();
        std::fs::write(d.join("nodeagent"), &bin).unwrap();
        let sha = hex::encode(Sha256::digest(&bin));
        std::fs::write(d.join("SHA256SUMS"), format!("{sha}  nodeagent\n{}  relayd\n", "0".repeat(64))).unwrap();
        std::fs::write(d.join("VERSION"), "0.1.0\n").unwrap();
        let mut c = cfg();
        c.dist = d.clone();
        let b = prepare(&c).unwrap();
        assert_eq!(b.version, format!("0.1.0-{}", &sha[..8]));
        assert_eq!(b.rel(), format!("/opt/fleet-edge/releases/0.1.0-{}", &sha[..8]));
        std::fs::write(d.join("SHA256SUMS"), format!("{}  nodeagent\n", "1".repeat(64))).unwrap();
        assert!(prepare(&c).unwrap_err().contains("不符"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn scripts_parse() {
        let bin = AgentBin { path: "/x".into(), sha256: "ab".repeat(32), version: "0.1.0-abababab".into(), unit: UNIT_EMBEDDED.into() };
        let c = cfg();
        for s in [
            preflight_script(&c, &bin.rel()),
            prepare_script(&bin.rel()),
            install_script(&c, &bin, "团队1-37", "cgroup2fs", Some("fe1_0123456789abcdef0123456789")),
            install_script(&c, &bin, "团队1-37", "tmpfs", None),
            REMOVE_SCRIPT.to_string(),
            STATUS_SCRIPT.to_string(),
        ] {
            let o = std::process::Command::new("bash").arg("-n").arg("-c").arg(&s).output().unwrap();
            assert!(o.status.success(), "bash -n 失败：{}\n{s}", String::from_utf8_lossy(&o.stderr));
        }
        let with = install_script(&c, &bin, "团队1-37", "cgroup2fs", Some("fe1_secret_token_value_0000"));
        assert!(with.contains("chmod 0600 /etc/fleet-edge/token.new"));
        assert!(!install_script(&c, &bin, "团队1-37", "cgroup2fs", None).contains("/etc/fleet-edge/token.new"));
    }

    /// 与 fleetd-ingest 的真实库结构一起测 HostView::query（fleetd 注入的后端就是这么查的）。
    #[test]
    fn host_view_against_ingest_schema() {
        let ing = fleetd_ingest::Ingest::open_memory(fleetd_ingest::IngestCfg::default());
        let v0 = ing.with_db(|c| HostView::query(c, "团队9-01")).unwrap();
        assert_eq!(v0, HostView::default());
        let t = ing.issue_token("团队9-01", TOKEN_NAME).unwrap();
        let v1 = ing.with_db(|c| HostView::query(c, "团队9-01")).unwrap();
        assert_eq!(v1.active, vec![hex::encode(Sha256::digest(t.as_bytes()))]);
        let now = fleetd_ingest::now();
        let batch = serde_json::json!({"v": 1, "kind": "node", "host_id": "团队9-01", "boot_id": "boot-A", "seq": 1, "agent": "nodeagent/0.1.0",
            "meta": {}, "outbox": {}, "samples": [{"ts": now, "cpu": {"pct": 1.0}, "mem": {"pct": 10.0}}]});
        ing.accept("团队9-01", "node", &batch, now).unwrap();
        let v2 = ing.with_db(|c| HostView::query(c, "团队9-01")).unwrap();
        assert!(first_batch_seen(&v2, now - 1.0, "boot-A"));
        assert_eq!(ing.revoke_tokens("团队9-01", None).unwrap(), 1);
        let v3 = ing.with_db(|c| HostView::query(c, "团队9-01")).unwrap();
        assert!(v3.active.is_empty());
    }
}
