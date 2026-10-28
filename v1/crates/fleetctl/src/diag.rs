//! `fleetctl diag <节点>`：节点卡、丢包要找供应商时，一条命令拿到能直接发给供应商的原始数据（ping / mtr / TCP 握手），最后一段给出判断。
//!
//! 以前要手写脚本：从各线路机 ping / mtr / 反复建连到节点，再借中控的密钥登节点装 mtr、反向 ping / mtr、测往外建连，才分清是
//! 「入站新建连接被丢、出站与已建连接正常」（2026-10-27 DeerVPS）；供应商要「ping 和 mtr 原始数据」，又手工拼文本。现在：
//! - 先取计划（`GET /machines/{id}/diag-plan`：节点地址、SS 出站端口、每台在用线路机是不是它挂的、一个在用入口端口），
//!   再经中控的远程执行（`POST /machines/{id}/exec`，要 exec 权限，每台一次、都记审计）在每台在用线路机和节点上各跑一段脚本；
//!   脚本里各项并行跑，输出按段原样带回（每段写源 → 目的、UTC 时间、命令行）。
//! - 线路机 → 节点：ping、mtr（ICMP）；节点所挂的线路机另测 TCP 握手连 SS 端口——节点的 nft 只放行自己的线路机，别的线路机连不上是正常的，不测。
//! - 节点 → 每台在用线路机（连它的一个入口端口）与对照 1.1.1.1（443）：ping、mtr、TCP 握手。同机 IP 节点在宿主机上跑、绑它自己的地址；
//!   家宽代理没有 SSH，只测线路机 → 它。
//! - 握手：有 python3 时读内核 TCP_INFO 的重传数（同 agent 的探测，`retrans>0` = 首次 SYN 被丢、重发后才连上）；没有时用 bash 计时，
//!   比最快的一次多 900 ms 以上算首次握手被丢（SYN 第一次重发在 1 秒）。每次超时 5 秒（SYN 第 0、1、3 秒各发一次都没回）。
//! - 并发 4（同 `fleetctl exec --team`）：中控同时最多 8 条远程命令、排队 30 秒还轮不到回 429，留一半给别人。
//!
//! 不要 --yes：只读（ping、mtr、建连后马上关），流量每段几 KB，同 `fleetctl exec` / `logs`。`--install-mtr` 要装包、会改机器，要 --yes。
//! mtr 没装时这一段写明「没有 mtr」、只给 ping：上架基线现在会装 mtr-tiny（新机器有），老机器用 `--install-mtr --yes` 装一次；
//! 不默认现装：诊断时往生产机上装包不该是隐含的副作用，apt 卡住还会拖慢诊断。

use crate::cmds::{outln, Ctx};
use crate::ops::confirm;
use crate::resolve::Want;
use anyhow::{bail, Context};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

/// 每次握手的超时（秒）
const TCP_TIMEOUT_S: u32 = 5;
/// 两次握手之间隔多久（秒）
const TCP_GAP_S: f64 = 0.2;
/// 握手出问题（超时 + 首次握手被丢）占这么多（%）算有问题：正常机器是 0，2026-10-27 的入站过滤是 20–40%
const TCP_BAD_PCT: f64 = 5.0;
/// ICMP 丢包到这么多（%）算有问题：100 个包丢 3 个
const ICMP_BAD_PCT: f64 = 3.0;
/// 同时跑几台
const PARALLEL: usize = 4;

#[derive(clap::Args)]
pub struct DiagOpts {
    /// 节点（显示名 / id / IP）
    pub node: String,
    /// ping / mtr 每段发多少个包（默认 100，--quick 时 20）
    #[arg(long)]
    pub count: Option<u32>,
    /// ping / mtr 的发包间隔（秒，0.2–2）
    #[arg(long, default_value_t = 0.2)]
    pub interval: f64,
    /// TCP 握手每段建连多少次（默认 50，--quick 时 20）
    #[arg(long)]
    pub tcp: Option<u32>,
    /// 少测几次（20 个包、20 次握手），半分钟左右出结果
    #[arg(long)]
    pub quick: bool,
    /// 另把全文写进这个文件（原样文本，可直接发给供应商；--json 时写 JSON）
    #[arg(long, value_name = "文件")]
    pub out: Option<std::path::PathBuf>,
    /// 机器上没有 mtr 时先装 mtr-tiny（装包会改机器，要 --yes）
    #[arg(long)]
    pub install_mtr: bool,
    #[arg(long)]
    pub yes: bool,
}

// ── 计划 ─────────────────────────────────────────────────────────────

#[derive(Deserialize, Clone, Debug)]
pub struct Plan {
    pub node: PlanNode,
    pub ss: Option<HostPort>,
    pub relays: Vec<PlanRelay>,
    pub control: HostPort,
}

#[derive(Deserialize, Clone, Debug)]
pub struct PlanNode {
    pub id: String,
    pub name: String,
    pub ip: String,
    pub bind_ip: Option<String>,
    pub host: Option<String>,
    pub provider: Option<String>,
    pub ssh: bool,
}

#[derive(Deserialize, Clone, Debug)]
pub struct HostPort {
    pub host: String,
    pub port: u16,
}

#[derive(Deserialize, Clone, Debug)]
pub struct PlanRelay {
    pub id: String,
    pub name: String,
    pub ip: String,
    pub own: bool,
    pub port: Option<u16>,
}

/// 测的方向：入（线路机 → 节点）、出（节点 → 线路机）、对照（节点 → 1.1.1.1）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    In,
    Out,
    Control,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Ping,
    Mtr,
    Tcp,
}

/// 一段测量
#[derive(Clone, Debug)]
pub struct Job {
    pub key: String,
    pub kind: Kind,
    pub dir: Dir,
    /// 「线路机1 198.51.100.1」这样的显示
    pub from: String,
    pub to: String,
    /// 对哪台线路机（入、出）；对照为 None
    pub relay: Option<String>,
    pub ip: String,
    pub port: Option<u16>,
    /// 源地址（同机 IP 节点在宿主机上跑，绑它自己的地址）
    pub src: Option<String>,
}

/// 在一台机器上跑的那一段
#[derive(Clone, Debug)]
pub struct Side {
    /// 交给 exec 的机器 id（同机 IP 节点给它自己的 id，中控登录宿主机）
    pub exec_id: String,
    pub name: String,
    pub jobs: Vec<Job>,
}

#[derive(Clone, Copy, Debug)]
pub struct Params {
    pub count: u32,
    pub interval: f64,
    pub tcp: u32,
    pub install_mtr: bool,
}

