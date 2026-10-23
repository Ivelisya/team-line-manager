//! 系统调优档 tune-v2：上架时紧跟系统基线固定执行（converge / converge_relay），已有机器用作业 `tune` 重做，`tune` 带 `rollback` 时按原值快照撤销。
//! 调优档就是这里的代码常量，跟中控版本走：改任何一项值或推导方法都升版本号（tune-v3 …），实验室验证、部署中控后 `fleetctl tune --all` 重做。
//! 选值依据与每一项的理由见 docs/DESIGN.md §3「系统调优」。
//!
//! tune-v2 相对 tune-v1：TCP 缓冲区上限不再固定 16 MB，而是按这台机器推导（`buf_max`，借鉴 tcpfit 的 2×BDP+2 MiB，只搬这一处推导）：
//! 带宽取套餐（中控渲染时代入），RTT 线路机在远端被动采样入口连接（`ss -ti`）、节点固定，内存远端读 /proc/meminfo，计算在远端 awk 里做，
//! 输入与结果原样输出进步骤结果（中控按同一公式核对）。另补几项固定值与「只纠正被改坏的」守卫项；任何「至少」键都不往下调。
//!
//! 远端脚本逐键直接写 `/proc/sys/…` 再读回，只把读回一致的键写进 `/etc/sysctl.d/99-zz-fleet-tune.conf`（容器里写不了的键不进文件，
//! 开机时也不报错）。`nf_conntrack_max` 存在（模块已加载）时另写 `/etc/modules-load.d/90-fleet-conntrack.conf`：开机时
//! systemd-modules-load 先于 systemd-sysctl 载入 nf_conntrack，这个键才设得上（不然要等防火墙规则加载才有，设置就丢了）。
//! 第一次动键之前（`/var/lib/fleet/tune-orig.conf` 不存在时）把本档每个键的原值记进这个文件（0600），回滚按它写回。
//! 逐键结果就是核对凭据，原样存进步骤结果（`parse`）。

use serde_json::{json, Map, Value};

/// 当前调优档。`machine.tune_profile` 记机器上最近一次成功应用的档；和它不一样就是「旧档」
pub const PROFILE: &str = "tune-v2";

const MIB: i64 = 1 << 20;
/// 缓冲区上限的下限：tune-v1 的固定值，推导结果不会比它小（不会因为换档把已有机器的缓冲调小）
pub const BUF_FLOOR: i64 = 16 * MIB;
/// 缓冲区上限的绝对封顶：代理机的单流速率受客户家宽限制，再大没有收益，只多占内存（tcpfit 是 256 MiB）
pub const BUF_CEIL: i64 = 64 * MIB;
/// 2×BDP 之外再加的余量（tcpfit 实测 300M/168ms 时加 2 MiB 才跑满）
const BUF_PAD: i64 = 2 * MIB;
/// 套餐没有带宽时按 1000 Mbps 算；参与计算前也封顶 1000（单个客户经家宽跨境的单流几乎不会超过 1G）
pub const BW_DEFAULT: i64 = 1000;
pub const BW_CAP: i64 = 1000;
/// 节点的 RTT 固定（不采样）：对端是线路机（美国本地）和目标网站，结果基本落在下限上
pub const RTT_NODE: i64 = 100;
/// 线路机采样不够时用的 RTT；采到的 P90 夹在这个范围内（中国客户到美国线路机 130–200 ms，晚高峰更高）
pub const RTT_RELAY_DEFAULT: i64 = 200;
pub const RTT_RELAY_RANGE: (i64, i64) = (150, 300);
/// 线路机入口连接的 RTT 样本至少这么多条才用 P90，否则用默认值
pub const RTT_MIN_SAMPLES: i64 = 20;

/// 原值快照（机器上）：本档第一次动键之前记下，回滚按它写回
pub const ORIG: &str = "/var/lib/fleet/tune-orig.conf";

/// 机器的角色：定哪些键适用，线路机另带入口端口池（`relay.port_min-port_max`：保留端口、RTT 采样都按它）
#[derive(Clone, Copy, Debug)]
pub enum Role {
    Node,
    Relay { port_min: i32, port_max: i32 },
}

impl Role {
    /// 本角色在用的服务单元：核对它的 LimitNOFILE
    fn unit(self) -> &'static str {
        match self {
            Role::Node => "fleet-ssserver",
            Role::Relay { .. } => "fleet-xray",
        }
    }
}

/// 参与计算的带宽（Mbps）与来源：套餐带宽（封顶 1000），套餐没写带宽时 1000
pub fn bandwidth(plan_mbps: Option<i32>) -> (i64, &'static str) {
    match plan_mbps.filter(|&b| b > 0) {
        Some(b) => ((b as i64).min(BW_CAP), "plan"),
        None => (BW_DEFAULT, "default"),
    }
}

