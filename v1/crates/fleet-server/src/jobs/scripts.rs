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

/// 系统基线：sshd 只允许密钥、BBR（容器里可能设不了，只报告不失败）
pub const BASELINE: &str = r#"
set -eu
export DEBIAN_FRONTEND=noninteractive
need=""; command -v nft >/dev/null || need="$need nftables"; command -v curl >/dev/null || need="$need curl"
if [ -n "$need" ]; then timeout 240 apt-get update -qq >/dev/null 2>&1 || true; timeout 300 apt-get install -y -qq $need >/dev/null; fi
install -d -m 755 /etc/ssh/sshd_config.d
grep -qE '^[[:space:]]*Include[[:space:]]+/etc/ssh/sshd_config\.d/\*\.conf' /etc/ssh/sshd_config || sed -i '1i Include /etc/ssh/sshd_config.d/*.conf' /etc/ssh/sshd_config
F=/etc/ssh/sshd_config.d/00-fleet-keyonly.conf
printf '%s\n' '# fleet：只允许密钥登录' 'PasswordAuthentication no' 'KbdInteractiveAuthentication no' 'PermitRootLogin prohibit-password' 'PubkeyAuthentication yes' > $F.tmp
if ! cmp -s $F.tmp $F; then mv $F.tmp $F; sshd -t; systemctl restart ssh 2>/dev/null || systemctl restart sshd; else rm -f $F.tmp; fi
echo "sshd_password=$(sshd -T 2>/dev/null | awk '/^passwordauthentication /{print $2}')"
printf '%s\n' 'net.core.default_qdisc=fq' 'net.ipv4.tcp_congestion_control=bbr' > /etc/sysctl.d/90-fleet-bbr.conf
modprobe tcp_bbr 2>/dev/null || true; sysctl -q -p /etc/sysctl.d/90-fleet-bbr.conf 2>/dev/null || true
echo "cc=$(cat /proc/sys/net/ipv4/tcp_congestion_control)"
"#;

/// 节点：ssserver + 只放行线路机的 nft 规则。变量 SS_PORT SS_METHOD SS_PASSWORD ALLOW（逗号分隔的线路机 IP）。
pub const NODE_SS: &str = r#"
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

/// 只更新节点防火墙放行名单（换线路机、加入口时）。变量 SS_PORT ALLOW。
pub const NODE_NFT_ONLY: &str = r#"
set -eu
N=/etc/fleet/nft/ss_guard.nft
[ -f $N ] || { echo nft=missing; exit 22; }
# 先改临时副本并校验，通过后才替换正式文件（避免留下一份开机时加载会失败的坏文件）
sed "s/elements = { [^}]* }/elements = { $ALLOW }/" $N > $N.tmp || { echo nft=error; exit 21; }
nft -c -f $N.tmp || { rm -f $N.tmp; echo nft=error; exit 21; }
mv -f $N.tmp $N || { echo nft=error; exit 21; }
nft -f $N || { echo nft=error; exit 21; }
echo "nft_relays=$(nft list set inet ss_guard relays | tr -d '\n' | sed 's/.*elements = { \(.*\) }.*/\1/' | tr -d ' \t')"
"#;

/// 线路机：fleet-xray 单元与访问日志轮转（配置由 agent 生成）。xray 二进制与 geoip.dat 已推到 /usr/local/bin、/usr/local/share/xray。
pub fn relay_xray() -> String { RELAY_XRAY.replace("__UNIT__", XRAY_UNIT.trim_end()).replace("__LOGROTATE_SH__", relay_logrotate().trim()) }

/// 线路机访问日志轮转：上架时装，升级 agent 时也装一遍（较早上架、还没有这份配置的线路机升级一次就补上）
pub fn relay_logrotate() -> String { LOGROTATE_SH.replace("__LOGROTATE__", XRAY_LOGROTATE.trim_end()) }

const LOGROTATE_SH: &str = r#"
L=/etc/logrotate.d/fleet-xray
cat > $L.tmp <<'LOGROTATE'
__LOGROTATE__
LOGROTATE
cmp -s $L.tmp $L && rm -f $L.tmp || mv $L.tmp $L
"#;

/// systemd 单元、logrotate 配置：唯一来源在 deploy/
pub const AGENT_UNIT: &str = include_str!("../../../../deploy/units/fleet-agent.service");
pub const XRAY_UNIT: &str = include_str!("../../../../deploy/units/fleet-xray.service");
pub const XRAY_LOGROTATE: &str = include_str!("../../../../deploy/logrotate/fleet-xray");

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
{
  echo '# 由 fleet-server 生成'
  echo "server_url = \"$SERVER_URL\""
  echo 'token_file = "/etc/fleet-agent/token"'
  echo "role = \"$ROLE\""
  echo "machine_id = \"$MACHINE_ID\""
  echo 'data_dir = "/var/lib/fleet-agent"'
  if [ "$ROLE" = relay ]; then
    echo 'xray_bin = "/usr/local/bin/xray"'
    echo 'xray_config = "/etc/fleet-agent/xray.json"'
    echo 'xray_api = "127.0.0.1:62789"'
    echo 'xray_service = "fleet-xray.service"'
    echo 'access_log = "/var/log/fleet-xray/access.log"'
    echo 'xray_asset_dir = "/usr/local/share/xray"'
  fi
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

/// 退役清理（节点）：先停 agent 与 ssserver，删防火墙表和文件。中控随后撤销令牌。
pub const NODE_CLEANUP: &str = r##"
set -u
systemctl disable --now fleet-agent 2>/dev/null; systemctl disable --now fleet-ssserver 2>/dev/null
rm -f /etc/systemd/system/fleet-agent.service /etc/systemd/system/fleet-ssserver.service
rm -rf /etc/fleet-agent /var/lib/fleet-agent /etc/shadowsocks-rust /usr/local/bin/ssserver /usr/local/bin/fleet-agent
nft delete table inet ss_guard 2>/dev/null; rm -f /etc/fleet/nft/ss_guard.nft
[ -f /etc/nftables.conf ] && sed -i '\#include "/etc/fleet/nft/ss_guard.nft"#d' /etc/nftables.conf
userdel fleet-ss 2>/dev/null
systemctl daemon-reload
ss -Hltn 'sport = :8388' | grep -q . && echo ss=still_listening || echo ss=gone
echo cleanup=done
"##;

/// 退役清理（线路机）：停 agent 与 xray，删配置、日志轮转和 xray 二进制与数据文件
pub const RELAY_CLEANUP: &str = r#"
set -u
systemctl disable --now fleet-agent 2>/dev/null; systemctl disable --now fleet-xray 2>/dev/null
rm -f /etc/systemd/system/fleet-agent.service /etc/systemd/system/fleet-xray.service /etc/logrotate.d/fleet-xray
rm -rf /etc/fleet-agent /var/lib/fleet-agent /var/log/fleet-xray /usr/local/bin/fleet-agent /usr/local/bin/xray /usr/local/share/xray
systemctl daemon-reload
echo cleanup=done
"#;

#[cfg(test)]
mod tests {
    #[test]
    fn vars_escapes_quotes() {
        assert_eq!(super::vars(&[("A", "it's")]), "A='it'\\''s'\n");
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
        assert!(s.contains("/var/log/fleet-xray/access.log {") && s.contains("copytruncate"));
        assert!(s.contains("mv $L.tmp $L"));
        assert!(super::RELAY_CLEANUP.contains("/etc/logrotate.d/fleet-xray"));
    }
}