/// 计划 → 每台机器要跑的几段。线路机按计划的顺序（节点所挂的那几台在前），每台：ping、mtr，挂着它的另加握手；
/// 节点：到每台在用线路机的 ping、mtr、握手（线路机没有在用入口就不测握手），再到对照目标
pub fn sides(p: &Plan) -> Vec<Side> {
    let node = format!("{} {}", p.node.name, p.node.ip);
    let mut relays: Vec<&PlanRelay> = p.relays.iter().collect();
    relays.sort_by_key(|r| !r.own);
    let mut out = vec![];
    let mut n = 0;
    let mut key = || {
        n += 1;
        format!("j{n}")
    };
    for r in &relays {
        let from = format!("{} {}", r.name, r.ip);
        let mut jobs = vec![];
        for kind in [Kind::Ping, Kind::Mtr] {
            jobs.push(Job { key: key(), kind, dir: Dir::In, from: from.clone(), to: node.clone(), relay: Some(r.name.clone()), ip: p.node.ip.clone(), port: None, src: None });
        }
        if let (true, Some(ss)) = (r.own, &p.ss) {
            jobs.push(Job {
                key: key(),
                kind: Kind::Tcp,
                dir: Dir::In,
                from: from.clone(),
                to: format!("{} {}:{}", p.node.name, ss.host, ss.port),
                relay: Some(r.name.clone()),
                ip: ss.host.clone(),
                port: Some(ss.port),
                src: None,
            });
        }
        out.push(Side { exec_id: r.id.clone(), name: r.name.clone(), jobs });
    }
    if p.node.ssh {
        // 同机 IP 节点：命令在宿主机上跑，从它自己的地址出去（1:1 NAT 的是映射到的本地地址）
        let src = p.node.host.as_ref().map(|_| p.node.bind_ip.clone().unwrap_or_else(|| p.node.ip.clone()));
        let mut jobs = vec![];
        for r in &relays {
            let to = format!("{} {}", r.name, r.ip);
            for kind in [Kind::Ping, Kind::Mtr] {
                jobs.push(Job { key: key(), kind, dir: Dir::Out, from: node.clone(), to: to.clone(), relay: Some(r.name.clone()), ip: r.ip.clone(), port: None, src: src.clone() });
            }
            if let Some(port) = r.port {
                jobs.push(Job { key: key(), kind: Kind::Tcp, dir: Dir::Out, from: node.clone(), to: format!("{to}:{port}"), relay: Some(r.name.clone()), ip: r.ip.clone(), port: Some(port), src: src.clone() });
            }
        }
        let c = &p.control;
        for kind in [Kind::Ping, Kind::Mtr, Kind::Tcp] {
            let to = if kind == Kind::Tcp { format!("对照 {}:{}", c.host, c.port) } else { format!("对照 {}", c.host) };
            let port = (kind == Kind::Tcp).then_some(c.port);
            jobs.push(Job { key: key(), kind, dir: Dir::Control, from: node.clone(), to, relay: None, ip: c.host.clone(), port, src: src.clone() });
        }
        out.push(Side { exec_id: p.node.id.clone(), name: p.node.name.clone(), jobs });
    }
    out
}

// ── 远端脚本 ───────────────────────────────────────────────────────────

/// 握手：读内核 TCP_INFO 的 tcpi_total_retrans（偏移 100，struct tcp_info 前 8 个字节、之后第 24 个 u32；同 agent 的探测）。
/// 每行「序号 ok 毫秒 重传数」「序号 timeout」「序号 error 原因」
const TCP_PY: &str = r#"import socket,struct,sys,time
h,p,n,src,to,gap=sys.argv[1],int(sys.argv[2]),int(sys.argv[3]),sys.argv[4],float(sys.argv[5]),float(sys.argv[6])
fam=socket.AF_INET6 if ':' in h else socket.AF_INET
for i in range(1,n+1):
    s=socket.socket(fam,socket.SOCK_STREAM); s.settimeout(to)
    try:
        if src!='-': s.bind((src,0))
        t=time.monotonic(); s.connect((h,p)); ms=(time.monotonic()-t)*1000
        try: r=struct.unpack_from('I',s.getsockopt(socket.IPPROTO_TCP,socket.TCP_INFO,104),100)[0]
        except Exception: r='-'
        print(i,'ok','%.1f'%ms,r,flush=True)
    except socket.timeout: print(i,'timeout',flush=True)
    except OSError as e: print(i,'error',(e.strerror or str(e)).replace(' ','_'),flush=True)
    finally: s.close()
    time.sleep(gap)
"#;

/// 远端脚本的固定部分：临时目录、握手函数（python3 或 bash 计时）、并行跑一段并记下开始时刻（UTC）与退出码的 run
const PRELUDE: &str = r####"set -u
D=$(mktemp -d) || exit 1
trap 'rm -rf "$D"' EXIT
PY=$(cat <<'PYEOF'
__PY__
PYEOF
)
tcpt() {
  if command -v python3 >/dev/null 2>&1; then python3 -c "$PY" "$1" "$2" "$3" "$4" __TO__ __GAP__; return; fi
  echo "# 没有 python3：用 bash /dev/tcp 计时（比最快的一次多 900 ms 以上算首次握手被丢）"
  [ "$4" = - ] || echo "# 绑不了源地址 $4，用的是这台机器的默认出口地址"
  i=0
  while [ $i -lt $3 ]; do
    i=$((i+1)); t0=$(date +%s%N)
    if timeout __TO__ bash -c "exec 3<>/dev/tcp/$1/$2" 2>/dev/null; then us=$(( ($(date +%s%N)-t0)/1000 )); printf '%d ok %d.%d -\n' $i $((us/1000)) $((us%1000/100))
    else rc=$?; if [ $rc = 124 ]; then echo "$i timeout"; else echo "$i error rc=$rc"; fi; fi
    sleep __GAP__
  done
}
run() { k=$1; shift; date -u +%Y-%m-%dT%H:%M:%SZ > "$D/$k.t"; "$@" > "$D/$k.o" 2>&1; echo "exit=$?" >> "$D/$k.o"; }
"####;

/// 现装 mtr-tiny（只在 --install-mtr 时）：先直接装，不行更新包索引再装；结果写进 meta
const INSTALL_MTR: &str = r####"if ! command -v mtr >/dev/null 2>&1; then
  export DEBIAN_FRONTEND=noninteractive
  { timeout 120 apt-get install -y -qq mtr-tiny || { timeout 120 apt-get update -qq && timeout 120 apt-get install -y -qq mtr-tiny; }; } >/dev/null 2>&1 </dev/null
  if command -v mtr >/dev/null 2>&1; then echo "### meta mtr_installed=1"; else echo "### meta mtr_installed=0"; fi
