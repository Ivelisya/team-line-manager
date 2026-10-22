//! 远端脚本。全部幂等：内容相同就不动；脚本最后输出 `key=value` 行给中控断言。
//! 参数通过脚本开头的 shell 变量注入（`vars()` 生成，单引号转义），秘密只出现在 ssh 的 stdin 里。

/// 生成 `K='v'` 赋值行
pub fn vars(kv: &[(&str, &str)]) -> String {
    kv.iter().map(|(k, v)| format!("{k}='{}'\n", v.replace('\'', "'\\''"))).collect()
}

/// 预检：采集事实
pub const FACTS: &str = r#"
set -u
echo "arch=$(uname -m)"
. /etc/os-release 2>/dev/null; echo "os=${ID:-?}-${VERSION_ID:-?}"
echo "kernel=$(uname -r)"
[ -d /run/systemd/system ] && echo systemd=1 || echo systemd=0
echo "mem_mb=$(awk '/MemTotal/{print int($2/1024)}' /proc/meminfo)"
echo "boot_id=$(cat /proc/sys/kernel/random/boot_id)"
command -v nft >/dev/null && echo nft=1 || echo nft=0
if ss -Hltn 2>/dev/null | grep -q ':8388 '; then echo port8388=busy; else echo port8388=free; fi
[ -f /etc/fleet-agent/agent.toml ] && echo "agent_machine=$(sed -n 's/^machine_id *= *"\(.*\)"/\1/p' /etc/fleet-agent/agent.toml)" || echo agent_machine=
"#;

/// 引导：把中控公钥加进 root 的 authorized_keys（用密码登录时执行）。需要变量 PUBKEY。
pub const BOOTSTRAP_KEY: &str = r#"
set -eu
umask 077; mkdir -p /root/.ssh; touch /root/.ssh/authorized_keys
grep -qxF "$PUBKEY" /root/.ssh/authorized_keys || echo "$PUBKEY" >> /root/.ssh/authorized_keys
chmod 600 /root/.ssh/authorized_keys
echo bootstrap=ok
"#;

/// 系统基线：缺的基础包补上（线路机访问日志轮转要 logrotate，精简系统可能没装）、sshd 只允许密钥、BBR（容器里可能设不了，只报告不失败）
pub const BASELINE: &str = r#"
set -eu
export DEBIAN_FRONTEND=noninteractive
need=""; command -v nft >/dev/null || need="$need nftables"; command -v curl >/dev/null || need="$need curl"
command -v logrotate >/dev/null || need="$need logrotate"
if [ -n "$need" ]; then timeout 240 apt-get update -qq >/dev/null 2>&1 || true; timeout 300 apt-get install -y -qq $need >/dev/null; fi
install -d -m 755 /etc/ssh/sshd_config.d
F=/etc/ssh/sshd_config.d/00-fleet-keyonly.conf
printf '%s\n' '# fleet：只允许密钥登录' 'PasswordAuthentication no' 'KbdInteractiveAuthentication no' 'PermitRootLogin prohibit-password' 'PubkeyAuthentication yes' > $F.tmp
changed=0
if ! cmp -s $F.tmp $F; then mv $F.tmp $F; changed=1; else rm -f $F.tmp; fi
# sshd 同一选项只认第一次出现的值：在主配置第一行单独引入这个文件，供应商镜像里排在它前面的 00-xxx.conf、主配置里的设置都盖不过它
if ! head -n1 /etc/ssh/sshd_config | grep -qxF "Include $F"; then sed -i "1i Include $F" /etc/ssh/sshd_config; changed=1; fi
if [ $changed = 1 ]; then sshd -t; systemctl restart ssh 2>/dev/null || systemctl restart sshd; fi
echo "sshd_password=$(sshd -T 2>/dev/null | awk '/^passwordauthentication /{print $2}')"
printf '%s\n' 'net.core.default_qdisc=fq' 'net.ipv4.tcp_congestion_control=bbr' > /etc/sysctl.d/90-fleet-bbr.conf
modprobe tcp_bbr 2>/dev/null || true; sysctl -q -p /etc/sysctl.d/90-fleet-bbr.conf 2>/dev/null || true
echo "cc=$(cat /proc/sys/net/ipv4/tcp_congestion_control)"
"#;