/// 缓冲区上限（字节）：`clamp(2 × BW × RTT / 8 + 2 MiB, 16 MiB, min(MemTotal / 32, 64 MiB))`，下限优先（内存很小时也不低于 16 MiB）。
/// `bw` Mbps、`rtt` ms：2 × bw·10⁶/8 × rtt/1000 = bw × rtt × 250。`mem_kb` 是 /proc/meminfo 的 MemTotal（kB），读不到传 0（结果就是下限）。
/// 远端脚本里的 awk 是同一个式子，中控用这里核对
pub fn buf_max(bw: i64, rtt: i64, mem_kb: i64) -> i64 {
    let target = bw * rtt * 250 + BUF_PAD;
    let cap = (mem_kb * 1024 / 32).min(BUF_CEIL);
    target.min(cap).max(BUF_FLOOR)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum For {
    Both,
    Node,
    Relay,
}

pub struct Knob {
    pub key: &'static str,
    pub val: &'static str,
    /// 「至少」语义：逐项取现值与目标的较大者（三元组如 tcp_rmem 也逐项比），现值已经不小于目标就保持不动（kept：内核按内存自动给的、
    /// 别人调过的，不往下改）；否则是「等于」
    pub at_least: bool,
    only: For,
}

const fn k(key: &'static str, val: &'static str, at_least: bool, only: For) -> Knob { Knob { key, val, at_least, only } }

/// 线路机的 ip_local_reserved_ports 目标值是渲染时代入的入口端口池（见 `reserved`）
const RESERVED: &str = "@reserved@";
/// 缓冲区上限：远端脚本算出 buf_max 后代入
const BUF: &str = "@BUF@";

/// 线路机上 Xray API 的监听端口（127.0.0.1），同 agent xray/render.rs 的 API_PORT：本地端口范围放宽到 65535 后要保留，
/// 免得 Xray 重启那几秒里被本机出站连接占去、Xray 绑不上
const XRAY_API_PORT: i32 = 62789;

/// 要保留的本地端口：入口端口池，加上各入口的回环探测入站端口（入口端口 + 10000，见 agent xray/render.rs 的 probe_ports）与 Xray API 端口。
/// 写成内核读回的格式（按端口排序，相邻或重叠的段合并成一段，单个端口不写成段），读回才对得上
fn reserved(min: i32, max: i32) -> String {
    let mut segs = vec![(min, max), (XRAY_API_PORT, XRAY_API_PORT)];
    if min + 10000 <= 65535 {
        segs.push((min + 10000, (max + 10000).min(65535)));
    }
    segs.sort();
    let mut merged: Vec<(i32, i32)> = vec![];
    for (a, b) in segs {
        match merged.last_mut() {
            Some(last) if a <= last.1 + 1 => last.1 = last.1.max(b),
            _ => merged.push((a, b)),
        }
    }
    merged.iter().map(|&(a, b)| if a == b { a.to_string() } else { format!("{a}-{b}") }).collect::<Vec<_>>().join(",")
}

/// 顺序就是应用顺序：ip_local_reserved_ports 要先于 ip_local_port_range 写（回滚按相反顺序写回）
pub const KNOBS: &[Knob] = &[
    k("net.core.default_qdisc", "fq", false, For::Both),
    k("net.ipv4.tcp_congestion_control", "bbr", false, For::Both),
    // 缓冲区：上限按机器推导；tcp_rmem / tcp_wmem 的默认值（中间项）保持 131072 / 16384（几百条并发的代理连接，默认值大只会多吃内存）
    k("net.core.rmem_max", BUF, true, For::Both),
    k("net.core.wmem_max", BUF, true, For::Both),
    k("net.ipv4.tcp_rmem", "4096 131072 @BUF@", true, For::Both),
    k("net.ipv4.tcp_wmem", "4096 16384 @BUF@", true, For::Both),
    k("net.ipv4.tcp_notsent_lowat", "131072", false, For::Both),
    k("net.ipv4.tcp_slow_start_after_idle", "0", false, For::Both),
    k("net.ipv4.tcp_mtu_probing", "1", false, For::Both),
    k("net.core.somaxconn", "8192", true, For::Both),
    k("net.ipv4.tcp_max_syn_backlog", "8192", true, For::Both),
    k("net.core.netdev_max_backlog", "16384", true, For::Both),
    k("net.core.optmem_max", "65536", true, For::Both),
    k("net.ipv4.tcp_fin_timeout", "30", false, For::Both),
    // 只影响开了 SO_KEEPALIVE 的连接：客户端掉线留下的半死连接早点清掉
    k("net.ipv4.tcp_keepalive_time", "600", false, For::Both),
    k("net.ipv4.tcp_tw_reuse", "1", false, For::Both),
    // systemd ≥ 240 的机器默认是 LONG_MAX：「至少」，绝不往下改
    k("fs.file-max", "1000000", true, For::Both),
    // 守卫项：内核默认就是 1，只纠正被改成 0 的机器（2 也是合法取值，如 syncookies=2 总发 cookie，保持）
    k("net.ipv4.tcp_sack", "1", true, For::Both),
    k("net.ipv4.tcp_dsack", "1", true, For::Both),
    k("net.ipv4.tcp_timestamps", "1", true, For::Both),
    k("net.ipv4.tcp_window_scaling", "1", true, For::Both),
    k("net.ipv4.tcp_syncookies", "1", true, For::Both),
    k("net.ipv4.tcp_moderate_rcvbuf", "1", true, For::Both),
    // 键存在（conntrack 模块已加载）才设；表满会直接丢新连接
    k("net.netfilter.nf_conntrack_max", "131072", true, For::Node),
    k("net.netfilter.nf_conntrack_max", "262144", true, For::Relay),
    k("net.ipv4.ip_local_reserved_ports", RESERVED, false, For::Relay),
    k("net.ipv4.ip_local_port_range", "10240 65535", false, For::Relay),
];

/// 本角色适用的键（目标值已代入；缓冲区上限仍是 `@BUF@`，远端代入）：(符号, 键, 目标值)，符号 `=` / `>=`
pub fn knobs(role: Role) -> Vec<(&'static str, &'static str, String)> {
    KNOBS
        .iter()
        .filter(|x| matches!((x.only, role), (For::Both, _) | (For::Node, Role::Node) | (For::Relay, Role::Relay { .. })))
        .map(|x| {
            let val = match (x.val, role) {
                (RESERVED, Role::Relay { port_min, port_max }) => reserved(port_min, port_max),
                (v, _) => v.to_string(),
            };
            (if x.at_least { ">=" } else { "=" }, x.key, val)
        })
        .collect()
}

/// 远端脚本模板。键表以变量 KN 嵌入；脚本的 stdin 就是脚本本身（bash -s），会读标准输入的命令一律 `</dev/null`。
/// 只有写文件失败（含原值快照）才退出非 0；键缺失 / 写不进、时间没同步上都不失败，记在输出里
const TUNE: &str = r##"
set -u
export LC_ALL=C
PS=/proc/sys
MI=/proc/meminfo
CONF=/etc/sysctl.d/99-zz-fleet-tune.conf
ORIG=__ORIG__
ct=0; command -v systemd-detect-virt >/dev/null && systemd-detect-virt -cq && ct=1
modprobe tcp_bbr 2>/dev/null </dev/null || true
KN=$(cat <<'KNOBS'
__KNOBS__
KNOBS
)
# ── 缓冲区上限：clamp(2×BW×RTT/8 + 2 MiB, 下限, min(MemTotal/32, 封顶))；BW 由中控按套餐代入 ──
BW=__BW__; echo "bw=$BW"; echo "bw_plan=__BW_PLAN__"; echo "bw_source=__BW_SRC__"
rtt=__RTT_DEF__; rtt_n=0; rtt_src=default
if [ __PMAX__ -gt 0 ]; then
  # 线路机：已建立的入口连接（本地端口在入口端口池内）的 RTT（tcp_info 的 rtt，ms），P90 夹在范围内；样本不够用默认值
  # 连接行（顶格）的第一个「地址:端口」是本地端；-i 的详情在下一行（缩进），其中 rtt:平均/方差
  S=$(ss -tin state established 2>/dev/null </dev/null | awk -v lo=__PMIN__ -v hi=__PMAX__ '
  /^[^ \t]/ { ok = 0; for (i = 1; i <= NF; i++) if ($i ~ /:[0-9]+$/) { p = $i; sub(/.*:/, "", p); ok = (p + 0 >= lo && p + 0 <= hi); break } }
  ok { for (i = 1; i <= NF; i++) if ($i ~ /^rtt:[0-9.]+\//) { v = $i; sub(/^rtt:/, "", v); sub(/\/.*/, "", v); print v } }' \
    | sort -n | awk '{ a[NR] = $1 } END { n = NR + 0; if (n == 0) { print 0; exit } print n, a[int((n * 9 + 9) / 10)] }')
  rtt_n=${S%% *}; p90=${S#* }
  if [ "$rtt_n" -ge __RTT_N__ ] 2>/dev/null; then
    rtt=$(awk -v v="$p90" -v lo=__RTT_LO__ -v hi=__RTT_HI__ 'BEGIN { v = int(v + 0.5); if (v < lo) v = lo; if (v > hi) v = hi; print v }')
    rtt_src=sampled
  fi
fi
mem_kb=$(awk '/^MemTotal:/ { print $2; exit }' $MI 2>/dev/null)
buf=$(awk -v bw="$BW" -v rtt="$rtt" -v mem="${mem_kb:-0}" -v lo=__FLOOR__ -v ceil=__CEIL__ -v pad=__PAD__ \
  'BEGIN { t = bw * rtt * 250 + pad; c = mem * 1024 / 32; if (c > ceil) c = ceil; v = t; if (v > c) v = c; if (v < lo) v = lo; printf "%.0f\n", v }')
echo "rtt=$rtt"; echo "rtt_source=$rtt_src"; echo "rtt_samples=$rtt_n"; echo "mem_kb=${mem_kb:-}"; echo "buf_max=$buf"
case $buf in ''|*[!0-9]*) echo "调优：缓冲区上限算不出来" >&2; exit 1;; esac
# ── 原值快照：第一次动键之前把本档每个键的现值记下（已有就不动，保留最早那份）──
if [ -e $ORIG ]; then snap=exists
else
  snap=created; [ -e $CONF ] && snap=created_tuned
  [ -d ${ORIG%/*} ] || install -d -m 755 ${ORIG%/*} || exit 1
  ( umask 077
    { printf '# fleet 调优前的原值（__PROFILE__ 首次调优时记下，回滚按它写回；勿手改）\n# at=%s prior=%s\n' "$(date -u +%FT%TZ)" "$snap"
      while read -r op key val; do
        p=$PS/$(echo "$key" | tr . /)
        if [ -e "$p" ]; then printf '%s = %s\n' "$key" "$(tr -s ' \t' ' ' < "$p" 2>/dev/null)"; fi
      done <<< "$KN"
    } > $ORIG.tmp ) || exit 1
  chmod 600 $ORIG.tmp && mv $ORIG.tmp $ORIG || exit 1
fi
echo "snapshot=$snap"
# 「至少」：逐项取现值与目标的较大者（项数不同或不是数字时取目标）
atleast() { awk -v c="$1" -v t="$2" 'BEGIN { n = split(c, a, " "); m = split(t, b, " "); if (n != m) { print t; exit }
  for (i = 1; i <= m; i++) if (a[i] !~ /^[0-9]+$/ || b[i] !~ /^[0-9]+$/) { print t; exit }
  o = ""; for (i = 1; i <= m; i++) o = o (i > 1 ? " " : "") (a[i] + 0 > b[i] + 0 ? a[i] : b[i]); print o }'; }
install -d -m 755 /etc/sysctl.d || exit 1
printf '%s\n' '# fleet __PROFILE__（中控生成，勿手改）' > $CONF.tmp || exit 1
while read -r op key val; do
  val=${val//@BUF@/$buf}
  p=$PS/$(echo "$key" | tr . /)
  [ -e "$p" ] || { echo "tune.$key=missing"; continue; }
  cur=$(tr -s ' \t' ' ' < "$p" 2>/dev/null)
  want=$val
  if [ "$op" = '>=' ]; then
    want=$(atleast "$cur" "$val")
    if [ "$want" = "$cur" ] && [ "$cur" != "$val" ]; then echo "tune.$key=kept:$cur"; continue; fi
  fi
  st=ok
  if [ "$cur" != "$want" ]; then
    { printf '%s' "$want" > "$p"; } 2>/dev/null
    got=$(tr -s ' \t' ' ' < "$p" 2>/dev/null)
    [ "$got" = "$want" ] || { echo "tune.$key=readonly:$got"; continue; }
    st="applied:$cur"
  fi
  echo "tune.$key=$st"
  printf '%s = %s\n' "$key" "$want" >> $CONF.tmp || exit 1
done <<< "$KN"
if cmp -s $CONF.tmp $CONF; then rm -f $CONF.tmp; else mv $CONF.tmp $CONF || exit 1; fi
if [ -e $PS/net/netfilter/nf_conntrack_max ]; then
  M=/etc/modules-load.d/90-fleet-conntrack.conf
  install -d -m 755 /etc/modules-load.d || exit 1
  printf '%s\n' '# fleet __PROFILE__（中控生成，勿手改）' nf_conntrack > $M.tmp || exit 1
  if cmp -s $M.tmp $M; then rm -f $M.tmp; else mv $M.tmp $M || exit 1; fi
fi
rm -f /etc/sysctl.d/90-fleet-bbr.conf
J=/etc/systemd/journald.conf.d/90-fleet.conf
install -d -m 755 /etc/systemd/journald.conf.d || exit 1
printf '%s\n' '# fleet __PROFILE__（中控生成，勿手改）' '[Journal]' 'SystemMaxUse=300M' > $J.tmp || exit 1
if cmp -s $J.tmp $J; then rm -f $J.tmp; echo journald=same
else mv $J.tmp $J || exit 1; systemctl restart systemd-journald 2>/dev/null </dev/null; echo journald=changed; fi
ntp_ok() { timedatectl show -p NTPSynchronized --value 2>/dev/null </dev/null; }
ntp=container
if [ $ct = 0 ]; then
  if [ "$(ntp_ok)" = yes ]; then ntp=synced
  else
    if ! command -v chronyd >/dev/null && ! command -v ntpd >/dev/null && [ ! -x /lib/systemd/systemd-timesyncd ] && [ ! -x /usr/lib/systemd/systemd-timesyncd ]; then
      export DEBIAN_FRONTEND=noninteractive
      timeout 120 apt-get update -qq >/dev/null 2>&1 </dev/null || true
      timeout 240 apt-get install -y -qq systemd-timesyncd >/dev/null 2>&1 </dev/null || true
    fi
    timedatectl set-ntp true 2>/dev/null </dev/null || true
    ntp=unsynced
    for _ in $(seq 30); do if [ "$(ntp_ok)" = yes ]; then ntp=synced; break; fi; sleep 1; done
  fi
fi
echo "ntp=$ntp"
# ── 第三方调优残留（只报告，不删）与默认路由上的初始窗口（只记录）──
[ -e /etc/sysctl.d/99-tcpfit.conf ] && echo "residue=tcpfit_conf"
{ [ -e /etc/systemd/system/tcpfit-qdisc.service ] || [ -e /usr/local/sbin/tcpfit-qdisc.sh ]; } && echo "residue=tcpfit_qdisc"
if command -v tc >/dev/null; then
  htb=$(tc qdisc show 2>/dev/null </dev/null | awk '$2 == "htb" && / root / { for (i = 3; i < NF; i++) if ($i == "dev") print $(i + 1) }' | sort -u | paste -sd, -)
  [ -n "$htb" ] && echo "residue=htb:$htb"
fi
if command -v ip >/dev/null; then
  r=$(ip -4 route show default 2>/dev/null </dev/null | head -1)
  for w in initcwnd initrwnd; do echo "route_$w=$(printf '%s\n' "$r" | awk -v w=$w '{ for (i = 1; i < NF; i++) if ($i == w) { print $(i + 1); exit } }')"; done
fi
echo "nofile=$(systemctl show -p LimitNOFILE --value __UNIT__ 2>/dev/null </dev/null)"
echo "mem_mb=$(awk '/MemTotal/{print int($2/1024)}' $MI)"
echo "swap_mb=$(awk '/SwapTotal/{print int($2/1024)}' $MI)"
echo "container=$ct"
echo "tune_profile=__PROFILE__"
"##;

/// 渲染本角色的调优脚本。`plan_mbps`：套餐带宽（没有为 None）
pub fn script(role: Role, plan_mbps: Option<i32>) -> String {
    let table: String = knobs(role).iter().map(|(op, key, val)| format!("{op} {key} {val}\n")).collect();
    let (bw, src) = bandwidth(plan_mbps);
    let (rtt_def, pmin, pmax) = match role {
        Role::Node => (RTT_NODE, 0, 0),
        Role::Relay { port_min, port_max } => (RTT_RELAY_DEFAULT, port_min, port_max),
    };
    let n = |v: i64| v.to_string();
    TUNE.replace("__PROFILE__", PROFILE)
        .replace("__UNIT__", role.unit())
        .replace("__ORIG__", ORIG)
        .replace("__KNOBS__", table.trim_end())
        .replace("__BW_PLAN__", &plan_mbps.map(|b| b.to_string()).unwrap_or_default())
        .replace("__BW_SRC__", src)
        .replace("__BW__", &n(bw))
        .replace("__RTT_DEF__", &n(rtt_def))
        .replace("__RTT_N__", &n(RTT_MIN_SAMPLES))
        .replace("__RTT_LO__", &n(RTT_RELAY_RANGE.0))
        .replace("__RTT_HI__", &n(RTT_RELAY_RANGE.1))
        .replace("__PMIN__", &pmin.to_string())
        .replace("__PMAX__", &pmax.to_string())
        .replace("__FLOOR__", &n(BUF_FLOOR))
        .replace("__CEIL__", &n(BUF_CEIL))
        .replace("__PAD__", &n(BUF_PAD))
}

/// 回滚脚本：按原值快照逐键写回（按记录的相反顺序：先收窄本地端口范围再清保留端口），读回核对；删掉调优写的 sysctl 与开机载入文件。
/// journald 体积上限、时间同步不撤（无害）。没有快照时输出 `tune_rollback=nosnapshot`，什么都不改
const ROLLBACK: &str = r##"
set -u
PS=/proc/sys
ORIG=__ORIG__
[ -e $ORIG ] || { echo tune_rollback=nosnapshot; exit 0; }
grep -v '^#' $ORIG | grep ' = ' | tac | while IFS= read -r line; do
  key=${line%% = *}; val=${line#* = }
  p=$PS/$(echo "$key" | tr . /)
  [ -e "$p" ] || { echo "orig.$key=missing"; continue; }
  cur=$(tr -s ' \t' ' ' < "$p" 2>/dev/null)
  [ "$cur" = "$val" ] && { echo "orig.$key=ok"; continue; }
  { printf '%s\n' "$val" > "$p"; } 2>/dev/null
  got=$(tr -s ' \t' ' ' < "$p" 2>/dev/null)
  if [ "$got" = "$val" ]; then echo "orig.$key=applied:$cur"; else echo "orig.$key=readonly:$got"; fi
done
rm -f /etc/sysctl.d/99-zz-fleet-tune.conf /etc/modules-load.d/90-fleet-conntrack.conf || exit 1
echo "container=$(command -v systemd-detect-virt >/dev/null && systemd-detect-virt -cq && echo 1 || echo 0)"
echo tune_rollback=done
"##;

pub fn rollback_script() -> String { ROLLBACK.replace("__ORIG__", ORIG) }

/// 单个键的结果
enum Status<'a> {
    Ok,
    /// 改之前的值
    Applied(&'a str),
    Kept,
    Missing,
    Readonly(&'a str),
}

fn knob_status(s: &str) -> Option<Status<'_>> {
    Some(match s.split_once(':') {
        None => match s {
            "ok" => Status::Ok,
            "missing" => Status::Missing,
            _ => return None,
        },
        Some(("applied", old)) => Status::Applied(old),
        Some(("kept", _)) => Status::Kept,
        Some(("readonly", got)) => Status::Readonly(got),
        Some(_) => return None,
    })
}

/// 解析后的脚本输出：`result` 存进步骤结果（可核对的凭据），`warns` 逐条记作业警告，`infos` 记作业日志（不算问题）
pub struct Report {
    pub result: Value,
    pub warns: Vec<String>,
    pub infos: Vec<String>,
}

/// 逐键结果：(applied, ok, kept, skipped, before, readonly)；`prefix` 是 `tune.`（调优）或 `orig.`（回滚）
type Keys<'a> = (Vec<&'a str>, Vec<&'a str>, Vec<&'a str>, Vec<Value>, Map<String, Value>, Vec<&'a str>);

fn keys<'a>(stdout: &'a str, prefix: &str) -> Keys<'a> {
    let mut r: Keys = Default::default();
    for l in stdout.lines() {
        let Some((key, st)) = l.strip_prefix(prefix).and_then(|x| x.split_once('=')) else { continue };
        match knob_status(st) {
            Some(Status::Applied(old)) => {
                r.0.push(key);
                r.4.insert(key.to_string(), json!(old));
            }
            Some(Status::Ok) => r.1.push(key),
            Some(Status::Kept) => r.2.push(key),
            Some(Status::Missing) => r.3.push(json!({"key": key, "why": "missing", "got": null})),
            Some(Status::Readonly(got)) => {
                r.3.push(json!({"key": key, "why": "readonly", "got": got}));
                r.5.push(key);
            }
            None => {}
        }
    }
    r
}

fn kv<'a>(stdout: &'a str, name: &str) -> &'a str {
    stdout.lines().rev().find_map(|l| l.strip_prefix(name).and_then(|r| r.strip_prefix('='))).unwrap_or("")
}

/// 第三方调优残留（`residue=…` 行）：(代号, 说明)。只报告，不删
fn residue_text(r: &str) -> String {
    match r.split_once(':') {
        _ if r == "tcpfit_conf" => "发现第三方调优脚本 tcpfit 的 /etc/sysctl.d/99-tcpfit.conf：开机时先于我们的配置生效，两边都设的键以我们为准，\
            其余（tcp_mem、rmem/wmem_default、ip_local_port_range=1024 65535、min_free_kbytes 等）照样生效。只报告，没有删".to_string(),
        _ if r == "tcpfit_qdisc" => "发现 tcpfit 的整形服务（tcpfit-qdisc.service / tcpfit-qdisc.sh）：开机会给网卡装 HTB 限速。只报告，没有删".to_string(),
        Some(("htb", devs)) => format!("网卡 {devs} 的根队列是 HTB（限速整形，可能是第三方调优留下的）：所有出向流量被限在它的速率上。只报告，没有动"),
        _ => format!("第三方调优残留：{r}"),
    }
}

/// 解析脚本输出。结果形如 `{"profile","applied":[…],"ok":[…],"kept":[…],"skipped":[{"key","why":"missing|readonly","got"}],"before":{键:改前的值},
/// "params":{"bw","bw_plan","bw_source","rtt","rtt_source","rtt_samples","mem_mb","buf_max"},"snapshot","warnings":[…],"route":{"initcwnd","initrwnd"},
/// "journald","ntp","nofile","container","mem_mb","swap_mb","bbr"}`；`bbr` = 拥塞控制是否已是 bbr（兼容上架结果里原来的 `bbr` 字段）。
/// 缺失 / 写不进 / 保持不动都不算失败：容器里（`container`）不记警告，非容器机器上写不进的键记一条（可能是供应商限制）。
/// 缓冲区上限按输出的 bw / rtt / 内存用同一公式重算核对，不一致记警告
pub fn parse(stdout: &str) -> Report {
    let kv = |name: &str| kv(stdout, name);
    let container = kv("container") == "1";
    let (applied, ok, kept, skipped, before, mut readonly) = keys(stdout, "tune.");
    let cc = stdout.lines().find_map(|l| l.strip_prefix("tune.net.ipv4.tcp_congestion_control=")).map(knob_status);
    let bbr = matches!(cc, Some(Some(Status::Ok | Status::Applied(_))));
    let (mut warns, mut infos) = (vec![], vec![]);
    if !bbr {
        warns.push("拥塞控制没能切到 BBR（容器里属正常）".to_string());
    }
    readonly.retain(|k| *k != "net.ipv4.tcp_congestion_control");
    if !container && !readonly.is_empty() {
        warns.push(format!("这台机器上 {} 写不进（可能是供应商限制），保持原值", readonly.join("、")));
    }
    if kv("ntp") == "unsynced" {
        warns.push("系统时钟没有同步：已启用时间同步，30 秒内还没同步上".to_string());
    }
    let nofile = kv("nofile");
    if nofile.parse::<u64>().is_ok_and(|n| n < 65536) {
        warns.push(format!("服务的 LimitNOFILE 是 {nofile}，低于 65536（单元被改过？）"));
    }
    let num = |name: &str| kv(name).parse::<i64>().ok();
    // 推导凭据：中控按同一公式重算
    let (bw, rtt, mem_kb, buf) = (num("bw"), num("rtt"), num("mem_kb"), num("buf_max"));
    if let (Some(bw), Some(rtt), Some(buf)) = (bw, rtt, buf) {
        let want = buf_max(bw, rtt, mem_kb.unwrap_or(0));
        if want != buf {
            warns.push(format!("缓冲区上限核对不一致：远端算出 {buf}，中控按 bw={bw} rtt={rtt} mem_kb={} 算出 {want}", kv("mem_kb")));
        }
    }
    let params = buf.map(|b| {
        json!({
            "bw": bw, "bw_plan": num("bw_plan"), "bw_source": kv("bw_source"),
            "rtt": rtt, "rtt_source": kv("rtt_source"), "rtt_samples": num("rtt_samples"),
            "mem_mb": mem_kb.map(|m| m / 1024), "buf_max": b,
        })
    });
    let snapshot = kv("snapshot");
    if snapshot == "created_tuned" {
        infos.push(format!("原值快照（{ORIG}）是在已有 fleet 调优配置的机器上第一次记的：其中被旧档改过的键记的是旧档的值，回滚只能回到旧档"));
    }
    let residue: Vec<String> = stdout.lines().filter_map(|l| l.strip_prefix("residue=")).map(residue_text).collect();
    warns.extend(residue.iter().cloned());
    let route_num = |w: &str| kv(&format!("route_{w}")).parse::<i64>().ok();
    let (cwnd, rwnd) = (route_num("initcwnd"), route_num("initrwnd"));
    let route = (cwnd.is_some() || rwnd.is_some()).then(|| json!({"initcwnd": cwnd, "initrwnd": rwnd}));
    if route.is_some() {
        let w = |n: Option<i64>| n.map_or("未设".to_string(), |n| n.to_string());
        infos.push(format!("默认路由带 initcwnd {} / initrwnd {}（供应商镜像或别的脚本设的；本档不改路由，照原样保留）", w(cwnd), w(rwnd)));
    }
    let result = json!({
        "profile": kv("tune_profile"),
        "applied": applied, "ok": ok, "kept": kept, "skipped": skipped, "before": before,
        "params": params, "snapshot": (!snapshot.is_empty()).then_some(snapshot),
        "warnings": residue, "route": route,
        "journald": kv("journald"), "ntp": kv("ntp"), "nofile": nofile,
        "container": container, "mem_mb": num("mem_mb"), "swap_mb": num("swap_mb"),
        "bbr": bbr,
    });
    Report { result, warns, infos }
}

/// 解析回滚脚本输出：`{"rollback":true,"applied":[写回的键],"ok":[本来就是原值],"kept":[],"skipped":[…],"before":{键:回滚前的值},"container"}`。
/// 没有快照返回 None
pub fn parse_rollback(stdout: &str) -> Option<Report> {
    if kv(stdout, "tune_rollback") != "done" {
        return None;
    }
    let container = kv(stdout, "container") == "1";
    let (applied, ok, kept, skipped, before, readonly) = keys(stdout, "orig.");
    let mut warns = vec![];
    if !container && !readonly.is_empty() {
        warns.push(format!("{} 写不回原值，保持现值", readonly.join("、")));
    }
    let result = json!({"rollback": true, "applied": applied, "ok": ok, "kept": kept, "skipped": skipped, "before": before, "container": container});
    Some(Report { result, warns, infos: vec![] })
}

#[cfg(test)]
mod tests {
    use super::*;

    const RELAY: Role = Role::Relay { port_min: 20000, port_max: 29999 };

    #[test]
    fn relay_script_reserves_the_entry_port_pool_before_the_port_range() {
        let s = script(RELAY, Some(1000));
        // 入口端口池 20000-29999 与回环探测入站 30000-39999 相邻，合成一段；Xray API 端口单独一个
        let (res, range) = (s.find("= net.ipv4.ip_local_reserved_ports 20000-39999,62789\n").unwrap(), s.find("= net.ipv4.ip_local_port_range 10240 65535\n").unwrap());
        assert!(res < range, "先保留入口端口池，再放宽本地端口范围");
        assert!(s.contains(">= net.netfilter.nf_conntrack_max 262144\n") && !s.contains("nf_conntrack_max 131072"));
        assert!(s.contains("= net.ipv4.tcp_tw_reuse 1\n") && s.contains("show -p LimitNOFILE --value fleet-xray "));
        assert!(!s.contains("__") && !s.contains(RESERVED));
        // 入口端口池按这台线路机自己的来
        let s = script(Role::Relay { port_min: 21000, port_max: 21999 }, None);
        assert!(s.contains("= net.ipv4.ip_local_reserved_ports 21000-21999,31000-31999,62789\n"));
        // API 端口落在端口池里、紧挨着探测入站段
        assert_eq!(
            (reserved(60000, 65000), reserved(20000, 35000), reserved(42000, 52788), reserved(40000, 49999)),
            ("60000-65000".into(), "20000-45000,62789".into(), "42000-62789".into(), "40000-59999,62789".into())
        );
    }

    #[test]
    fn node_script_has_no_relay_only_keys() {
        let s = script(Role::Node, Some(50));
        for key in ["ip_local_reserved_ports", "ip_local_port_range", "nf_conntrack_max 262144"] {
            assert!(!s.contains(key), "{key}");
        }
        assert!(s.contains(">= net.netfilter.nf_conntrack_max 131072\n") && s.contains(">= net.ipv4.tcp_rmem 4096 131072 @BUF@\n"));
        assert!(s.contains(">= net.core.rmem_max @BUF@\n") && s.contains("show -p LimitNOFILE --value fleet-ssserver "));
        // 节点也设 tcp_tw_reuse；新补的固定项与守卫项
        for l in ["= net.ipv4.tcp_tw_reuse 1\n", "= net.ipv4.tcp_keepalive_time 600\n", ">= net.core.optmem_max 65536\n", ">= fs.file-max 1000000\n", ">= net.core.somaxconn 8192\n", ">= net.ipv4.tcp_syncookies 1\n"] {
            assert!(s.contains(l), "{l}");
        }
        assert!(!s.contains("__"));
    }

    /// 中控代入的值：带宽（套餐，封顶 1000；没有用 1000）、RTT 默认值、采样的端口池（节点 0 = 不采样）、上下限常量
    #[test]
    fn script_substitutes_bandwidth_rtt_and_bounds() {
        let s = script(Role::Node, Some(50));
        assert!(s.contains("\nBW=50; ") && s.contains("bw_plan=50\"") && s.contains("bw_source=plan\""), "{s}");
        assert!(s.contains("\nrtt=100; ") && s.contains("if [ 0 -gt 0 ]; then"));
        assert!(s.contains("-v lo=16777216 -v ceil=67108864 -v pad=2097152 "));
        let s = script(RELAY, Some(2560));
        assert!(s.contains("\nBW=1000; ") && s.contains("bw_plan=2560\"") && s.contains("bw_source=plan\""));
        assert!(s.contains("\nrtt=200; ") && s.contains("if [ 29999 -gt 0 ]; then") && s.contains("-v lo=20000 -v hi=29999 "));
        assert!(s.contains("-ge 20 ]") && s.contains("-v lo=150 -v hi=300 "));
        let s = script(RELAY, None);
        assert!(s.contains("\nBW=1000; ") && s.contains("bw_plan=\"") && s.contains("bw_source=default\""));
        assert_eq!((bandwidth(Some(0)), bandwidth(Some(300))), ((1000, "default"), (300, "plan")));
        assert!(!rollback_script().contains("__") && rollback_script().contains("ORIG=/var/lib/fleet/tune-orig.conf\n"));
    }

    /// 缓冲区上限的边界（MemTotal 取实机读数：4G 套餐约 3.9 GB、2G 约 1.95 GB、1G 约 0.96 GB）
    #[test]
    fn buf_max_bounds() {
        let (m4, m2, m1) = (4_005_000, 2_000_000, 983_040);
        // 50M 节点：BDP 很小，落在下限 16 MiB
        assert_eq!(buf_max(bandwidth(Some(50)).0, RTT_NODE, m4), BUF_FLOOR);
        // 1000M、2G 线路机，RTT 200：2×1000M×0.2s/8 + 2 MiB = 52097152（内存封顶 2000000×32 = 64000000，没碰到）
        assert_eq!(buf_max(1000, 200, m2), 52_097_152);
        // 2560M 套餐封顶 1000：RTT 200 同上；RTT 300 时 77097152 被 64 MiB 封顶
        let bw = bandwidth(Some(2560)).0;
        assert_eq!((buf_max(bw, 200, m4), buf_max(bw, 300, m4)), (52_097_152, BUF_CEIL));
        // 2G 内存、RTT 300：内存封顶 MemTotal/32
        assert_eq!(buf_max(1000, 300, m2), 64_000_000);
        // 1G 内存：MemTotal/32 = 31457280
        assert_eq!(buf_max(1000, 200, m1), 31_457_280);
        // 内存读不到 / 极小：下限优先
        assert_eq!((buf_max(1000, 200, 0), buf_max(1000, 200, 300_000)), (BUF_FLOOR, BUF_FLOOR));
    }

    #[test]
    fn knobs_are_unique_per_role() {
        for role in [Role::Node, RELAY] {
            let keys: Vec<_> = knobs(role).iter().map(|x| x.1).collect();
            let mut u = keys.clone();
            u.sort();
            u.dedup();
            assert_eq!(u.len(), keys.len(), "{role:?}");
        }
    }

    #[test]
    fn parse_reports_every_key_and_warns_only_where_it_matters() {
        let out = "bw=1000\nbw_plan=2560\nbw_source=plan\nrtt=164\nrtt_source=sampled\nrtt_samples=37\nmem_kb=4005000\nbuf_max=43097152\nsnapshot=created\n\
                   tune.net.ipv4.tcp_congestion_control=ok\ntune.net.core.rmem_max=applied:212992\ntune.net.core.somaxconn=kept:65535\n\
                   tune.net.netfilter.nf_conntrack_max=missing\ntune.net.ipv4.tcp_fin_timeout=readonly:\n\
                   journald=changed\nntp=synced\nroute_initcwnd=10000\nroute_initrwnd=10000\nnofile=65536\nmem_mb=3911\nswap_mb=0\ncontainer=0\ntune_profile=tune-v2\n";
        let r = parse(out);
        let j = &r.result;
        assert_eq!((j["profile"].as_str(), j["bbr"].as_bool(), j["mem_mb"].as_i64(), j["container"].as_bool()), (Some("tune-v2"), Some(true), Some(3911), Some(false)));
        assert_eq!((&j["applied"], &j["ok"], &j["kept"]), (&json!(["net.core.rmem_max"]), &json!(["net.ipv4.tcp_congestion_control"]), &json!(["net.core.somaxconn"])));
        assert_eq!(j["before"], json!({"net.core.rmem_max": "212992"}));
        assert_eq!(j["skipped"], json!([{"key": "net.netfilter.nf_conntrack_max", "why": "missing", "got": null}, {"key": "net.ipv4.tcp_fin_timeout", "why": "readonly", "got": ""}]));
        assert_eq!(
            j["params"],
            json!({"bw": 1000, "bw_plan": 2560, "bw_source": "plan", "rtt": 164, "rtt_source": "sampled", "rtt_samples": 37, "mem_mb": 3911, "buf_max": 43097152})
        );
        assert_eq!((&j["snapshot"], &j["route"], &j["warnings"]), (&json!("created"), &json!({"initcwnd": 10000, "initrwnd": 10000}), &json!([])));
        assert_eq!(r.warns, ["这台机器上 net.ipv4.tcp_fin_timeout 写不进（可能是供应商限制），保持原值"]);
        assert!(r.infos.len() == 1 && r.infos[0].contains("initcwnd 10000"), "初始窗口只记录，不算问题：{:?}", r.infos);
        // 容器里：写不进不记警告（BBR 没切上、时钟用宿主机的照常只是结果）
        let r = parse("tune.net.ipv4.tcp_congestion_control=readonly:cubic\ntune.net.ipv4.tcp_fin_timeout=readonly:60\nntp=container\nnofile=1048576\ncontainer=1\ntune_profile=tune-v2\n");
        assert_eq!((r.result["bbr"].as_bool(), r.warns.len(), &r.result["params"], &r.result["route"]), (Some(false), 1, &Value::Null, &Value::Null));
        // 时钟没同步上、单元的文件数限制被改低
        let r = parse("ntp=unsynced\nnofile=4096\ncontainer=0\ntune_profile=tune-v2\n");
        assert!(r.warns.iter().any(|w| w.contains("时钟")) && r.warns.iter().any(|w| w.contains("4096")), "{:?}", r.warns);
    }

    /// 推导核对、残留检测、改之前的值是空串
    #[test]
    fn parse_checks_the_derivation_and_reports_residue() {
        let r = parse("bw=1000\nrtt=200\nmem_kb=2000000\nbuf_max=16777216\ntune.net.ipv4.tcp_congestion_control=applied:\nsnapshot=created_tuned\n\
                       residue=tcpfit_conf\nresidue=htb:eth0,ens3\ncontainer=0\ntune_profile=tune-v2\n");
        assert!(r.warns.iter().any(|w| w.contains("核对不一致") && w.contains("52097152")), "{:?}", r.warns);
        assert!(r.warns.iter().any(|w| w.contains("99-tcpfit.conf")) && r.warns.iter().any(|w| w.contains("eth0,ens3")), "{:?}", r.warns);
        assert_eq!((r.result["warnings"].as_array().unwrap().len(), &r.result["before"], r.result["bbr"].as_bool()), (2, &json!({"net.ipv4.tcp_congestion_control": ""}), Some(true)));
        assert!(r.infos.iter().any(|i| i.contains("快照")), "{:?}", r.infos);
        assert_eq!(r.result["params"]["mem_mb"], 1953);
    }

    #[test]
    fn parse_rollback_needs_the_snapshot() {
        assert!(parse_rollback("tune_rollback=nosnapshot\n").is_none());
        let r = parse_rollback("orig.net.ipv4.ip_local_port_range=applied:10240 65535\norig.net.core.rmem_max=ok\norig.net.core.wmem_max=readonly:16777216\ncontainer=0\ntune_rollback=done\n").unwrap();
        assert_eq!((&r.result["applied"], &r.result["before"]), (&json!(["net.ipv4.ip_local_port_range"]), &json!({"net.ipv4.ip_local_port_range": "10240 65535"})));
        assert_eq!(r.warns, ["net.core.wmem_max 写不回原值，保持现值"]);
    }
}