fi
"####;

fn secs(x: f64) -> String {
    let s = format!("{x:.1}");
    s.trim_end_matches(".0").to_string()
}

/// 单引号转义（地址来自中控，照样转义）
fn q(s: &str) -> String { format!("'{}'", s.replace('\'', "'\\''")) }

/// 提示里的命令参数：字母数字（含中文）与 -_.:/@+=, 原样，否则加单引号（同中控告警的建议命令）
fn arg(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || "-_.:/@+=,".contains(c)) { s.to_string() } else { q(s) }
}

/// 一段的命令行（报告里原样写出；ping / mtr 就是远端执行的那条，脚本里的地址另加引号）
pub fn cmdline(j: &Job, p: &Params) -> String { command(j, p, &|s: &str| s.to_string()) }

/// ping 不加 -w：有 -w 时 -c 变成「等够这么多个回包」，丢包时会一直发到时限，发出的个数就不是 -c 了
fn command(j: &Job, p: &Params, q: &dyn Fn(&str) -> String) -> String {
    let (c, i) = (p.count, secs(p.interval));
    match j.kind {
        Kind::Ping => {
            let src = j.src.as_ref().map(|s| format!(" -I {}", q(s))).unwrap_or_default();
            format!("ping -n{src} -c {c} -i {i} {}", q(&j.ip))
        }
        Kind::Mtr => {
            let src = j.src.as_ref().map(|s| format!(" -a {}", q(s))).unwrap_or_default();
            format!("mtr -rwnz{src} -c {c} -i {i} {}", q(&j.ip))
        }
        Kind::Tcp => format!(
            "TCP 握手 {} 次 → {}:{}（每次超时 {TCP_TIMEOUT_S} 秒、间隔 {} 秒{}）",
            p.tcp,
            j.ip,
            j.port.unwrap_or(0),
            secs(TCP_GAP_S),
            j.src.as_ref().map(|s| format!("，源地址 {s}")).unwrap_or_default()
        ),
    }
}

/// 一台机器上要跑的脚本：各段并行跑，跑完按顺序输出「### 段 开始时刻」+ 原样输出（最后一行 exit=退出码）；开头一行 meta 报有没有 mtr / python3
pub fn script(jobs: &[Job], p: &Params) -> String {
    let mut s = PRELUDE.replace("__PY__", TCP_PY.trim_end()).replace("__TO__", &TCP_TIMEOUT_S.to_string()).replace("__GAP__", &secs(TCP_GAP_S));
    if p.install_mtr && jobs.iter().any(|j| j.kind == Kind::Mtr) {
        s += INSTALL_MTR;
    }
    s += "M=0; command -v mtr >/dev/null 2>&1 && M=1\n";
    s += "echo \"### meta mtr=$M python3=$(command -v python3 >/dev/null 2>&1 && echo 1 || echo 0)\"\n";
    for j in jobs {
        let line = match j.kind {
            Kind::Ping | Kind::Mtr => {
                let c = command(j, p, &q);
                if j.kind == Kind::Mtr { format!("if [ $M = 1 ]; then run {} {c} & fi\n", j.key) } else { format!("run {} {c} &\n", j.key) }
            }
            Kind::Tcp => format!("run {} tcpt {} {} {} {} &\n", j.key, q(&j.ip), j.port.unwrap_or(0), p.tcp, q(j.src.as_deref().unwrap_or("-"))),
        };
        s += &line;
    }
    s += "wait\n";
    let keys: Vec<&str> = jobs.iter().map(|j| j.key.as_str()).collect();
    let _ = writeln!(s, "for k in {}; do [ -f \"$D/$k.o\" ] || continue; echo \"### $k $(cat \"$D/$k.t\")\"; cat \"$D/$k.o\"; done", keys.join(" "));
    s
}

/// 这台机器的远程执行给多少秒：最慢的一段（mtr 遇到不回包的跳会慢一倍；握手全部超时）再加余量，装 mtr 另加 6 分钟；夹在 60–600
pub fn exec_timeout(jobs: &[Job], p: &Params) -> u64 {
    let icmp = p.count as f64 * p.interval * 2.0 + 30.0;
    let tcp = if jobs.iter().any(|j| j.kind == Kind::Tcp) { p.tcp as f64 * (TCP_TIMEOUT_S as f64 + TCP_GAP_S) } else { 0.0 };
    let install = if p.install_mtr { 360.0 } else { 0.0 };
    (icmp.max(tcp) + 30.0 + install).clamp(60.0, 600.0) as u64
}

// ── 解析 ─────────────────────────────────────────────────────────────

/// 一台机器的输出：meta（mtr=0/1、python3=0/1、mtr_installed）与各段（开始时刻、原样输出、退出码）
#[derive(Debug, Default)]
pub struct Parsed {
    pub meta: HashMap<String, String>,
    pub secs: HashMap<String, (String, String, Option<i32>)>,
}

pub fn parse(stdout: &str) -> Parsed {
    let mut p = Parsed::default();
    let mut cur: Option<(String, String, Vec<&str>)> = None;
    let flush = |cur: &mut Option<(String, String, Vec<&str>)>, p: &mut Parsed| {
        if let Some((k, t, mut lines)) = cur.take() {
            let exit = lines.last().and_then(|l| l.strip_prefix("exit=")).and_then(|x| x.parse().ok());
            if exit.is_some() {
                lines.pop();
            }
            p.secs.insert(k, (t, lines.join("\n"), exit));
        }
    };
    for l in stdout.lines() {
        if let Some(rest) = l.strip_prefix("### meta ") {
            for kv in rest.split_whitespace() {
                if let Some((k, v)) = kv.split_once('=') {
                    p.meta.insert(k.into(), v.into());
                }
            }
        } else if let Some(rest) = l.strip_prefix("### ") {
            flush(&mut cur, &mut p);
            let (k, t) = rest.split_once(' ').unwrap_or((rest, ""));
            cur = Some((k.into(), t.into(), vec![]));
        } else if let Some(c) = cur.as_mut() {
            c.2.push(l);
        }
    }
    flush(&mut cur, &mut p);
    p
}

/// ping 的汇总行：(发出, 收到, 丢包 %, 平均往返 ms)
pub fn ping_summary(body: &str) -> Option<(u32, u32, f64, Option<f64>)> {
    let l = body.lines().find(|l| l.contains("packets transmitted"))?;
    let nums: Vec<u32> = l.split(',').filter_map(|p| p.split_whitespace().next()?.parse().ok()).collect();
    let (sent, recv) = (*nums.first()?, *nums.get(1)?);
    let loss = if sent == 0 { 100.0 } else { (sent - recv.min(sent)) as f64 * 100.0 / sent as f64 };
    // rtt min/avg/max/mdev = 150.1/152.3/170.2/3.1 ms
    let avg = body.lines().find(|l| l.contains("min/avg/max")).and_then(|l| l.split('=').nth(1)).and_then(|v| v.trim().split('/').nth(1)?.parse().ok());
    Some((sent, recv, loss, avg))
}