/// 节点上的一个 ssserver 实例。宿主机（普通落地机）：单元 `fleet-ssserver`、配置 config.json、nft 表 `ss_guard`，监听 0.0.0.0:8388；
/// 同机 IP 节点：模板单元的实例 `fleet-ssserver@<端口>`、配置 `<端口>.json`、nft 表 `ss_guard_<端口>`，监听自己的 IP 并从它出站
pub struct SsInst {
    /// 本机监听端口
    pub port: u16,
    /// 同机 IP 节点自己的 IP；宿主机为 None
    pub ip: Option<String>,
}

impl SsInst {
    pub fn host() -> Self { SsInst { port: 8388, ip: None } }

    /// systemd 单元
    pub fn svc(&self) -> String {
        if self.ip.is_some() { format!("fleet-ssserver@{}", self.port) } else { "fleet-ssserver".into() }
    }

    /// nft 表名，也是 /etc/fleet/nft/ 下的文件名
    pub fn table(&self) -> String {
        if self.ip.is_some() { format!("ss_guard_{}", self.port) } else { "ss_guard".into() }
    }
}

/// 节点：ssserver + 只放行线路机的 nft 规则。变量 SS_PORT SS_METHOD SS_PASSWORD ALLOW（逗号分隔的线路机 IP）。
/// 宿主机渲染出的脚本除了开头的锁，和引入同机 IP 节点之前逐字相同：已上架的节点重新收敛时配置、单元、nft、include 行都不变，
/// 不会重启 ssserver。同机 IP 节点的 IP 直接写进配置（校验过的 IPv4，不含引号和 %）
pub fn node_ss(i: &SsInst) -> String {
    let (conf, unit, unit_conf, listen, bind) = match &i.ip {
        None => ("config.json".to_string(), "fleet-ssserver.service", "config.json", "0.0.0.0".to_string(), String::new()),
        Some(ip) => (format!("{}.json", i.port), "fleet-ssserver@.service", "%i.json", ip.clone(), format!(r#","outbound_bind_addr":"{ip}""#)),
    };
    NODE_SS
        .replace("__UNIT_CONF__", unit_conf)
        .replace("__UNIT__", unit)
        .replace("__CONF__", &conf)
        .replace("__LISTEN__", &listen)
        .replace("__BIND__", &bind)
        .replace("__SVC__", &i.svc())
        .replace("__T__", &i.table())
}

const NODE_SS: &str = r#"
set -eu
exec 9>/run/fleet-ss.lock; flock 9
id fleet-ss >/dev/null 2>&1 || useradd --system --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin fleet-ss
install -d -m 750 -g fleet-ss /etc/shadowsocks-rust
C=/etc/shadowsocks-rust/__CONF__
printf '{"server":"__LISTEN__","server_port":%s,"password":"%s","method":"%s","mode":"tcp_and_udp","timeout":300__BIND__}\n' "$SS_PORT" "$SS_PASSWORD" "$SS_METHOD" > $C.tmp
chgrp fleet-ss $C.tmp; chmod 640 $C.tmp
RESTART=0
cmp -s $C.tmp $C && rm -f $C.tmp || { mv $C.tmp $C; RESTART=1; }
U=/etc/systemd/system/__UNIT__
cat > $U.tmp <<'UNIT'
[Unit]
Description=fleet shadowsocks-rust ssserver
After=network-online.target
Wants=network-online.target
[Service]
ExecStart=/usr/local/bin/ssserver -c /etc/shadowsocks-rust/__UNIT_CONF__
User=fleet-ss
Group=fleet-ss
Restart=always
RestartSec=2
LimitNOFILE=65536
MemoryMax=128M
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
[Install]
WantedBy=multi-user.target
UNIT
cmp -s $U.tmp $U && rm -f $U.tmp || { mv $U.tmp $U; systemctl daemon-reload; RESTART=1; }
# 磁盘上的 ssserver 已换成新版（同机别的实例收敛时推的）、这个实例还在跑旧的：也重启，换上新程序；程序没换过时不重启
[ /proc/$(systemctl show -p MainPID --value __SVC__)/exe -ef /usr/local/bin/ssserver ] || RESTART=1
systemctl enable -q __SVC__
if [ $RESTART = 1 ] || ! systemctl -q is-active __SVC__; then systemctl restart __SVC__; fi
# 防火墙：SS 端口只放行线路机
install -d -m 755 /etc/fleet/nft
N=/etc/fleet/nft/__T__.nft
cat > $N.tmp <<NFT
table inet __T__
delete table inet __T__
table inet __T__ {
  set relays { type ipv4_addr; elements = { $ALLOW } }
  chain input {
    type filter hook input priority -10; policy accept;
    tcp dport $SS_PORT ip saddr @relays accept
    udp dport $SS_PORT ip saddr @relays accept
    tcp dport $SS_PORT drop
    udp dport $SS_PORT drop
  }
}
NFT
# set -e 对 && 链中间的失败不会退出，这里逐条显式判断；任何一步失败都让脚本以非 0 退出
nft -c -f $N.tmp || { echo nft=error; exit 21; }
mv -f $N.tmp $N || { echo nft=error; exit 21; }
nft -f $N || { echo nft=error; exit 21; }
[ -f /etc/nftables.conf ] || printf '#!/usr/sbin/nft -f\n' > /etc/nftables.conf
grep -qF 'include "/etc/fleet/nft/__T__.nft"' /etc/nftables.conf || echo 'include "/etc/fleet/nft/__T__.nft"' >> /etc/nftables.conf
systemctl enable -q nftables 2>/dev/null || true
for i in $(seq 1 30); do ss -Hltn "sport = :$SS_PORT" | grep -q . && break; sleep 0.2; done
ss -Hltn "sport = :$SS_PORT" | grep -q . && echo ss=listening || echo ss=down
echo "nft_relays=$(nft list set inet __T__ relays | tr -d '\n' | sed 's/.*elements = { \(.*\) }.*/\1/' | tr -d ' \t')"
"#;

/// 只更新节点防火墙放行名单（换线路机、加入口时）。变量 NFT_TABLE（`SsInst::table`）ALLOW。
pub const NODE_NFT_ONLY: &str = r#"
set -eu
N=/etc/fleet/nft/$NFT_TABLE.nft
[ -f $N ] || { echo nft=missing; exit 22; }
# 先改临时副本并校验，通过后才替换正式文件（避免留下一份开机时加载会失败的坏文件）
sed "s/elements = { [^}]* }/elements = { $ALLOW }/" $N > $N.tmp || { echo nft=error; exit 21; }
nft -c -f $N.tmp || { rm -f $N.tmp; echo nft=error; exit 21; }
mv -f $N.tmp $N || { echo nft=error; exit 21; }
nft -f $N || { echo nft=error; exit 21; }
echo "nft_relays=$(nft list set inet $NFT_TABLE relays | tr -d '\n' | sed 's/.*elements = { \(.*\) }.*/\1/' | tr -d ' \t')"
"#;

/// 同机 IP 节点上架前在宿主机上核对：本机的 IPv4、经这个 IP 访问中控 /echo 看到的来源、正在监听的端口（TCP + UDP），
/// 以及配置里本来就绑着这个 IP 的实例端口（这台节点上次退役没清理掉的旧实例：恢复时收敛会覆盖它，不算被别人占用）。
/// 变量 IP（节点自己的 IP）ECHO（中控的 /echo 地址）
pub const IP_CHECK: &str = r#"
set -u
echo "local_ips=$(ip -4 -o addr show | awk '{split($4, a, "/"); if (a[1] !~ /^127\./) print a[1]}' | paste -sd, -)"
echo "echo=$(curl -4 -sS -m 10 --interface "$IP" "$ECHO" 2>&1 | tr -d '\r\n' | head -c 200)"
echo "busy=$(ss -Hltun | awk '{n = split($5, a, ":"); print a[n]}' | sort -un | paste -sd, -)"
echo "own=$(grep -lF "\"server\":\"$IP\"" /etc/shadowsocks-rust/[0-9]*.json 2>/dev/null | sed 's#.*/##; s#\.json$##' | paste -sd, -)"
"#;

/// 线路机：fleet-xray 单元与访问日志轮转（配置由 agent 生成）。xray 二进制与 geoip.dat 已推到 /usr/local/bin、/usr/local/share/xray。
pub fn relay_xray() -> String { RELAY_XRAY.replace("__UNIT__", XRAY_UNIT.trim_end()).replace("__LOGROTATE_SH__", logrotate_sh().trim()) }

/// 升级 agent 时也装一遍访问日志轮转（较早上架、还没有这份配置或每小时定时器的线路机升级一次就补上）。
/// 只是补装：包在子 shell 里，失败只输出 `logrotate=error`，不让 agent 步骤失败
pub fn relay_logrotate() -> String { format!("(\n{}\n) || echo logrotate=error\n", logrotate_sh().trim()) }

fn logrotate_sh() -> String {
    LOGROTATE_SH
        .replace("__LOGROTATE__", XRAY_LOGROTATE.trim_end())
        .replace("__LR_SERVICE__", XRAY_LOGROTATE_SERVICE.trim_end())
        .replace("__LR_TIMER__", XRAY_LOGROTATE_TIMER.trim_end())
}

/// 线路机访问日志轮转配置（上架时随 fleet-xray 一起装，失败则该步骤失败）。
/// 系统 logrotate 每天只跑一次：另装一个每小时只跑这一份配置的定时器，maxsize 才起作用
const LOGROTATE_SH: &str = r#"
install -d -m 755 /etc/logrotate.d
L=/etc/logrotate.d/fleet-xray
cat > $L.tmp <<'LOGROTATE'
__LOGROTATE__
LOGROTATE
cmp -s $L.tmp $L && rm -f $L.tmp || mv $L.tmp $L
LR=0
T=/etc/systemd/system/fleet-xray-logrotate.service
cat > $T.tmp <<'UNIT'
__LR_SERVICE__
UNIT
cmp -s $T.tmp $T && rm -f $T.tmp || { mv $T.tmp $T; LR=1; }
T=/etc/systemd/system/fleet-xray-logrotate.timer
cat > $T.tmp <<'UNIT'
__LR_TIMER__
UNIT
cmp -s $T.tmp $T && rm -f $T.tmp || { mv $T.tmp $T; LR=1; }
[ $LR = 0 ] || systemctl daemon-reload
systemctl enable -q --now fleet-xray-logrotate.timer
"#;

/// systemd 单元、logrotate 配置：唯一来源在 deploy/
pub const AGENT_UNIT: &str = include_str!("../../../../deploy/units/fleet-agent.service");
pub const XRAY_UNIT: &str = include_str!("../../../../deploy/units/fleet-xray.service");
pub const XRAY_LOGROTATE: &str = include_str!("../../../../deploy/logrotate/fleet-xray");
pub const XRAY_LOGROTATE_SERVICE: &str = include_str!("../../../../deploy/units/fleet-xray-logrotate.service");
pub const XRAY_LOGROTATE_TIMER: &str = include_str!("../../../../deploy/units/fleet-xray-logrotate.timer");

const RELAY_XRAY: &str = r#"
set -eu
install -d -m 755 /usr/local/share/xray /etc/fleet-agent
U=/etc/systemd/system/fleet-xray.service
cat > $U.tmp <<'UNIT'
__UNIT__
UNIT
cmp -s $U.tmp $U && rm -f $U.tmp || { mv $U.tmp $U; systemctl daemon-reload; }
__LOGROTATE_SH__
systemctl enable -q fleet-xray
echo "xray_version=$(/usr/local/bin/xray version 2>/dev/null | head -1 | awk '{print $2}')"
"#;

/// 装 agent：变量 SERVER_URL ROLE MACHINE_ID，TOKEN（可空：空则沿用节点上已有的令牌文件）
pub fn agent() -> String { AGENT.replace("__UNIT__", AGENT_UNIT.trim_end()) }

const AGENT: &str = r#"
set -eu
umask 022
install -d -m 755 /etc/fleet-agent
install -d -m 700 /var/lib/fleet-agent
RESTART=0
A=/etc/fleet-agent/agent.toml
# 只写因机器而异的三项：令牌文件、数据目录、xray 路径等都用 agent 内置的默认值
{
  echo '# 由 fleet-server 生成'
  echo "server_url = \"$SERVER_URL\""
  echo "role = \"$ROLE\""
  echo "machine_id = \"$MACHINE_ID\""
} > $A.tmp
cmp -s $A.tmp $A && rm -f $A.tmp || { mv $A.tmp $A; RESTART=1; }
if [ -n "$TOKEN" ]; then
  (umask 077; printf '%s\n' "$TOKEN" > /etc/fleet-agent/token.new); mv -f /etc/fleet-agent/token.new /etc/fleet-agent/token; RESTART=1
fi
[ -s /etc/fleet-agent/token ] || { echo agent=no_token; exit 4; }
U=/etc/systemd/system/fleet-agent.service
cat > $U.tmp <<'UNIT'
__UNIT__
UNIT
cmp -s $U.tmp $U && rm -f $U.tmp || { mv $U.tmp $U; systemctl daemon-reload; RESTART=1; }
[ "${BIN_CHANGED:-0}" = 1 ] && RESTART=1
systemctl enable -q fleet-agent
if [ $RESTART = 1 ] || ! systemctl -q is-active fleet-agent; then systemctl restart fleet-agent; fi
echo "agent=$(systemctl is-active fleet-agent)"
echo "boot_id=$(cat /proc/sys/kernel/random/boot_id)"
"#;

/// 退役清理（节点）：先停 agent 与 ssserver（宿主机连同它上面同机 IP 节点的实例），删防火墙表和文件。中控随后撤销令牌。
pub const NODE_CLEANUP: &str = r##"
set -u
exec 9>/run/fleet-ss.lock; flock 9
systemctl disable --now fleet-agent 2>/dev/null; systemctl disable --now fleet-ssserver 2>/dev/null
for f in /etc/shadowsocks-rust/[0-9]*.json; do
  [ -e "$f" ] || continue
  p=$(basename "$f" .json)
  systemctl disable --now "fleet-ssserver@$p" 2>/dev/null; nft delete table inet "ss_guard_$p" 2>/dev/null
done
rm -f /etc/systemd/system/fleet-agent.service /etc/systemd/system/fleet-ssserver.service /etc/systemd/system/fleet-ssserver@.service
rm -rf /etc/fleet-agent /var/lib/fleet-agent /etc/shadowsocks-rust /usr/local/bin/ssserver /usr/local/bin/fleet-agent
nft delete table inet ss_guard 2>/dev/null; rm -f /etc/fleet/nft/ss_guard.nft /etc/fleet/nft/ss_guard_*.nft
[ -f /etc/nftables.conf ] && sed -i '\#include "/etc/fleet/nft/ss_guard#d' /etc/nftables.conf
userdel fleet-ss 2>/dev/null
systemctl daemon-reload
ss -Hltn 'sport = :8388' | grep -q . && echo ss=still_listening || echo ss=gone
echo cleanup=done
"##;

/// 退役清理（同机 IP 节点，在宿主机上执行）：只停自己的实例、删自己的配置、nft 表和 include 行；宿主机的 agent 与 ssserver 不动。变量 SS_PORT
pub const NODE_IP_CLEANUP: &str = r##"
set -u
exec 9>/run/fleet-ss.lock; flock 9
systemctl disable --now "fleet-ssserver@$SS_PORT" 2>/dev/null
rm -f "/etc/shadowsocks-rust/$SS_PORT.json"
nft delete table inet "ss_guard_$SS_PORT" 2>/dev/null; rm -f "/etc/fleet/nft/ss_guard_$SS_PORT.nft"
[ -f /etc/nftables.conf ] && sed -i "\#include \"/etc/fleet/nft/ss_guard_$SS_PORT.nft\"#d" /etc/nftables.conf
ss -Hltn "sport = :$SS_PORT" | grep -q . && echo ss=still_listening || echo ss=gone
echo cleanup=done
"##;

/// 退役清理（线路机）：停 agent 与 xray，删配置、日志轮转（含每小时的定时器）和 xray 二进制与数据文件
pub const RELAY_CLEANUP: &str = r#"
set -u
systemctl disable --now fleet-agent 2>/dev/null; systemctl disable --now fleet-xray 2>/dev/null; systemctl disable --now fleet-xray-logrotate.timer 2>/dev/null
rm -f /etc/systemd/system/fleet-agent.service /etc/systemd/system/fleet-xray.service /etc/logrotate.d/fleet-xray
rm -f /etc/systemd/system/fleet-xray-logrotate.service /etc/systemd/system/fleet-xray-logrotate.timer
rm -rf /etc/fleet-agent /var/lib/fleet-agent /var/log/fleet-xray /usr/local/bin/fleet-agent /usr/local/bin/xray /usr/local/share/xray
systemctl daemon-reload
echo cleanup=done
"#;

#[cfg(test)]
mod tests {
    use super::{node_ss, SsInst};

    #[test]
    fn vars_escapes_quotes() {
        assert_eq!(super::vars(&[("A", "it's")]), "A='it'\\''s'\n");
    }

    /// 第 17 轮（同机多 IP 节点）之前的 NODE_SS 原文
    const OLD_NODE_SS: &str = r#"
set -eu
id fleet-ss >/dev/null 2>&1 || useradd --system --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin fleet-ss
install -d -m 750 -g fleet-ss /etc/shadowsocks-rust
C=/etc/shadowsocks-rust/config.json
printf '{"server":"0.0.0.0","server_port":%s,"password":"%s","method":"%s","mode":"tcp_and_udp","timeout":300}\n' "$SS_PORT" "$SS_PASSWORD" "$SS_METHOD" > $C.tmp
chgrp fleet-ss $C.tmp; chmod 640 $C.tmp
RESTART=0
cmp -s $C.tmp $C && rm -f $C.tmp || { mv $C.tmp $C; RESTART=1; }
U=/etc/systemd/system/fleet-ssserver.service
cat > $U.tmp <<'UNIT'
[Unit]
Description=fleet shadowsocks-rust ssserver
After=network-online.target
Wants=network-online.target
[Service]
ExecStart=/usr/local/bin/ssserver -c /etc/shadowsocks-rust/config.json
User=fleet-ss
Group=fleet-ss
Restart=always
RestartSec=2
LimitNOFILE=65536
MemoryMax=128M
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
[Install]
WantedBy=multi-user.target
UNIT
cmp -s $U.tmp $U && rm -f $U.tmp || { mv $U.tmp $U; systemctl daemon-reload; RESTART=1; }
systemctl enable -q fleet-ssserver
if [ $RESTART = 1 ] || ! systemctl -q is-active fleet-ssserver; then systemctl restart fleet-ssserver; fi
# 防火墙：SS 端口只放行线路机
install -d -m 755 /etc/fleet/nft
N=/etc/fleet/nft/ss_guard.nft
cat > $N.tmp <<NFT
table inet ss_guard
delete table inet ss_guard
table inet ss_guard {
  set relays { type ipv4_addr; elements = { $ALLOW } }
  chain input {
    type filter hook input priority -10; policy accept;
    tcp dport $SS_PORT ip saddr @relays accept
    udp dport $SS_PORT ip saddr @relays accept
    tcp dport $SS_PORT drop
    udp dport $SS_PORT drop
  }
}
NFT
# set -e 对 && 链中间的失败不会退出，这里逐条显式判断；任何一步失败都让脚本以非 0 退出
nft -c -f $N.tmp || { echo nft=error; exit 21; }
mv -f $N.tmp $N || { echo nft=error; exit 21; }
nft -f $N || { echo nft=error; exit 21; }
[ -f /etc/nftables.conf ] || printf '#!/usr/sbin/nft -f\n' > /etc/nftables.conf
grep -qF 'include "/etc/fleet/nft/ss_guard.nft"' /etc/nftables.conf || echo 'include "/etc/fleet/nft/ss_guard.nft"' >> /etc/nftables.conf
systemctl enable -q nftables 2>/dev/null || true
for i in $(seq 1 30); do ss -Hltn "sport = :$SS_PORT" | grep -q . && break; sleep 0.2; done
ss -Hltn "sport = :$SS_PORT" | grep -q . && echo ss=listening || echo ss=down
echo "nft_relays=$(nft list set inet ss_guard relays | tr -d '\n' | sed 's/.*elements = { \(.*\) }.*/\1/' | tr -d ' \t')"
"#;

    const LOCK: &str = "exec 9>/run/fleet-ss.lock; flock 9\n";
    /// 进程跑的不是磁盘上当前的 ssserver 才重启（程序没换过时不重启）
    const STALE: &str = "# 磁盘上的 ssserver 已换成新版（同机别的实例收敛时推的）、这个实例还在跑旧的：也重启，换上新程序；程序没换过时不重启\n\
                         [ /proc/$(systemctl show -p MainPID --value fleet-ssserver)/exe -ef /usr/local/bin/ssserver ] || RESTART=1\n";

    /// 宿主机（普通落地机）除了多一行锁和一行「进程跑的是旧程序才重启」，和改动前逐字相同：生产节点升级中控后重新收敛，配置、单元、nft、include 行都不变，
    /// 程序没换过也不重启 ssserver
    #[test]
    fn host_render_is_unchanged() {
        let s = node_ss(&SsInst::host());
        assert!(s.starts_with(&format!("\nset -eu\n{LOCK}")), "{s}");
        assert!(s.contains(STALE), "{s}");
        assert_eq!(s.replacen(LOCK, "", 1).replacen(STALE, "", 1), OLD_NODE_SS);
        assert_eq!((SsInst::host().svc(), SsInst::host().table()), ("fleet-ssserver".to_string(), "ss_guard".to_string()));
    }

    #[test]
    fn ip_node_render_uses_own_instance() {
        let i = SsInst { port: 8389, ip: Some("203.0.113.7".into()) };
        let s = node_ss(&i);
        assert!(!s.contains("__"), "占位符都已替换");
        assert!(s.contains(
            r#"printf '{"server":"203.0.113.7","server_port":%s,"password":"%s","method":"%s","mode":"tcp_and_udp","timeout":300,"outbound_bind_addr":"203.0.113.7"}\n'"#
        ));
        assert!(s.contains("C=/etc/shadowsocks-rust/8389.json\n"));
        assert!(s.contains("U=/etc/systemd/system/fleet-ssserver@.service\n") && s.contains("ExecStart=/usr/local/bin/ssserver -c /etc/shadowsocks-rust/%i.json\n"));
        assert!(s.contains("systemctl enable -q fleet-ssserver@8389\n") && s.contains("then systemctl restart fleet-ssserver@8389; fi"));
        assert!(s.contains("table inet ss_guard_8389 {") && s.contains("nft list set inet ss_guard_8389 relays"));
        assert!(s.contains(r#"echo 'include "/etc/fleet/nft/ss_guard_8389.nft"' >> /etc/nftables.conf"#));
        // 宿主机自己的配置、单元、防火墙一个都不碰
        for host in ["config.json", "fleet-ssserver.service", "fleet-ssserver\n", "ss_guard.nft", "inet ss_guard "] {
            assert!(!s.contains(host), "{host}");
        }
        assert_eq!((i.svc(), i.table()), ("fleet-ssserver@8389".to_string(), "ss_guard_8389".to_string()));
    }

    #[test]
    fn ip_node_cleanup_leaves_host_alone() {
        let s = super::NODE_IP_CLEANUP;
        assert!(s.contains(r#"systemctl disable --now "fleet-ssserver@$SS_PORT""#) && s.contains(r#"nft delete table inet "ss_guard_$SS_PORT""#));
        assert!(s.contains(r#"rm -f "/etc/shadowsocks-rust/$SS_PORT.json""#) && s.contains(r##"ss_guard_$SS_PORT.nft\"#d"##));
        for host in ["fleet-agent", "config.json", "ss_guard.nft", "inet ss_guard ", "fleet-ssserver.service", "fleet-ssserver@.service", "/usr/local/bin", "userdel", "rm -rf"] {
            assert!(!s.contains(host), "{host}");
        }
        // 宿主机退役的清理连同实例一起删：按配置文件停各实例、删模板单元和 ss_guard_* 表，include 行按前缀删
        let c = super::NODE_CLEANUP;
        assert!(c.contains("/etc/shadowsocks-rust/[0-9]*.json") && c.contains(r#"systemctl disable --now "fleet-ssserver@$p""#));
        assert!(c.contains("/etc/systemd/system/fleet-ssserver@.service") && c.contains("/etc/fleet/nft/ss_guard_*.nft"));
        assert!(c.contains(r#"sed -i '\#include "/etc/fleet/nft/ss_guard#d'"#));
    }

    /// set -e 下 `a && b && c` 中间失败不会退出：防火墙脚本里不许再出现这种写法
    #[test]
    fn nft_failures_are_explicit() {
        for s in [super::NODE_SS, super::NODE_NFT_ONLY] {
            assert!(!s.contains("&& nft"), "nft 命令不能放在 && 链里");
            assert!(s.contains("nft -c -f $N.tmp || {"));
            assert!(s.contains("nft -f $N || {"));
        }
        assert!(super::NODE_NFT_ONLY.contains("[^}]*"), "替换放行名单的 sed 不能吃掉集合的右花括号");
    }

    #[test]
    fn relay_xray_installs_logrotate() {
        let s = super::relay_xray();
        assert!(!s.contains("__"), "占位符都已替换");
        assert!(s.contains("/var/log/fleet-xray/access.log {") && s.contains("copytruncate") && s.contains("    maxsize 200M\n"));
        assert!(s.contains("install -d -m 755 /etc/logrotate.d\n") && s.contains("mv $L.tmp $L"));
        // 每小时只跑这一份配置的定时器：单元变了才 daemon-reload
        assert!(s.contains("ExecStart=/usr/sbin/logrotate /etc/logrotate.d/fleet-xray\n") && s.contains("OnCalendar=*-*-* 01..23:00:00\n"));
        assert!(s.contains("[ $LR = 0 ] || systemctl daemon-reload\nsystemctl enable -q --now fleet-xray-logrotate.timer\n"));
        assert!(!s.contains("logrotate=error"), "上架时装不上要让步骤失败");
        assert!(super::BASELINE.contains(r#"command -v logrotate >/dev/null || need="$need logrotate""#));
        let c = super::RELAY_CLEANUP;
        assert!(c.contains("/etc/logrotate.d/fleet-xray") && c.contains("systemctl disable --now fleet-xray-logrotate.timer"));
        assert!(c.contains("/etc/systemd/system/fleet-xray-logrotate.service /etc/systemd/system/fleet-xray-logrotate.timer"));
    }

    #[test]
    fn agent_logrotate_failure_does_not_fail_step() {
        let s = super::relay_logrotate();
        assert!(s.starts_with("(\ninstall -d -m 755 /etc/logrotate.d\n"), "{s}");
        assert!(s.ends_with("\n) || echo logrotate=error\n"), "{s}");
        assert!(!s.contains("__"));
    }
}