/// mtr 报告最后一跳的丢包 %（「 9.|-- 203.0.113.5  12.0%  100 …」或带 AS 号的写法：第一个以 % 结尾的数）
#[cfg(test)]
pub fn mtr_last_loss(body: &str) -> Option<f64> {
    let last = body.lines().rfind(|l| l.trim_start().split('.').next().is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())))?;
    last.split_whitespace().find_map(|w| w.strip_suffix('%')?.parse().ok())
}

/// 握手的汇总
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct TcpSum {
    pub n: u32,
    pub ok: u32,
    pub timeout: u32,
    pub error: u32,
    /// 首次握手被丢：内核重传数 > 0（有 python3），或耗时比最快的一次多 900 ms 以上（bash 计时）
    pub syn_lost: u32,
    /// 用的是内核 TCP_INFO（否则是计时）
    pub kernel: bool,
    pub min_ms: Option<f64>,
}

impl TcpSum {
    fn add(&mut self, o: &TcpSum) {
        self.n += o.n;
        self.ok += o.ok;
        self.timeout += o.timeout;
        self.error += o.error;
        self.syn_lost += o.syn_lost;
    }
    /// 握手出问题的比例（超时 + 首次被丢）
    pub fn bad_pct(&self) -> f64 { if self.n == 0 { 0.0 } else { (self.timeout + self.syn_lost) as f64 * 100.0 / self.n as f64 } }
    pub fn line(&self) -> String {
        let how = if self.kernel { "按内核 TCP_INFO 的重传数" } else { "按耗时，比最快的一次多 900 ms 以上" };
        let mut s = format!("结果：{} 次，连上 {}，超时 {}，首次握手被丢 {}（{how}）", self.n, self.ok, self.timeout, self.syn_lost);
        if self.error > 0 {
            let _ = write!(s, "，出错 {}", self.error);
        }
        if let Some(m) = self.min_ms {
            let _ = write!(s, "；最快 {m:.1} ms");
        }
        s
    }
}

pub fn tcp_summary(body: &str) -> TcpSum {
    let mut s = TcpSum { kernel: true, ..Default::default() };
    let mut oks: Vec<(f64, Option<u32>)> = vec![];
    for l in body.lines().filter(|l| !l.starts_with('#')) {
        let w: Vec<&str> = l.split_whitespace().collect();
        match w.get(1).copied() {
            Some("ok") => oks.push((w.get(2).and_then(|x| x.parse().ok()).unwrap_or(0.0), w.get(3).and_then(|x| x.parse().ok()))),
            Some("timeout") => s.timeout += 1,
            Some("error") => s.error += 1,
            _ => continue,
        }
        s.n += 1;
    }
    s.ok = oks.len() as u32;
    s.kernel = !oks.is_empty() && oks.iter().all(|(_, r)| r.is_some());
    s.min_ms = oks.iter().map(|o| o.0).reduce(f64::min);
    s.syn_lost = match (s.kernel, s.min_ms) {
        (true, _) => oks.iter().filter(|(_, r)| r.unwrap_or(0) > 0).count() as u32,
        (false, Some(min)) => oks.iter().filter(|(ms, _)| *ms > min + 900.0).count() as u32,
        _ => 0,
    };
    s
}

// ── 结果与判断 ─────────────────────────────────────────────────────────

/// 一段的结果
#[derive(Debug, Clone)]
pub struct Got {
    pub job: Job,
    pub cmd: String,
    pub utc: Option<String>,
    pub output: String,
    pub exit: Option<i32>,
    /// 没测成的原因（SSH 失败、没有 mtr…）
    pub note: Option<String>,
}

fn pct(x: f64) -> String { format!("{x:.0}%") }

/// 各段 → 判断（几行）：先列数字，最后一行「→ 结论」
pub fn judge(plan: &Plan, got: &[Got]) -> Vec<String> {
    let tcp = |d: Dir| {
        let mut s = TcpSum::default();
        for g in got.iter().filter(|g| g.job.kind == Kind::Tcp && g.job.dir == d && g.note.is_none()) {
            s.add(&tcp_summary(&g.output));
        }
        s
    };
    // ICMP：每段的丢包（ping 为准，没测成的不算）
    let icmp = |d: Dir| -> Vec<(String, f64)> {
        got.iter()
            .filter(|g| g.job.kind == Kind::Ping && g.job.dir == d && g.note.is_none())
            .filter_map(|g| ping_summary(&g.output).map(|s| (g.job.relay.clone().unwrap_or_else(|| g.job.ip.clone()), s.2)))
            .collect()
    };
    let (tin, tout, tctl) = (tcp(Dir::In), tcp(Dir::Out), tcp(Dir::Control));
    let (iin, iout, ictl) = (icmp(Dir::In), icmp(Dir::Out), icmp(Dir::Control));
    let worst = |v: &[(String, f64)]| v.iter().map(|x| x.1).fold(None, |a: Option<f64>, b| Some(a.map_or(b, |a| a.max(b))));
    let range = |v: &[(String, f64)]| match (v.iter().map(|x| x.1).reduce(f64::min), worst(v)) {
        (Some(a), Some(b)) if (a - b).abs() < 0.5 => pct(a),
        (Some(a), Some(b)) => format!("{}–{}", pct(a), pct(b)),
        _ => "没测".into(),
    };
    let tline = |s: &TcpSum| if s.n == 0 { "没测".to_string() } else { format!("{}（{} 次里超时 {}、首次握手被丢 {}）", pct(s.bad_pct()), s.n, s.timeout, s.syn_lost) };
    let out_side = plan.node.ssh && (tout.n > 0 || !iout.is_empty());
    let mut lines = vec![format!("入站握手（所挂线路机 → 节点 SS 端口）：{}", tline(&tin))];
    if out_side {
        lines.push(format!("出站握手（节点 → 各线路机入口端口）：{}；对照 {}:{}：{}", tline(&tout), plan.control.host, plan.control.port, tline(&tctl)));
    }
    lines.push(if out_side {
        format!("ICMP 丢包：线路机 → 节点 {}；节点 → 线路机 {}；节点 → {} {}", range(&iin), range(&iout), plan.control.host, range(&ictl))
    } else {
        format!("ICMP 丢包：线路机 → 节点 {}", range(&iin))
    });

    let bad_t = |s: &TcpSum| s.n > 0 && s.bad_pct() >= TCP_BAD_PCT;
    let bad_i = |v: &[(String, f64)]| worst(v).is_some_and(|w| w >= ICMP_BAD_PCT);
    let (in_t, out_t, ctl_t, in_i, out_i, ctl_i) = (bad_t(&tin), bad_t(&tout), bad_t(&tctl), bad_i(&iin), bad_i(&iout), bad_i(&ictl));
    let bad_from: Vec<&str> = iin.iter().filter(|x| x.1 >= ICMP_BAD_PCT).map(|x| x.0.as_str()).collect();
    let advice = "把上面的原始数据发给供应商；客户受影响时开穿透绕开入站（fleetctl entry reverse <入口> --on），或换到备用节点（fleetctl replace）";
    let verdict = if tin.n == 0 && iin.is_empty() && !out_side {
        "→ 没测成（线路机上的命令都没跑起来，看上面各段的原因）".to_string()
    } else if !out_side {
        let why = if plan.node.ssh { "节点上的命令没跑成（看上面的原因）" } else { "家宽代理没有 SSH" };
        match (in_t, in_i) {
            (true, _) | (_, true) => format!("→ 线路机 → 节点这一段有丢包（{why}，反方向没测：到节点那一侧自己 ping / mtr 线路机对照）"),
            _ => format!("→ 线路机 → 节点这一段没测到丢包（{why}，反方向没测）"),
        }
    } else if tin.n == 0 {
        // 所挂线路机上没跑成（或节点没有入口）：不能说入站正常
        let out = if out_t || out_i { "出方向有丢包（看 mtr 从哪一跳开始）" } else { "出方向正常" };
        format!("→ 入站握手没测成（看上面所挂线路机那几段的原因），{out}；修好后再跑一次")
    } else if in_t && !out_t && !in_i && !out_i {
        format!("→ 入站新建连接被丢 {}、出站 {}、ICMP 正常：入站过滤（供应商侧只丢进来的新建连接，已建连接和出站不受影响）。{advice}", pct(tin.bad_pct()), pct(tout.bad_pct()))
    } else if (in_t || in_i) && !out_t && !out_i {
        format!("→ 只有入方向丢包（握手 {}、ICMP {}），出方向正常：供应商入方向的链路或过滤问题，mtr 里线路机 → 节点从哪一跳开始丢就是哪一段。{advice}", pct(tin.bad_pct()), range(&iin))
    } else if (in_t || in_i) && (out_t || out_i) {
        "→ 双向都丢包：节点所在机房 / 网络（或节点本机）的问题，对照两个方向的 mtr 从哪一跳开始丢，发给供应商；必要时换备用（fleetctl replace）".to_string()
    } else if out_t || out_i {
        "→ 只有出方向丢包（入站正常）：节点往外这一段有问题（供应商出站限制，或节点本机），mtr 里节点 → 线路机从哪一跳开始丢就是哪一段".to_string()
    } else if ctl_t || ctl_i {
        format!("→ 节点到线路机正常，到对照 {} 丢包：节点往公网这一段（或对照目标本身）有问题，与线路机无关", plan.control.host)
    } else {
        "→ 这次没测到明显丢包：问题可能是间歇的，在告警或卡片红块出现时再跑一次，或加大次数（--count 300 --tcp 200）".to_string()
    };
    lines.push(verdict);
    if !bad_from.is_empty() && bad_from.len() < iin.len() {
        lines.push(format!("（ICMP 只在 {} → 节点 这几路丢：可能是这几台线路机到这一段的路由问题，换线路机看 fleetctl placement）", bad_from.join("、")));
    }
    lines
}

/// 握手的逐次结果每行 10 次（毫秒；「r1」= SYN 重传过 1 次、首次握手被丢；「超时」；出错写原因），说明行（# 开头）原样
pub fn compact_tcp(body: &str) -> String {
    let mut out = String::new();
    let mut row: Vec<String> = vec![];
    let mut first = 0;
    for l in body.lines() {
        if l.starts_with('#') {
            let _ = writeln!(out, "{l}");
            continue;
        }
        let w: Vec<&str> = l.split_whitespace().collect();
        let tok = match w.get(1).copied() {
            Some("ok") => match w.get(3).and_then(|r| r.parse::<u32>().ok()) {
                Some(r) if r > 0 => format!("{}r{r}", w.get(2).unwrap_or(&"?")),
                _ => w.get(2).unwrap_or(&"?").to_string(),
            },
            Some("timeout") => "超时".into(),
            Some("error") => format!("出错:{}", w.get(2).unwrap_or(&"?")),
            _ => continue,
        };
        if row.is_empty() {
            first = w[0].parse().unwrap_or(0);
        }
        row.push(tok);
        if row.len() == 10 {
            let _ = writeln!(out, "{:>3}–{:<3} {}", first, first + 9, row.join(" "));
            row.clear();
        }
    }
    if !row.is_empty() {
        let _ = writeln!(out, "{:>3}–{:<3} {}", first, first + row.len() - 1, row.join(" "));
    }
    out
}

/// 报告全文：抬头、各段（源 → 目的、UTC 时间、命令行、原样输出）、判断
pub fn render(plan: &Plan, p: &Params, started: &str, got: &[Got], verdict: &[String]) -> String {
    let mut s = String::new();
    let own: Vec<&str> = plan.relays.iter().filter(|r| r.own).map(|r| r.name.as_str()).collect();
    let mut about = vec![plan.node.ip.clone()];
    about.extend(plan.node.provider.clone());
    if let Some(h) = &plan.node.host {
        about.push(format!("同机 IP 节点，宿主机 {h}"));
    }
    if !own.is_empty() {
        about.push(format!("挂在 {}", own.join("、")));
    }
    let _ = writeln!(s, "fleetctl diag {}（{}）  开始于 {started} UTC", plan.node.name, about.join("，"));
    let _ = writeln!(s, "ping / mtr 每段 {} 个包、间隔 {} 秒；TCP 握手每段 {} 次、每次超时 {TCP_TIMEOUT_S} 秒", p.count, secs(p.interval), p.tcp);
    if !plan.node.ssh {
        let _ = writeln!(s, "家宽代理没有 SSH：只测了线路机 → 它，反方向要在家宽那一侧自己测");
    }
    for g in got {
        let what = match (g.job.kind, g.job.dir) {
            (Kind::Ping, _) => "ping",
            (Kind::Mtr, _) => "mtr",
            (Kind::Tcp, Dir::In) => "TCP 握手（入站）",
            (Kind::Tcp, _) => "TCP 握手（出站）",
        };
        let _ = writeln!(s, "\n=== {} → {} · {what} ===", g.job.from, g.job.to);
        let _ = writeln!(s, "# {} UTC  {}", g.utc.as_deref().unwrap_or("-"), g.cmd);
        if let Some(n) = &g.note {
            let _ = writeln!(s, "（{n}）");
            continue;
        }
        if g.job.kind == Kind::Tcp {
            let _ = write!(s, "{}", compact_tcp(&g.output));
            let _ = writeln!(s, "{}", tcp_summary(&g.output).line());
        } else if !g.output.is_empty() {
            let _ = writeln!(s, "{}", g.output.trim_end());
        }
        if let Some(c) = g.exit.filter(|c| *c != 0) {
            let _ = writeln!(s, "（退出码 {c}）");
        }
    }
    let _ = writeln!(s, "\n=== 判断 ===");
    for l in verdict {
        let _ = writeln!(s, "{l}");
    }
    s
}

/// 一台机器的结果 → 各段（没跑成的写原因）
pub fn collect(side: &Side, p: &Params, res: &anyhow::Result<Value>, install_hint: &str) -> Vec<Got> {
    let mut out = vec![];
    let parsed = match res {
        Ok(d) => {
            let mut x = parse(d["stdout"].as_str().unwrap_or(""));
            if d["timed_out"] == true {
                x.meta.insert("timed_out".into(), "1".into());
            }
            // 一段都没有、退出码不是 0：脚本没跑起来（SSH 连不上是 255，原因在 stderr）
            match d["exit_code"].as_i64() {
                Some(c) if c != 0 && x.secs.is_empty() && !x.meta.contains_key("timed_out") => {
                    let e: String = d["stderr"].as_str().unwrap_or("").trim().chars().take(300).collect();
                    Err(format!("在 {} 上执行失败（退出码 {c}）：{e}", side.name))
                }
                _ => Ok(x),
            }
        }
        Err(e) => Err(format!("在 {} 上执行失败：{e:#}", side.name)),
    };
    for j in &side.jobs {
        let cmd = cmdline(j, p);
        let mut g = Got { job: j.clone(), cmd, utc: None, output: String::new(), exit: None, note: None };
        match &parsed {
            Err(e) => g.note = Some(e.clone()),
            Ok(x) => match x.secs.get(&j.key) {
                Some((t, body, exit)) => {
                    g.utc = Some(t.clone());
                    g.output = body.clone();
                    g.exit = *exit;
                }
                None if j.kind == Kind::Mtr && x.meta.get("mtr").map(String::as_str) == Some("0") => {
                    let tried = x.meta.get("mtr_installed").is_some_and(|v| v == "0");
                    g.note = Some(if tried { format!("{} 上装 mtr-tiny 没装上，只有上面的 ping", side.name) } else { format!("{} 上没有 mtr，只有上面的 ping；{install_hint}", side.name) });
                }
                None if x.meta.contains_key("timed_out") => g.note = Some(format!("{} 上的命令超时被终止，这一段没有输出", side.name)),
                None => g.note = Some("没有输出".into()),
            },
        }
        out.push(g);
    }
    out
}

pub fn run(ctx: &Ctx, o: DiagOpts) -> anyhow::Result<u8> {
    let count = o.count.unwrap_or(if o.quick { 20 } else { 100 });
    let tcp = o.tcp.unwrap_or(if o.quick { 20 } else { 50 });
    if !(5..=1000).contains(&count) {
        bail!("--count 要在 5 到 1000 之间");
    }
    if !(0.2..=2.0).contains(&o.interval) {
        bail!("--interval 要在 0.2 到 2 秒之间");
    }
    if !(5..=500).contains(&tcp) {
        bail!("--tcp 要在 5 到 500 之间");
    }
    let p = Params { count, interval: o.interval, tcp, install_mtr: o.install_mtr };
    let (id, name) = ctx.find_id(&o.node, Want::Live)?;
    let raw = ctx.c.get(&format!("/machines/{id}/diag-plan"))?;
    let plan: Plan = serde_json::from_value(raw.clone()).context("诊断计划的格式不对（中控版本不匹配？）")?;
    let sides = sides(&plan);
    if o.install_mtr {
        let names: Vec<&str> = sides.iter().map(|s| s.name.as_str()).collect();
        confirm(&format!("没有 mtr 的机器上会先 apt-get install mtr-tiny（{}）", names.join("、")), o.yes)?;
    }
    let hint = format!("要 mtr 就加 --install-mtr --yes 现装：fleetctl diag {} --install-mtr --yes", arg(&name));
    let started = time::OffsetDateTime::now_utc();
    if !ctx.json {
        let longest = sides.iter().map(|s| exec_timeout(&s.jobs, &p)).max().unwrap_or(60);
        let names: Vec<&str> = sides.iter().map(|s| s.name.as_str()).collect();
        eprintln!(
            "在 {} 上执行（并发 {PARALLEL}，每台一次远程执行、都记审计）：每台约 {} 秒，最长 {longest} 秒……",
            names.join("、"),
            (p.count as f64 * p.interval).max(p.tcp as f64 * (TCP_GAP_S + 0.2)).ceil() as u64 + 10
        );
    }
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Option<anyhow::Result<Value>>>> = Mutex::new((0..sides.len()).map(|_| None).collect());
    std::thread::scope(|s| {
        for _ in 0..PARALLEL.min(sides.len()) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                let Some(side) = sides.get(i) else { break };
                let t = exec_timeout(&side.jobs, &p);
                // 中控最坏 timeout + 45 秒多才回（排队 30 秒、本地兜底 15 秒），同 fleetctl exec 多留 15 秒
                let r = ctx.c.post_t(&format!("/machines/{}/exec", side.exec_id), &json!({"command": script(&side.jobs, &p), "timeout_s": t}), Duration::from_secs(t + 60));
                results.lock().unwrap()[i] = Some(r);
            });
        }
    });
    let results: Vec<anyhow::Result<Value>> = results.into_inner().unwrap().into_iter().map(|r| r.unwrap_or_else(|| Err(anyhow::anyhow!("没有执行")))).collect();
    let got: Vec<Got> = sides.iter().zip(&results).flat_map(|(side, r)| collect(side, &p, r, &hint)).collect();
    let verdict = judge(&plan, &got);
    let fmt = time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");
    let started = started.format(&fmt).unwrap_or_default();
    let text = if ctx.json {
        json!({
            "node": plan.node.name, "started_at": started, "plan": raw,
            "sections": got.iter().map(|g| json!({
                "from": g.job.from, "to": g.job.to, "kind": format!("{:?}", g.job.kind).to_lowercase(), "dir": format!("{:?}", g.job.dir).to_lowercase(),
                "utc": g.utc, "command": g.cmd, "output": g.output, "exit_code": g.exit, "note": g.note,
            })).collect::<Vec<_>>(),
            "verdict": verdict,
        })
        .to_string()
    } else {
        render(&plan, &p, &started, &got, &verdict)
    };
    if let Some(path) = &o.out {
        std::fs::write(path, &text).with_context(|| format!("写 {}", path.display()))?;
        if !ctx.json {
            eprintln!("已写入 {}", path.display());
        }
    }
    if ctx.json {
        outln(text);
    } else {
        crate::cmds::out_raw(&text);
    }
    // 一台都没跑成算失败
    Ok(if results.iter().all(|r| r.is_err()) { 1 } else { 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(ssh: bool, host: Option<&str>) -> Plan {
        serde_json::from_value(json!({
            "node": {"id": "n1", "name": "团队1-01", "kind": "vps", "ip": "203.0.113.5", "bind_ip": null, "host": host, "provider": "DeerVPS", "ssh": ssh},
            "ss": {"host": "203.0.113.5", "port": 8388},
            "relays": [
                {"id": "r2", "name": "线路机2", "ip": "198.51.100.2", "own": false, "port": 20031},
                {"id": "r1", "name": "线路机1", "ip": "198.51.100.1", "own": true, "port": 20005},
            ],
            "control": {"host": "1.1.1.1", "port": 443},
        }))
        .unwrap()
    }

    const P: Params = Params { count: 100, interval: 0.2, tcp: 50, install_mtr: false };

    #[test]
    fn plan_to_sides() {
        let s = sides(&plan(true, None));
        assert_eq!(s.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), ["线路机1", "线路机2", "团队1-01"], "所挂的线路机在前，节点最后");
        // 所挂的线路机测握手（连 SS 端口），别的线路机不测（节点 nft 不放行它）
        assert_eq!(s[0].jobs.iter().map(|j| j.kind).collect::<Vec<_>>(), [Kind::Ping, Kind::Mtr, Kind::Tcp]);
        assert_eq!((s[0].jobs[2].ip.as_str(), s[0].jobs[2].port), ("203.0.113.5", Some(8388)));
        assert_eq!(s[1].jobs.iter().map(|j| j.kind).collect::<Vec<_>>(), [Kind::Ping, Kind::Mtr]);
        // 节点：每台线路机 ping / mtr / 握手（入口端口），再对照
        let n = &s[2].jobs;
        assert_eq!(n.len(), 9);
        assert_eq!((n[2].ip.as_str(), n[2].port, n[2].dir), ("198.51.100.1", Some(20005), Dir::Out));
        assert_eq!((n[8].ip.as_str(), n[8].port, n[8].dir), ("1.1.1.1", Some(443), Dir::Control));
        assert!(n.iter().all(|j| j.src.is_none()));
        // 家宽代理：只有线路机那几段
        assert_eq!(sides(&plan(false, None)).len(), 2);
        // 同机 IP 节点：在宿主机上跑（exec 给它自己的 id），从它自己的地址出去
        let s = sides(&plan(true, Some("团队1-03")));
        assert_eq!((s[2].exec_id.as_str(), s[2].jobs[0].src.as_deref()), ("n1", Some("203.0.113.5")));
        assert_eq!(cmdline(&s[2].jobs[0], &P), "ping -n -I 203.0.113.5 -c 100 -i 0.2 198.51.100.1");
        assert_eq!(cmdline(&s[2].jobs[1], &P), "mtr -rwnz -a 203.0.113.5 -c 100 -i 0.2 198.51.100.1");
    }

    #[test]
    fn script_shape() {
        let s = sides(&plan(true, None));
        let sc = script(&s[0].jobs, &P);
        assert!(sc.contains("run j1 ping -n -c 100 -i 0.2 '203.0.113.5' &\n"), "{sc}");
        assert!(sc.contains("if [ $M = 1 ]; then run j2 mtr -rwnz -c 100 -i 0.2 '203.0.113.5' & fi\n"), "{sc}");
        assert!(sc.contains("run j3 tcpt '203.0.113.5' 8388 50 '-' &\n"), "{sc}");
        assert!(sc.contains("for k in j1 j2 j3; do"));
        assert!(sc.contains("python3 -c \"$PY\" \"$1\" \"$2\" \"$3\" \"$4\" 5 0.2"));
        assert!(!sc.contains("__") && !sc.contains("apt-get"), "占位符都已替换；不加 --install-mtr 不装包");
        let sc = script(&s[0].jobs, &Params { install_mtr: true, ..P });
        assert!(sc.contains("apt-get install -y -qq mtr-tiny"));
        // 时限：握手全超时最慢（50 × 5.2 = 260 秒）
        assert_eq!(exec_timeout(&s[0].jobs, &P), 290);
        assert_eq!(exec_timeout(&s[1].jobs, &P), 100);
        assert_eq!(exec_timeout(&s[0].jobs, &Params { install_mtr: true, ..P }), 600);
    }

    const PING_OK: &str = "PING 203.0.113.5 (203.0.113.5) 56(84) bytes of data.\n\n--- 203.0.113.5 ping statistics ---\n100 packets transmitted, 100 received, 0% packet loss, time 19803ms\nrtt min/avg/max/mdev = 151.204/152.310/160.877/1.402 ms";
    const PING_LOSS: &str = "--- 198.51.100.1 ping statistics ---\n100 packets transmitted, 88 received, 12% packet loss, time 19790ms\nrtt min/avg/max/mdev = 151.1/152.0/155.0/1.0 ms";
    const MTR: &str = "Start: 2026-10-27T03:10:12+0000\nHOST: relay1                      Loss%   Snt   Last   Avg  Best  Wrst StDev\n  1. AS???    10.0.0.1             0.0%   100    0.3   0.3   0.2   0.6   0.0\n  2. AS64500  192.0.2.77           0.0%   100    1.1   1.2   0.9   3.0   0.3\n  3. AS64501  203.0.113.5          0.0%   100  152.0 152.3 151.2 160.8   1.4";

    fn tcp_out(n: u32, timeout: &[u32], retrans: &[u32]) -> String {
        (1..=n)
            .map(|i| if timeout.contains(&i) { format!("{i} timeout") } else { format!("{i} ok {} {}", if retrans.contains(&i) { "1152.3" } else { "152.3" }, u32::from(retrans.contains(&i))) })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn parsers() {
        assert_eq!(ping_summary(PING_OK), Some((100, 100, 0.0, Some(152.31))));
        assert_eq!(ping_summary(PING_LOSS).map(|x| x.2), Some(12.0));
        assert_eq!(mtr_last_loss(MTR), Some(0.0));
        assert_eq!(mtr_last_loss("  5.|-- 203.0.113.5   12.0%   100  150.1"), Some(12.0));
        let s = tcp_summary(&tcp_out(50, &[7, 30], &[3, 4, 9, 20, 41]));
        assert_eq!((s.n, s.ok, s.timeout, s.syn_lost, s.kernel), (50, 48, 2, 5, true));
        assert_eq!(s.bad_pct(), 14.0);
        // bash 计时（没有 python3）：比最快的多 900 ms 以上算首次被丢
        let b = "# 没有 python3：用 bash /dev/tcp 计时\n1 ok 150.2 -\n2 ok 1151.0 -\n3 timeout\n4 error rc=1\n5 ok 151.0 -";
        let s = tcp_summary(b);
        assert_eq!((s.n, s.ok, s.timeout, s.error, s.syn_lost, s.kernel), (5, 3, 1, 1, 1, false));
        let x = parse("### meta mtr=0 python3=1\n### j1 2026-10-27T03:10:12Z\nPING …\nexit=0\n### j3 2026-10-27T03:10:12Z\n1 ok 152.3 0\nexit=0\n");
        assert_eq!(x.meta["mtr"], "0");
        assert_eq!(x.secs["j1"], ("2026-10-27T03:10:12Z".into(), "PING …".into(), Some(0)));
        assert_eq!(x.secs["j3"].1, "1 ok 152.3 0");
    }

    /// 造一份各段结果：入站握手 `in_tcp`、出站握手 `out_tcp`、各方向 ping 输出
    fn fake(plan: &Plan, in_tcp: &str, out_tcp: &str, ping_in: &str, ping_out: &str) -> Vec<Got> {
        sides(plan)
            .iter()
            .flat_map(|s| s.jobs.clone())
            .map(|j| {
                let output = match (j.kind, j.dir) {
                    (Kind::Tcp, Dir::In) => in_tcp.to_string(),
                    (Kind::Tcp, _) => out_tcp.to_string(),
                    (Kind::Ping, Dir::In) => ping_in.to_string(),
                    (Kind::Ping, _) => ping_out.to_string(),
                    (Kind::Mtr, _) => MTR.to_string(),
                };
                Got { cmd: cmdline(&j, &P), job: j, utc: Some("2026-10-27T03:10:12Z".into()), output, exit: Some(0), note: None }
            })
            .collect()
    }

    #[test]
    fn verdicts() {
        let p = plan(true, None);
        let bad_in = tcp_out(50, &[7, 30, 44], &[3, 4, 9, 20, 41, 12, 13, 14, 15]);
        let good = tcp_out(50, &[], &[]);
        // 2026-10-27 的样子：入站握手丢、出站与 ICMP 正常 → 入站过滤
        let v = judge(&p, &fake(&p, &bad_in, &good, PING_OK, PING_OK));
        assert_eq!(v[0], "入站握手（所挂线路机 → 节点 SS 端口）：24%（50 次里超时 3、首次握手被丢 9）");
        assert!(v.last().unwrap().starts_with("→ 入站新建连接被丢 24%、出站 0%、ICMP 正常：入站过滤"), "{v:?}");
        assert!(v.last().unwrap().contains("fleetctl replace"));
        // 入方向 ICMP 也丢 → 入方向链路
        let v = judge(&p, &fake(&p, &bad_in, &good, PING_LOSS, PING_OK));
        assert!(v.last().unwrap().starts_with("→ 只有入方向丢包"), "{v:?}");
        // 双向都丢
        let v = judge(&p, &fake(&p, &bad_in, &bad_in, PING_LOSS, PING_LOSS));
        assert!(v.last().unwrap().starts_with("→ 双向都丢包"), "{v:?}");
        // 什么都没测到
        let v = judge(&p, &fake(&p, &good, &good, PING_OK, PING_OK));
        assert!(v.last().unwrap().starts_with("→ 这次没测到明显丢包"), "{v:?}");
        // 家宽代理：只有入方向
        let px = plan(false, None);
        let v = judge(&px, &fake(&px, &bad_in, &good, PING_OK, PING_OK));
        assert_eq!(v.len(), 3, "{v:?}");
        assert!(v[2].contains("家宽代理没有 SSH"), "{v:?}");
    }

    /// 报告长什么样（也是交付说明里的样例）：`cargo test -p fleetctl diag::tests::sample -- --nocapture`
    #[test]
    fn sample() {
        let p = plan(true, None);
        let bad_in = tcp_out(20, &[7], &[3, 9, 15]);
        let good = tcp_out(20, &[], &[]);
        let qp = Params { count: 20, tcp: 20, ..P };
        let mut got = fake(&p, &bad_in, &good, PING_OK, PING_OK);
        for g in &mut got {
            g.cmd = cmdline(&g.job, &qp);
        }
        // 线路机2 上没有 mtr
        let i = got.iter().position(|g| g.job.kind == Kind::Mtr && g.job.from.starts_with("线路机2")).unwrap();
        got[i].note = Some("线路机2 上没有 mtr，只有上面的 ping；要 mtr 就加 --install-mtr --yes 现装：fleetctl diag 团队1-01 --install-mtr --yes".into());
        let v = judge(&p, &got);
        let r = render(&p, &qp, "2026-10-27 03:10:11", &got, &v);
        println!("{r}");
        assert!(r.starts_with("fleetctl diag 团队1-01（203.0.113.5，DeerVPS，挂在 线路机1）  开始于 2026-10-27 03:10:11 UTC\n"));
        assert!(r.contains("\n=== 线路机1 198.51.100.1 → 团队1-01 203.0.113.5:8388 · TCP 握手（入站） ===\n# 2026-10-27T03:10:12Z UTC  TCP 握手 20 次 → 203.0.113.5:8388（每次超时 5 秒、间隔 0.2 秒）\n"));
        assert!(r.contains("  1–10  152.3 152.3 1152.3r1 152.3 152.3 152.3 超时 152.3 1152.3r1 152.3\n 11–20  152.3 152.3 152.3 152.3 1152.3r1 152.3 152.3 152.3 152.3 152.3\n结果：20 次，连上 19，超时 1，首次握手被丢 3（按内核 TCP_INFO 的重传数）；最快 152.3 ms"), "{r}");
        assert!(r.contains("（线路机2 上没有 mtr"));
        assert!(r.contains("\n=== 判断 ===\n入站握手（所挂线路机 → 节点 SS 端口）：20%（20 次里超时 1、首次握手被丢 3）\n"));
    }
}
