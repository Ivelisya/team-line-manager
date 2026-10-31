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
# 主机防火墙：ufw / firewalld 在运行时 SS 端口要对线路机放行（收敛时处理，见 HOST_FW）
if command -v ufw >/dev/null 2>&1 && LC_ALL=C ufw status 2>/dev/null </dev/null | grep -qx 'Status: active'; then echo host_fw=ufw
elif command -v firewall-cmd >/dev/null 2>&1 && [ "$(firewall-cmd --state 2>/dev/null </dev/null)" = running ]; then echo host_fw=firewalld
else echo host_fw=none; fi
if ss -Hltn 2>/dev/null | grep -q ':8388 '; then echo port8388=busy; else echo port8388=free; fi
[ -f /etc/fleet-agent/agent.toml ] && echo "agent_machine=$(sed -n 's/^machine_id *= *"\(.*\)"/\1/p' /etc/fleet-agent/agent.toml)" || echo agent_machine=
"#;

/// 只读发行版（上架检查用中控密钥试登录时跑）：`os=` 同 FACTS
pub const OS_RELEASE: &str = r#"
. /etc/os-release 2>/dev/null; echo "os=${ID:-?}-${VERSION_ID:-?}"
"#;

/// 已过标准支持期的发行版：(os-release 的 ID, 显示名, 仍在标准支持期内的最低版本)，低于它的上架时给警告（不拦）。
/// 版本按点分的数字逐段比较。哪个版本过期了就改这里：Ubuntu 22.04 标准支持到 2027-04（之后只有付费的 ESM）；
/// Debian 12 常规安全支持到 2026-06、之后转 LTS（免费、照常有安全更新）到 2028-06。Ubuntu 的非 LTS 版本（24.10、25.04…）只支持 9 个月，表里不管
pub const OS_EOL: &[(&str, &str, &str)] = &[("ubuntu", "Ubuntu", "22.04"), ("debian", "Debian", "12")];

/// `os=` 的值（`ubuntu-20.04`）对照 `OS_EOL`：已过标准支持期时返回警告文字；表里没有的发行版、读不出版本的不管
pub fn os_eol_warning(os: &str) -> Option<String> {
    let (id, ver) = os.rsplit_once('-')?;
    let (_, name, min) = OS_EOL.iter().find(|(i, ..)| *i == id)?;
    let nums = |s: &str| s.split('.').map(|p| p.parse::<u32>().ok()).collect::<Option<Vec<u32>>>();
    if nums(ver)? >= nums(min)? {
        return None;
    }
    Some(format!("系统是 {name} {ver}，已过标准支持期、不再有常规安全更新（{name} {min} 起仍在支持）：可以照常上架，建议找机会重装成新版本"))
}

/// 引导：把中控公钥加进 root 的 authorized_keys（用密码登录时执行）。需要变量 PUBKEY。
pub const BOOTSTRAP_KEY: &str = r#"
set -eu
umask 077; mkdir -p /root/.ssh; touch /root/.ssh/authorized_keys
grep -qxF "$PUBKEY" /root/.ssh/authorized_keys || echo "$PUBKEY" >> /root/.ssh/authorized_keys
chmod 600 /root/.ssh/authorized_keys
echo bootstrap=ok
"#;

/// 系统基线：缺的基础包补上（线路机访问日志轮转要 logrotate，精简系统可能没装）、sshd 只允许密钥。BBR 等内核参数归调优档管（见 tune.rs）。
/// mtr 给 `fleetctl diag` 用（供应商要 mtr 原始数据）：Debian 12/13、Ubuntu 22.04/24.04 的包名都是 `mtr-tiny`（不带图形界面依赖）。
/// 它不是运行必需的，装不上（源坏了、没网）不让基线失败，只输出 `mtr=missing`，步骤记一条 warn
pub const BASELINE: &str = r#"
set -eu
export DEBIAN_FRONTEND=noninteractive
need=""; command -v nft >/dev/null || need="$need nftables"; command -v curl >/dev/null || need="$need curl"
command -v logrotate >/dev/null || need="$need logrotate"
if [ -n "$need" ]; then timeout 240 apt-get update -qq >/dev/null 2>&1 || true; timeout 300 apt-get install -y -qq $need >/dev/null; fi
# 先直接装（包索引多半是新的）；不行再更新索引重试（上面刚更新过就不再更新）。超时放短：基线整体 600 秒，别被它拖垮
if ! command -v mtr >/dev/null; then
  timeout 120 apt-get install -y -qq mtr-tiny >/dev/null 2>&1 </dev/null || { [ -n "$need" ] || timeout 120 apt-get update -qq >/dev/null 2>&1 </dev/null || true; timeout 120 apt-get install -y -qq mtr-tiny >/dev/null 2>&1 </dev/null || true; }
fi
if command -v mtr >/dev/null; then echo mtr=ok; else echo mtr=missing; fi
install -d -m 755 /etc/ssh/sshd_config.d
F=/etc/ssh/sshd_config.d/00-fleet-keyonly.conf
printf '%s\n' '# fleet：只允许密钥登录' 'PasswordAuthentication no' 'KbdInteractiveAuthentication no' 'PermitRootLogin prohibit-password' 'PubkeyAuthentication yes' > $F.tmp
changed=0
if ! cmp -s $F.tmp $F; then mv $F.tmp $F; changed=1; else rm -f $F.tmp; fi
# sshd 同一选项只认第一次出现的值：在主配置第一行单独引入这个文件，供应商镜像里排在它前面的 00-xxx.conf、主配置里的设置都盖不过它
if ! head -n1 /etc/ssh/sshd_config | grep -qxF "Include $F"; then sed -i "1i Include $F" /etc/ssh/sshd_config; changed=1; fi
# sshd 只在启动时读配置：以运行中的为准——它比这两个文件旧（上次换上之后没来得及重启就被打断），这次文件没变也重启，
# 否则下面 sshd -T 读文件报 no、运行中的 sshd 却还允许密码登录。按秒比，同一秒里分不清先后的也重启（多重启一次无妨）
since=$(TZ=UTC0 systemctl show -p ExecMainStartTimestamp --value ssh)
[ -n "$since" ] || since=$(TZ=UTC0 systemctl show -p ExecMainStartTimestamp --value sshd)
[ -z "$since" ] || [ "$(date -d "$since" +%s)" -gt "$(stat -c %Y $F /etc/ssh/sshd_config | sort -n | tail -n1)" ] || changed=1
if [ $changed = 1 ]; then sshd -t; systemctl restart ssh 2>/dev/null || systemctl restart sshd; fi
echo "sshd_password=$(sshd -T 2>/dev/null | awk '/^passwordauthentication /{print $2}')"
"#;

/// 节点上的一个 ssserver 实例。宿主机（普通 VPS 节点）：单元 `fleet-ssserver`、配置 config.json、nft 表 `ss_guard`，监听 0.0.0.0:8388；
/// 同机 IP 节点：模板单元的实例 `fleet-ssserver@<端口>`、配置 `<端口>.json`、nft 表 `ss_guard_<端口>`，监听自己的 bind 地址并从它出站
pub struct SsInst {
    /// 本机监听端口
    pub port: u16,
    /// 同机 IP 节点在宿主机网卡上的地址（bind 地址）：公网 IP 直接配在网卡上时就是它，1:1 NAT 映射的是映射到的本地地址
    /// （`machine.bind_ip`）。线路机连的仍是公网 IP，这里只用于 ssserver 的监听与出站；宿主机为 None
    pub ip: Option<String>,
}

impl SsInst {
    pub fn host() -> Self { SsInst { port: crate::meta::SS_PORT, ip: None } }

    /// 同机 IP 节点的实例：`bind` 是 `machine.bind_ip`（1:1 NAT 映射到的本地地址），为空时公网 IP `public` 就在网卡上、用它
    pub fn ip_node(port: u16, public: &str, bind: Option<&str>) -> Self { SsInst { port, ip: Some(bind.unwrap_or(public).to_string()) } }

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
/// 不会重启 ssserver。同机 IP 节点的 bind 地址直接写进配置（校验过的或 `ip -4 addr` 列出的 IPv4，不含引号和 %）。
/// 重跑时以运行中的实例为准：配置、单元比它新（换上之后没来得及重启就被打断）也重启。输出 `linklocal_dns=1` 见 `LINKLOCAL_DNS`；`ss_restarted=1|0` = 这次重启了 ssserver（配置、ACL、单元有变化，或实例没在跑）
pub fn node_ss(i: &SsInst, public_url: &str) -> String {
    let (conf, unit, unit_conf, listen, bind) = match &i.ip {
        None => ("config.json".to_string(), "fleet-ssserver.service", "config.json", "0.0.0.0".to_string(), String::new()),
        Some(ip) => (format!("{}.json", i.port), "fleet-ssserver@.service", "%i.json", ip.clone(), format!(r#","outbound_bind_addr":"{ip}""#)),
    };
    NODE_SS
        .replace("__OUTBOUND_ACL__", &outbound_acl(public_url))
        .replace("__LINKLOCAL_DNS__", LINKLOCAL_DNS)
        .replace("__PUBLIC_DNS__", PUBLIC_DNS)
        .replace("__UNIT_CONF__", unit_conf)
        .replace("__UNIT__", unit)
        .replace("__CONF__", &conf)
        .replace("__LISTEN__", &listen)
        .replace("__BIND__", &bind)
        .replace("__SVC__", &i.svc())
        .replace("__T__", &i.table())
        .replace("__HOST_FW__", HOST_FW.trim())
}

/// 条件：/etc/resolv.conf 的 nameserver 是链路本地地址（169.254.0.0/16、fe80::/10：GCE、Oracle 等不经 systemd-resolved 的镜像；
/// GCE 上 DNS 和元数据服务是同一个 169.254.169.254）。单元里拦着 link-local，程序就解析不了域名。拦截不能去掉：去掉了，客户经域名
/// （metadata.google.internal、169.254.169.254.nip.io）就能让程序连上元数据服务、读走实例的服务账号令牌。改用公共解析器：
/// ssserver 在配置里加 "dns"（NODE_SS），Xray 的单元把写着公共解析器的 /etc/fleet-xray/resolv.conf 挂到它看到的 /etc/resolv.conf 上
/// （XRAY_UNIT_SH）。脚本输出 `linklocal_dns=1`，步骤记一条 warn
const LINKLOCAL_DNS: &str = r#"grep -qiE '^[[:space:]]*nameserver[[:space:]]+(169\.254\.|fe[89ab][0-9a-f]:)' /etc/resolv.conf 2>/dev/null"#;
/// 链路本地 DNS 时 ssserver 用的公共解析器（shadowsocks-rust 配置的 "dns"：逗号分隔，UDP / TCP 53；1.25 核对过）
const PUBLIC_DNS: &str = "1.1.1.1,8.8.8.8";

/// ssserver 的出站目标黑名单（shadowsocks-rust 1.25 的 acl 文件，配置里 "acl" 指向它）：目标是回环、私网、链路本地、CGNAT 的连接一律拒绝。
/// 不能写 IPv4 映射段 ::ffff:0:0/96：shadowsocks-rust 判 IPv4 目标时还会拿它的映射地址（::ffff:a.b.c.d）去比 IPv6 规则，写了就拦下全部 IPv4 出站
/// （2026-10-25 生产团队1-01 下发后客户流量全被拒）；反过来 ::ffff:10.0.0.1 这类映射地址也会转回 IPv4 比上面的段，不写也绕不过去。
/// 域名目标按解析出的 IP 判（任一解析结果命中就拒绝），所以客户把域名解析到 127.0.0.1 / 10.x 也到不了节点本机服务和供应商内网。
/// 挡的是目标地址，不是源地址：同机 IP 节点的 outbound_bind_addr（含 NAT 的 10.116.x）照常出站。ssserver 查 DNS 走自己的解析器，不受它影响。
/// 解析后到真正连接之间 ssserver 会再解析一次，拦不住秒级 TTL 轮换的 DNS 重绑定
const OUTBOUND_ACL: &str = "[outbound_block_list]
0.0.0.0/8
127.0.0.0/8
10.0.0.0/8
172.16.0.0/12
192.168.0.0/16
169.254.0.0/16
100.64.0.0/10
::/128
::1/128
fc00::/7
fe80::/10";

/// 写到节点上的黑名单：中控地址（public_url 是 IP 字面量）落在哪一段，就不带那一段——出口探测要经节点连中控的 /echo，
/// 实验室中控在 192.168.x 上，拦了就验收不过；ACL 里拦截优先于放行，没法只给中控开个口子。生产的 public_url 是域名，名单完整
fn outbound_acl(public_url: &str) -> String {
    let central = reqwest::Url::parse(public_url).ok().and_then(|u| u.host_str().and_then(|h| h.trim_start_matches('[').trim_end_matches(']').parse::<std::net::IpAddr>().ok()));
    OUTBOUND_ACL
        .lines()
        .filter(|l| !central.is_some_and(|c| l.parse::<ipnetwork::IpNetwork>().is_ok_and(|n| n.contains(c))))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 主机防火墙（NODE_SS、NODE_NFT_ONLY 共用）。供应商镜像自带的 ufw / firewalld 常常只放行旧线路机，SS 端口对新线路机就不通（nft 表放行了也没用：
/// ufw 的链在后面照样丢弃）。ufw / firewalld 在运行时，对 ALLOW 里的每个线路机放行 SS 端口（tcp、udp），名单里已经没有的线路机的规则删掉：
/// ufw 的规则带注释 fleet-ss，只动这个端口上带它的；firewalld 的 rich rule 没有注释，只删这个端口上写法与下面一模一样的。
/// 只增删放行规则，不启停防火墙、不碰 ssserver；规则已有时 ufw / firewalld 不会重复添加。ufw / firewalld 没在运行就不动；
/// 都没有、iptables 的 INPUT 默认（或最后一条）就丢弃时只输出 `host_fw=iptables-drop`，让步骤记警告（不自动改）。整段的 stdin 接 /dev/null：脚本经 `bash -s` 从 stdin 读，命令读了 stdin 会吃掉后面的脚本。
/// 输出 `host_fw=none|ufw|firewalld|iptables-drop`，改规则失败时为 `ufw-error` / `firewalld-error`。变量 SS_PORT ALLOW
macro_rules! host_fw {
    () => {
        r##"
{
# 同一台主机上几个作业可能同时改（同机 IP 节点一起换线、NODE_SS 与 NODE_NFT_ONLY 并发）：ufw / firewalld 改规则不加锁、整份读改写，
# 并发会丢规则。单独一把锁（不能用 NODE_SS 已持有的 fd 9 那把，会自己锁死）
exec 8>/run/fleet-hostfw.lock; flock 8
HOST_FW=none; FWERR=
FW_IPS=$(echo "$ALLOW" | tr ',' ' ')
if command -v ufw >/dev/null 2>&1 && LC_ALL=C ufw status 2>/dev/null | grep -qx 'Status: active'; then
  HOST_FW=ufw
  STALE=$(LC_ALL=C ufw status 2>/dev/null | awk -v port="$SS_PORT" '$2 == "ALLOW" && $NF == "fleet-ss" && ($1 == (port "/tcp") || $1 == (port "/udp")) {split($1, a, "/"); print $3, a[2]}')
  for ip in $FW_IPS; do for p in tcp udp; do
    ufw allow from $ip to any port $SS_PORT proto $p comment fleet-ss >/dev/null 2>&1 || FWERR=1
  done; done
  while read -r ip p; do
    [ -n "$ip" ] || continue
    case " $FW_IPS " in *" $ip "*) continue;; esac
    ufw --force delete allow from $ip to any port $SS_PORT proto $p >/dev/null 2>&1 || FWERR=1
  done <<FWSTALE
$STALE
FWSTALE
elif command -v firewall-cmd >/dev/null 2>&1 && [ "$(firewall-cmd --state 2>/dev/null)" = running ]; then
  HOST_FW=firewalld
  Z=$(firewall-cmd --get-zone-of-interface="$(ip -4 route show default 2>/dev/null | awk '{print $5; exit}')" 2>/dev/null) || Z=
  { [ -z "$Z" ] || [ "$Z" = "no zone" ]; } && { Z=$(firewall-cmd --get-default-zone 2>/dev/null) || FWERR=1; }
  for perm in "" --permanent; do
    STALE=$(firewall-cmd $perm --zone="$Z" --list-rich-rules 2>/dev/null | sed -n 's/^rule family="ipv4" source address="\([0-9.]*\)" port port="'$SS_PORT'" protocol="\(tcp\|udp\)" accept$/\1 \2/p')
    for ip in $FW_IPS; do for p in tcp udp; do
      R="rule family=\"ipv4\" source address=\"$ip\" port port=\"$SS_PORT\" protocol=\"$p\" accept"
      firewall-cmd -q $perm --zone="$Z" --query-rich-rule="$R" || firewall-cmd -q $perm --zone="$Z" --add-rich-rule="$R" >/dev/null 2>&1 || FWERR=1
    done; done
    while read -r ip p; do
      [ -n "$ip" ] || continue
      case " $FW_IPS " in *" $ip "*) continue;; esac
      firewall-cmd -q $perm --zone="$Z" --remove-rich-rule="rule family=\"ipv4\" source address=\"$ip\" port port=\"$SS_PORT\" protocol=\"$p\" accept" >/dev/null 2>&1 || FWERR=1
    done <<FWSTALE
$STALE
FWSTALE
  done
elif command -v iptables >/dev/null 2>&1 && { IPT=$(iptables -S INPUT 2>/dev/null); echo "$IPT" | grep -qE '^-P INPUT (DROP|REJECT)$' || echo "$IPT" | tail -n1 | grep -qE '^-A INPUT -j (DROP|REJECT)'; }; then
  HOST_FW=iptables-drop
fi
echo "host_fw=$HOST_FW${FWERR:+-error}"
} </dev/null
"##
    };
}
const HOST_FW: &str = host_fw!();

const NODE_SS: &str = r#"
set -eu
exec 9>/run/fleet-ss.lock; flock 9
id fleet-ss >/dev/null 2>&1 || useradd --system --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin fleet-ss
install -d -m 750 -g fleet-ss /etc/shadowsocks-rust
C=/etc/shadowsocks-rust/__CONF__
# DNS 是链路本地地址（见 LINKLOCAL_DNS）：ssserver 改用公共解析器，元数据拦截照旧；不是时配置和原来逐字相同
DNS=; if __LINKLOCAL_DNS__; then DNS=',"dns":"__PUBLIC_DNS__"'; echo linklocal_dns=1; fi
# 出站目标黑名单（见 OUTBOUND_ACL）：同机的所有实例共用这一份
A=/etc/shadowsocks-rust/fleet.acl
cat > $A.tmp <<'ACL'
__OUTBOUND_ACL__
ACL
chgrp fleet-ss $A.tmp; chmod 640 $A.tmp
printf '{"server":"__LISTEN__","server_port":%s,"password":"%s","method":"%s","mode":"tcp_and_udp","timeout":300,"acl":"/etc/shadowsocks-rust/fleet.acl"__BIND__%s}\n' "$SS_PORT" "$SS_PASSWORD" "$SS_METHOD" "$DNS" > $C.tmp
chgrp fleet-ss $C.tmp; chmod 640 $C.tmp
RESTART=0
cmp -s $A.tmp $A && rm -f $A.tmp || { mv $A.tmp $A; RESTART=1; }
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
# ssserver 自己解析域名：客户经域名（169.254.169.254.nip.io）也到不了云厂商的元数据服务（回环、私网由配置里的 ACL 拦；这里不拦回环：Ubuntu 上解析要经 127.0.0.53）
IPAddressDeny=link-local 100.100.100.200/32
[Install]
WantedBy=multi-user.target
UNIT
cmp -s $U.tmp $U && rm -f $U.tmp || { mv $U.tmp $U; systemctl daemon-reload; RESTART=1; }
# 单元文件换了、systemd 还没重新加载（上次换上之后没来得及 daemon-reload 就被打断）
[ "$(systemctl show -p NeedDaemonReload --value __SVC__)" != yes ] || { systemctl daemon-reload; RESTART=1; }
# 磁盘上的 ssserver 已换成新版（同机别的实例收敛时推的）、这个实例还在跑旧的：也重启，换上新程序；程序没换过时不重启
[ /proc/$(systemctl show -p MainPID --value __SVC__)/exe -ef /usr/local/bin/ssserver ] || RESTART=1
# 运行中的实例比它的配置或单元旧（上次换上之后没来得及重启就被打断，ssserver 还在用旧密码；同机别的实例收敛时换了模板单元）：也重启。
# 按秒比，同一秒里分不清先后的不算旧（不为它多断一次客户的连接）
since=$(TZ=UTC0 systemctl show -p ExecMainStartTimestamp --value __SVC__)
[ -z "$since" ] || [ "$(date -d "$since" +%s)" -ge "$(stat -c %Y $C $U $A | sort -n | tail -n1)" ] || RESTART=1
systemctl enable -q __SVC__
systemctl -q is-active __SVC__ || RESTART=1
[ $RESTART = 0 ] || systemctl restart __SVC__
echo ss_restarted=$RESTART
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
# 主机防火墙（见 HOST_FW）
__HOST_FW__
for i in $(seq 1 30); do ss -Hltn "sport = :$SS_PORT" | grep -q . && break; sleep 0.2; done
ss -Hltn "sport = :$SS_PORT" | grep -q . && echo ss=listening || echo ss=down
echo "nft_relays=$(nft list set inet __T__ relays | tr -d '\n' | sed 's/.*elements = { \(.*\) }.*/\1/' | tr -d ' \t')"
"#;

/// 只更新节点防火墙放行名单（换线路机、加入口时），连同主机防火墙（`HOST_FW`）。变量 NFT_TABLE（`SsInst::table`）SS_PORT ALLOW。
pub const NODE_NFT_ONLY: &str = concat!(
    r#"
set -eu
N=/etc/fleet/nft/$NFT_TABLE.nft
[ -f $N ] || { echo nft=missing; exit 22; }
# 先改临时副本并校验，通过后才替换正式文件（避免留下一份开机时加载会失败的坏文件）
sed "s/elements = { [^}]* }/elements = { $ALLOW }/" $N > $N.tmp || { echo nft=error; exit 21; }
nft -c -f $N.tmp || { rm -f $N.tmp; echo nft=error; exit 21; }
mv -f $N.tmp $N || { echo nft=error; exit 21; }
nft -f $N || { echo nft=error; exit 21; }
"#,
    host_fw!(),
    r#"echo "nft_relays=$(nft list set inet $NFT_TABLE relays | tr -d '\n' | sed 's/.*elements = { \(.*\) }.*/\1/' | tr -d ' \t')"
"#
);

/// 本机的 IPv4（不含回环），逗号分隔：输出 `local_ips=…` 一行。变量 WANT（逗号分隔的公网 IP）里有不在网卡上的时
/// （1:1 NAT 映射：公网 IP 映射到网卡上的某个内网地址），再让每个本机 IPv4 各自经它访问中控 /echo（变量 ECHO），
/// 输出 `nat=本地地址>回显,…`：回显不是 IPv4 的（docker0 之类不通外网的地址、curl 报错）记为空。各地址同时探测、各 8 秒封顶，
/// 整段最多多花 8 秒多一点；WANT 都在网卡上时不探测、不输出 nat。shell 变量 MISS（有不在网卡上的为 1）、NAT 留给后面的脚本用
macro_rules! host_ips {
    () => {
        r#"
L=$(ip -4 -o addr show | awk '{split($4, a, "/"); if (a[1] !~ /^127\./) print a[1]}')
echo "local_ips=$(echo "$L" | paste -sd, -)"
MISS=; NAT=
for w in $(echo "$WANT" | tr ',' ' '); do echo "$L" | grep -qxF "$w" || MISS=1; done
if [ -n "$MISS" ]; then
  D=$(mktemp -d)
  for l in $L; do curl -4 -sS -m 8 --interface "$l" "$ECHO" 2>/dev/null | head -c 64 | tr -d '\r\n' > "$D/$l" & done
  wait
  NAT=$(for l in $L; do printf '%s>%s\n' "$l" "$(grep -xE '[0-9]{1,3}(\.[0-9]{1,3}){3}' "$D/$l")"; done | paste -sd, -)
  rm -rf "$D"
  echo "nat=$NAT"
fi
"#
    };
}
/// 同上，单独一段：上架「检查」时登录宿主机看同机 IP 节点的 IP 在不在网卡上、不在时是不是 NAT 映射到哪个本机地址上
/// （作业里的 `IP_CHECK` 也是这一段）。变量 WANT（这台宿主机上要检查的全部公网 IP，逗号分隔）ECHO（中控的 /echo 地址）
pub const HOST_IPS: &str = concat!("\nset -u\n", host_ips!());

/// 同机 IP 节点的公网 IP `ip` 在宿主机 `host` 上用哪个本地地址（`local_ips`、`nat` 是 `HOST_IPS` 的输出）：直接配在网卡上 → `Ok(None)`；
/// 不在网卡上、恰好一个本机地址经 NAT 出去是它 → `Ok(Some(这个地址))`（ssserver 监听并从它出站）；都不是 → `Err`（给人看）。
/// 上架检查与作业第一步共用
pub fn bind_addr(ip: &str, host: &str, local_ips: &str, nat: &str) -> Result<Option<String>, String> {
    if local_ips.split(',').any(|x| x.trim() == ip) {
        return Ok(None);
    }
    let pairs: Vec<(&str, &str)> = nat.split(',').filter_map(|p| p.split_once('>')).map(|(l, e)| (l.trim(), e.trim())).filter(|(l, _)| l.parse::<std::net::Ipv4Addr>().is_ok()).collect();
    let hits: Vec<&str> = pairs.iter().filter(|(_, e)| *e == ip).map(|(l, _)| *l).collect();
    match hits[..] {
        [l] => Ok(Some(l.to_string())),
        [] => {
            let seen = pairs.iter().map(|(l, e)| format!("{l} → {}", if e.is_empty() { "不通" } else { e })).collect::<Vec<_>>().join("、");
            Err(format!(
                "IP {ip} 不在宿主机 {host} 的网卡上（本机：{}），也没有哪个本机地址经 NAT 出去是这个 IP（{}）：\
                 只支持直接配在网卡上、或 1:1 NAT 映射到网卡上某个地址（经它出站也是这个 IP）的公网 IPv4",
                if local_ips.is_empty() { "无".to_string() } else { local_ips.replace(',', ", ") },
                if seen.is_empty() { "没探测到结果".to_string() } else { format!("各本机地址出去是：{seen}") },
            ))
        }
        // 和上一条一样以「IP 」开头：网页上架表单（provision-model.ts）按它把错误归到 IP 一栏
        _ => Err(format!("IP {ip} 不在宿主机 {host} 的网卡上，有几个本机地址经 NAT 出去都是它（{}）：分不清 ssserver 该绑哪个，先理顺宿主机的策略路由", hits.join(", "))),
    }
}

/// 同机 IP 节点上架前在宿主机上核对：本机的 IPv4（公网 IP 不在网卡上时连同各本机地址经 NAT 出去的 IP，见 `HOST_IPS`）、
/// 经 bind 地址访问中控 /echo 看到的来源、正在监听的端口（TCP + UDP），以及配置里本来就绑着 bind 地址的实例端口
/// （这台节点上次退役没清理掉的旧实例：恢复时收敛会覆盖它，不算被别人占用）。bind 地址：公网 IP 在网卡上就是它自己；
/// 不在时是经 NAT 出去是它的那个本机地址（有几个取第一个，中控按 `bind_addr` 报错），探测时已经回显过，不再访问一次
/// （总时长不超过原来只访问一次的 10 秒）。变量 IP（节点自己的公网 IP）ECHO（中控的 /echo 地址）
pub const IP_CHECK: &str = concat!(
    "\nset -u\nWANT=$IP\n",
    host_ips!(),
    r#"if [ -z "$MISS" ]; then B=$IP; echo "echo=$(curl -4 -sS -m 10 --interface "$IP" "$ECHO" 2>&1 | tr -d '\r\n' | head -c 200)"
else B=$(echo "$NAT" | tr ',' '\n' | awk -F'>' -v ip="$IP" '$2 == ip {print $1; exit}'); echo "echo=${B:+$IP}"; fi
echo "busy=$(ss -Hltun | awk '{n = split($5, a, ":"); print a[n]}' | sort -un | paste -sd, -)"
echo "own=$(grep -lF "\"server\":\"${B:-$IP}\"" /etc/shadowsocks-rust/[0-9]*.json 2>/dev/null | sed 's#.*/##; s#\.json$##' | paste -sd, -)"
"#
);

/// 线路机：fleet-xray 单元与访问日志轮转（配置由 agent 生成）。xray 二进制与 geoip.dat 已推到 /usr/local/bin、/usr/local/share/xray。
pub fn relay_xray() -> String { RELAY_XRAY.replace("__XRAY_UNIT_SH__", xray_unit_sh().trim()).replace("__LOGROTATE_SH__", logrotate_sh().trim()) }

/// 装 agent（上架、升级）时也收敛一遍 fleet-xray 单元（加固项随版本变，较早上架的线路机升级一次就换上）。单元变了只 daemon-reload、
/// 一般不重启 Xray：新单元等它下次重启（应用时骨架变了、进程退出被拉起、重启机器）才生效。唯一的例外见 `XRAY_RESOLV_SH`。
/// 只是补装：包在子 shell 里，失败只输出 `xray_unit=error`，不让 agent 步骤失败
pub fn relay_xray_unit() -> String { format!("(\n{}\n) && (\n{}\n) || echo xray_unit=error\n", xray_unit_sh().trim(), XRAY_RESOLV_SH.trim()) }

/// 线路机装 agent：先收敛 Xray 单元（需要时重启 Xray，见 `XRAY_RESOLV_SH`）、AnyTLS 外壳的单元（`relay_singbox`）和日志轮转，再换 agent——
/// 新 agent 起来时 Xray 已经用上公共解析器，它的第一次应用（骨架没变，只热更新、不重启 Xray）解析得了域名；
/// /etc/fleet-singbox 也已经建好（agent 单元把它列进 ReadWritePaths，启动时不存在就写不进去）
pub fn relay_agent() -> String { format!("umask 022\n{}{}{}{}", relay_xray_unit(), relay_singbox(), relay_logrotate(), agent()) }

/// 线路机的 AnyTLS 外壳（sing-box，配置由 agent 生成，见 fleet-agent 的 singbox.rs）：建 /etc/fleet-singbox（0700），
/// 程序在（/usr/local/bin/sing-box，中控 dist 里有才推）时收敛 fleet-singbox 单元并 enable。单元变了只 daemon-reload（同 fleet-xray，不为换单元重启）；
/// 程序换了（变量 SINGBOX_CHANGED=1）且正在跑才重启一次换上新程序，输出 `singbox_restart=1`（只断开这台线路机上的 AnyTLS 连接，Reality 不受影响；失败为 `error`）。
/// 输出 `singbox_version=`；程序不在时 `singbox=missing`。上架、升级 agent 都用这一段，只是补装：包在子 shell 里，失败只输出 `singbox_unit=error`，不让步骤失败
pub fn relay_singbox() -> String { format!("(\n{}\n) || echo singbox_unit=error\n", SINGBOX_SH.replace("__UNIT__", SINGBOX_UNIT.trim_end()).trim()) }

const SINGBOX_SH: &str = r#"
install -d -m 700 /etc/fleet-singbox || exit 1
if [ -x /usr/local/bin/sing-box ]; then
S=/etc/systemd/system/fleet-singbox.service
cat > $S.tmp <<'UNIT' || exit 1
__UNIT__
UNIT
cmp -s $S.tmp $S && rm -f $S.tmp || { mv $S.tmp $S || exit 1; systemctl daemon-reload; }
systemctl enable -q fleet-singbox || exit 1
if [ "${SINGBOX_CHANGED:-0}" = 1 ] && systemctl -q is-active fleet-singbox; then
if systemctl restart fleet-singbox; then echo singbox_restart=1; else echo singbox_restart=error; fi
fi
echo "singbox_version=$(/usr/local/bin/sing-box version 2>/dev/null | head -1 | awk '{print $3}')"
else echo singbox=missing; fi
"#;

/// DNS 是链路本地地址（装单元时写了 /etc/fleet-xray/resolv.conf）、正在跑的 Xray 看到的 /etc/resolv.conf 却不是它（单元刚换上这个挂载，
/// 或换上之后 Xray 还没重启过）：重启一次 Xray，挂载才生效——不然 Xray 还用着被 IPAddressDeny=link-local 拦掉的解析器，解析不了域名，
/// 而新 agent 的配置与旧的骨架相同，应用时不会替它重启。重启会断开这台线路机上所有客户的连接，只在这种情况下做，输出 `xray_restart=resolv`
/// （重启失败为 `error`）。经 /proc/<主进程>/root 看的是 Xray 自己挂载命名空间里的文件，重启过就一致了，重跑不再重启；
/// Xray 没在跑时不动（agent 拉起它时挂载就生效）。只在单元装好之后跑，自身不会失败
const XRAY_RESOLV_SH: &str = r#"
pid=$(systemctl show -p MainPID --value fleet-xray)
if [ -f /etc/fleet-xray/resolv.conf ] && [ "${pid:-0}" != 0 ] && ! cmp -s /etc/fleet-xray/resolv.conf /proc/$pid/root/etc/resolv.conf; then
if systemctl restart fleet-xray; then echo xray_restart=resolv; else echo xray_restart=error; fi
fi
"#;

fn xray_unit_sh() -> String { XRAY_UNIT_SH.replace("__LINKLOCAL_DNS__", LINKLOCAL_DNS).replace("__UNIT__", XRAY_UNIT.trim_end()) }

/// fleet-xray 单元：内容变了才换，只 daemon-reload，从不重启 Xray（重启会断开所有连接）。
/// 写、换每一步都查结果：升级时这段包在 `( … ) || echo …` 里，set -e 不起作用，磁盘满时写出来的空文件会被当成新单元换上。
/// DNS 是链路本地地址时（见 `LINKLOCAL_DNS`；元数据拦截照旧）写 /etc/fleet-xray/resolv.conf（单元把它只读挂到 Xray 的 /etc/resolv.conf 上）、
/// 输出 `linklocal_dns=1`；不是时删掉它。同样不重启：Xray 下次重启时才换解析器
const XRAY_UNIT_SH: &str = r#"
U=/etc/systemd/system/fleet-xray.service
cat > $U.tmp <<'UNIT' || exit 1
__UNIT__
UNIT
# glibc 与 Go 的解析都只用前三条 nameserver：两条 IPv4、一条 IPv6（只有 IPv6 的机器也能解析）
R=/etc/fleet-xray/resolv.conf
if __LINKLOCAL_DNS__; then
echo linklocal_dns=1
install -d -m 755 /etc/fleet-xray || exit 1
printf 'nameserver %s\n' 1.1.1.1 8.8.8.8 2606:4700:4700::1111 > $R.tmp && chmod 644 $R.tmp || exit 1
cmp -s $R.tmp $R && rm -f $R.tmp || mv $R.tmp $R || exit 1
else rm -f $R; fi
cmp -s $U.tmp $U && rm -f $U.tmp || { mv $U.tmp $U || exit 1; systemctl daemon-reload; }
"#;

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
/// 系统 logrotate 每天只跑一次：另装一个每小时只跑这一份配置的定时器，maxsize 才起作用。
/// 写、换每一步都查结果（同 XRAY_UNIT_SH：升级时包在 `( … ) || echo …` 里，set -e 不起作用）
const LOGROTATE_SH: &str = r#"
install -d -m 755 /etc/logrotate.d || exit 1
L=/etc/logrotate.d/fleet-xray
cat > $L.tmp <<'LOGROTATE' || exit 1
__LOGROTATE__
LOGROTATE
cmp -s $L.tmp $L && rm -f $L.tmp || mv $L.tmp $L || exit 1
LR=0
T=/etc/systemd/system/fleet-xray-logrotate.service
cat > $T.tmp <<'UNIT' || exit 1
__LR_SERVICE__
UNIT
cmp -s $T.tmp $T && rm -f $T.tmp || { mv $T.tmp $T || exit 1; LR=1; }
T=/etc/systemd/system/fleet-xray-logrotate.timer
cat > $T.tmp <<'UNIT' || exit 1
__LR_TIMER__
UNIT
cmp -s $T.tmp $T && rm -f $T.tmp || { mv $T.tmp $T || exit 1; LR=1; }
[ $LR = 0 ] || systemctl daemon-reload
systemctl enable -q --now fleet-xray-logrotate.timer
"#;

/// systemd 单元、logrotate 配置：唯一来源在 deploy/
pub const AGENT_UNIT: &str = include_str!("../../../../deploy/units/fleet-agent.service");
pub const XRAY_UNIT: &str = include_str!("../../../../deploy/units/fleet-xray.service");
pub const SINGBOX_UNIT: &str = include_str!("../../../../deploy/units/fleet-singbox.service");
pub const XRAY_LOGROTATE: &str = include_str!("../../../../deploy/logrotate/fleet-xray");
pub const XRAY_LOGROTATE_SERVICE: &str = include_str!("../../../../deploy/units/fleet-xray-logrotate.service");
pub const XRAY_LOGROTATE_TIMER: &str = include_str!("../../../../deploy/units/fleet-xray-logrotate.timer");

const RELAY_XRAY: &str = r#"
set -eu
install -d -m 755 /usr/local/share/xray /etc/fleet-agent
__XRAY_UNIT_SH__
__LOGROTATE_SH__
systemctl enable -q fleet-xray
echo "xray_version=$(/usr/local/bin/xray version 2>/dev/null | head -1 | awk '{print $2}')"
"#;

pub const BRIDGE_UNIT: &str = include_str!("../../../../deploy/units/fleet-xray-bridge.service");

/// 读节点上 bridge 现在的配置（base64，没有为空）与运行状态：中控据此决定这次要不要重启 bridge（reverse::only_removed）
pub const BRIDGE_READ: &str = r#"
set -u
C=/etc/fleet-bridge/xray.json
if [ -f $C ]; then echo "config=$(base64 -w0 < $C)"; else echo config=; fi
echo "active=$(systemctl is-active fleet-xray-bridge 2>/dev/null || true)"
"#;

/// 节点上穿透的 bridge（fleet-xray-bridge，配置见 reverse::render）。变量 CONFIG（整份 JSON）、ACTION：
/// - `restart`：写单元与配置，配置先 `xray run -test`，变了（或程序换了：BIN_CHANGED=1、没在跑）就重启——会断开这台节点上全部经隧道的连接；
/// - `quiet`：只删了隧道，写配置不重启（运行中的 bridge 对删掉的入口还会试连，对面端口已关，无害；下次重启按新配置）；
/// - `remove`：节点上不再有穿透入口：停掉、删单元与配置（xray 程序留着，下次开启不用再推）。
/// DNS 是链路本地地址时同线路机写 /etc/fleet-xray/resolv.conf（单元挂到 Xray 看到的 /etc/resolv.conf），变了也要重启才生效。
/// 输出 `bridge=active|removed|crashing|…`、`restarted=0|1`；配置没过 -test 时 `bridge=bad_config` 并以 5 退出（原配置不动）
pub fn bridge() -> String { BRIDGE.replace("__UNIT__", BRIDGE_UNIT.trim_end()).replace("__LINKLOCAL_DNS__", LINKLOCAL_DNS) }

const BRIDGE: &str = r#"
set -eu
umask 022
U=/etc/systemd/system/fleet-xray-bridge.service
C=/etc/fleet-bridge/xray.json
if [ "$ACTION" = remove ]; then
  systemctl disable --now fleet-xray-bridge 2>/dev/null || true
  rm -f $U; rm -rf /etc/fleet-bridge /etc/fleet-xray
  systemctl daemon-reload
  echo restarted=0
  echo bridge=removed
  exit 0
fi
install -d -m 700 /etc/fleet-bridge
RESTART=0
cat > $U.tmp <<'UNIT' || exit 1
__UNIT__
UNIT
cmp -s $U.tmp $U && rm -f $U.tmp || { mv $U.tmp $U || exit 1; systemctl daemon-reload; RESTART=1; }
RC=/etc/fleet-xray/resolv.conf
if __LINKLOCAL_DNS__; then
  echo linklocal_dns=1
  install -d -m 755 /etc/fleet-xray
  printf 'nameserver %s\n' 1.1.1.1 8.8.8.8 2606:4700:4700::1111 > $RC.tmp && chmod 644 $RC.tmp
  cmp -s $RC.tmp $RC && rm -f $RC.tmp || { mv $RC.tmp $RC; RESTART=1; }
elif [ -f $RC ]; then rm -f $RC; RESTART=1; fi
(umask 077; printf '%s
' "$CONFIG" > $C.tmp)
# 临时文件扩展名是 .tmp：xray 按扩展名认格式，不给 -format 就报「Failed to get format」。报错打在标准输出，去掉开头的版本行
if ! /usr/local/bin/xray run -test -format json -c $C.tmp >$C.err 2>&1; then
  echo bridge=bad_config; sed '/^Xray /d;/^A unified platform/d' $C.err >&2; rm -f $C.tmp $C.err; exit 5
fi
rm -f $C.err
if cmp -s $C.tmp $C; then rm -f $C.tmp; else mv $C.tmp $C; [ "$ACTION" = quiet ] || RESTART=1; fi
[ "${BIN_CHANGED:-0}" = 1 ] && RESTART=1
systemctl enable -q fleet-xray-bridge
pid=
if [ $RESTART = 1 ] || ! systemctl -q is-active fleet-xray-bridge; then
  systemctl restart fleet-xray-bridge; pid=$(systemctl show -p MainPID --value fleet-xray-bridge); echo restarted=1
else echo restarted=0; fi
# 刚重启时 is-active 一定是 active：隔 3 秒再看，主进程换了就是起来就退出、被反复拉起（同 agent）
[ -n "$pid" ] && sleep 3
st=$(systemctl is-active fleet-xray-bridge || true)
[ -n "$pid" ] && [ "$st" = active ] && [ "$(systemctl show -p MainPID --value fleet-xray-bridge)" != "$pid" ] && st=crashing
echo "xray_version=$(/usr/local/bin/xray version 2>/dev/null | head -1 | awk '{print $2}')"
echo "bridge=$st"
"#;

/// 装 agent：变量 SERVER_URL ROLE MACHINE_ID，TOKEN（可空：空则沿用节点上已有的令牌文件）。
/// 输出 `agent`（active 才算起来了）、`version`（磁盘上这份程序的版本）、`prev_version`（有 .prev 时它的版本）
pub fn agent() -> String { AGENT.replace("__UNIT__", AGENT_UNIT.trim_end()).replace("__AGENT_STATE__", AGENT_STATE) }

/// 升级失败时换回旧的 agent：.prev 换上去并重启。输出 `agent`、`version`（换回去的版本）；没有 .prev 时 `rollback=no_prev` 并以 6 退出
pub fn agent_rollback() -> String { AGENT_ROLLBACK.replace("__AGENT_STATE__", AGENT_STATE) }

/// 重启 agent 之后（`pid` 是刚重启时的主进程号；没重启过为空）输出 `agent=`。Type=simple 下刚重启时 is-active 一定是 active，
/// 所以重启过的隔 3 秒再看：主进程换了，是起来就退出、被 systemd（RestartSec=3）又拉起了一个，记为 crashing
const AGENT_STATE: &str = r#"[ -n "$pid" ] && sleep 3
st=$(systemctl is-active fleet-agent || true)
[ -n "$pid" ] && [ "$st" = active ] && [ "$(systemctl show -p MainPID --value fleet-agent)" != "$pid" ] && st=crashing
echo "agent=$st""#;

const AGENT_ROLLBACK: &str = r#"
set -eu
B=/usr/local/bin/fleet-agent
[ -f $B.prev ] || { echo rollback=no_prev; exit 6; }
# 换上去的是 .prev 的硬链接，.prev 原样留着；已经换过（连接断了重跑）就不再换：同一个文件的两个名字之间 mv 的结果不可靠
[ $B -ef $B.prev ] || { ln -f $B.prev $B.rollback; mv -f $B.rollback $B; }
systemctl restart fleet-agent; pid=$(systemctl show -p MainPID --value fleet-agent)
__AGENT_STATE__
echo "version=$($B --version 2>/dev/null | awk '{print $2}')"
"#;

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
pid=
if [ $RESTART = 1 ] || ! systemctl -q is-active fleet-agent; then systemctl restart fleet-agent; pid=$(systemctl show -p MainPID --value fleet-agent); fi
__AGENT_STATE__
B=/usr/local/bin/fleet-agent
echo "version=$($B --version 2>/dev/null | awk '{print $2}')"
[ -f $B.prev ] && echo "prev_version=$($B.prev --version 2>/dev/null | awk '{print $2}')"
echo "boot_id=$(cat /proc/sys/kernel/random/boot_id)"
"#;

/// 退役清理（节点）：先停 agent、ssserver（宿主机连同它上面同机 IP 节点的实例）与穿透的 bridge，删防火墙表和文件。中控随后撤销令牌。
pub const NODE_CLEANUP: &str = r##"
set -u
exec 9>/run/fleet-ss.lock; flock 9
systemctl disable --now fleet-agent 2>/dev/null; systemctl disable --now fleet-ssserver 2>/dev/null
systemctl disable --now fleet-xray-bridge 2>/dev/null
rm -f /etc/systemd/system/fleet-xray-bridge.service; rm -rf /etc/fleet-bridge /etc/fleet-xray /usr/local/bin/xray
for f in /etc/shadowsocks-rust/[0-9]*.json; do
  [ -e "$f" ] || continue
  p=$(basename "$f" .json)
  systemctl disable --now "fleet-ssserver@$p" 2>/dev/null; nft delete table inet "ss_guard_$p" 2>/dev/null
done
rm -f /etc/systemd/system/fleet-agent.service /etc/systemd/system/fleet-ssserver.service /etc/systemd/system/fleet-ssserver@.service
rm -rf /etc/fleet-agent /var/lib/fleet-agent /etc/shadowsocks-rust /usr/local/bin/ssserver /usr/local/bin/fleet-agent /usr/local/bin/fleet-agent.prev
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

/// 退役清理（线路机）：停 agent、xray 与 sing-box，删配置、日志轮转（含每小时的定时器）和 xray、sing-box 的二进制与数据文件
pub const RELAY_CLEANUP: &str = r#"
set -u
systemctl disable --now fleet-agent 2>/dev/null; systemctl disable --now fleet-xray 2>/dev/null; systemctl disable --now fleet-xray-logrotate.timer 2>/dev/null
systemctl disable --now fleet-singbox 2>/dev/null; rm -f /etc/systemd/system/fleet-singbox.service; rm -rf /etc/fleet-singbox /usr/local/bin/sing-box
rm -f /etc/systemd/system/fleet-agent.service /etc/systemd/system/fleet-xray.service /etc/logrotate.d/fleet-xray
rm -f /etc/systemd/system/fleet-xray-logrotate.service /etc/systemd/system/fleet-xray-logrotate.timer
rm -rf /etc/fleet-agent /etc/fleet-xray /var/lib/fleet-agent /var/log/fleet-xray /var/log/private/fleet-xray /usr/local/bin/fleet-agent /usr/local/bin/fleet-agent.prev /usr/local/bin/xray /usr/local/share/xray
systemctl daemon-reload
echo cleanup=done
"#;

#[cfg(test)]
mod tests {
    use super::{node_ss, SsInst};

    /// 生产形态的中控地址（域名：黑名单完整）
    const URL: &str = "https://fleet.example.com";

    #[test]
    fn vars_escapes_quotes() {
        assert_eq!(super::vars(&[("A", "it's")]), "A='it'\\''s'\n");
    }

    /// 发行版过期表：低于表里版本的警告（逐段按数字比，20.10 < 22.04 < 22.10），表里没有的、读不出版本的不管
    #[test]
    fn os_eol() {
        use super::os_eol_warning as w;
        let warn = w("ubuntu-20.04").unwrap();
        assert!(warn.starts_with("系统是 Ubuntu 20.04，已过标准支持期") && warn.contains("Ubuntu 22.04 起仍在支持"), "{warn}");
        assert!(w("ubuntu-18.04").is_some() && w("ubuntu-20.10").is_some());
        assert_eq!((w("ubuntu-22.04"), w("ubuntu-22.10"), w("ubuntu-24.04")), (None, None, None));
        assert!(w("debian-11").unwrap().contains("Debian 11"));
        assert!(w("debian-10").is_some() && w("debian-12").is_none() && w("debian-13").is_none());
        // 表里没有的发行版、没有 VERSION_ID（Debian testing）、读不到 os-release
        for os in ["centos-7", "almalinux-9.4", "debian-?", "?-?", "", "ubuntu"] {
            assert_eq!(w(os), None, "{os}");
        }
    }

    /// 宿主机（普通 VPS 节点）的渲染：生产节点升级中控后重新收敛，配置文件路径与写法、nft 表名、include 行、服务名都不能变（变了就会重启 ssserver、断客户的连接）。
    /// 「配置没变就不重启」本身由下面在 bash 里真跑的用例和 tests/upgrade.rs 覆盖
    #[test]
    fn host_render_keeps_names() {
        let s = node_ss(&SsInst::host(), URL);
        for want in [
            "C=/etc/shadowsocks-rust/config.json\n",
            "\"timeout\":300,\"acl\":\"/etc/shadowsocks-rust/fleet.acl\"%s}\\n' \"$SS_PORT\" \"$SS_PASSWORD\" \"$SS_METHOD\" \"$DNS\"",
            "table inet ss_guard {",
            "include \"/etc/fleet/nft/ss_guard.nft\"",
        ] {
            assert!(s.contains(want), "{want}\n{s}");
        }
        assert_eq!((SsInst::host().svc(), SsInst::host().table()), ("fleet-ssserver".to_string(), "ss_guard".to_string()));
    }

    /// 节点上的 ssserver 带出站黑名单：配置指向 acl 文件，文件里回环、私网、链路本地、CGNAT 都在；同机 IP 节点同样（源地址 outbound_bind_addr 不受影响）
    #[test]
    fn ss_config_blocks_private_outbound_targets() {
        for s in [node_ss(&SsInst::host(), URL), node_ss(&SsInst { port: 8389, ip: Some("10.116.0.67".into()) }, URL)] {
            assert!(s.contains("\"acl\":\"/etc/shadowsocks-rust/fleet.acl\""));
            let acl = s.split("<<'ACL'\n").nth(1).unwrap().split("\nACL\n").next().unwrap();
            assert!(acl.starts_with("[outbound_block_list]\n"), "{acl}");
            for net in ["127.0.0.0/8", "10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "169.254.0.0/16", "100.64.0.0/10", "::1/128", "fc00::/7", "fe80::/10"] {
                assert!(acl.lines().any(|l| l == net), "{net}");
            }
            assert!(!acl.lines().any(|l| l.starts_with("::ffff:")), "IPv4 映射段会拦下全部 IPv4 出站：{acl}");
        }
        // 中控在私网（实验室）：去掉它所在的那一段，出口探测才连得上中控的 /echo；别的段照拦
        let s = node_ss(&SsInst::host(), "http://192.168.31.60:8080");
        let acl = s.split("<<'ACL'
").nth(1).unwrap().split("
ACL
").next().unwrap();
        assert!(!acl.lines().any(|l| l == "192.168.0.0/16") && acl.lines().any(|l| l == "10.0.0.0/8"), "{acl}");
    }

    /// /etc/resolv.conf 的 nameserver 是链路本地地址时：ssserver 配置加公共解析器、报 linklocal_dns=1，单元里的元数据拦截一律不动；
    /// 不是时配置和原来逐字相同。拿渲染出来的那两行在 bash 里真跑；线路机的 Xray 单元同样不动拦截
    #[cfg(unix)]
    #[test]
    fn linklocal_dns_keeps_ip_address_deny() {
        let dir = tempfile::tempdir().unwrap();
        let resolv = dir.path().join("resolv.conf");
        let s = node_ss(&SsInst::host(), URL);
        let lines: String = s.lines().filter(|l| l.starts_with("DNS=;") || l.starts_with("printf '{\"server\"")).map(|l| format!("{l}\n")).collect();
        let sh = format!("SS_PORT=8388 SS_PASSWORD=pw SS_METHOD=aes-256-gcm C=$1\n{}", lines.replace("/etc/resolv.conf", &resolv.display().to_string()));
        let conf_out = dir.path().join("c");
        for (conf, linklocal) in [
            ("nameserver 127.0.0.53\noptions edns0\n", false),
            ("# nameserver 169.254.169.254\nnameserver 10.0.0.2\n", false),
            ("search example.internal\nnameserver 169.254.169.254\n", true),
            ("nameserver FE80::1%eth0\n", true),
        ] {
            std::fs::write(&resolv, conf).unwrap();
            let out = std::process::Command::new("bash").arg("-c").arg(&sh).arg("x").arg(&conf_out).output().unwrap();
            assert!(out.status.success(), "{conf}");
            let cfg = std::fs::read_to_string(dir.path().join("c.tmp")).unwrap();
            let want = format!(
                "{{\"server\":\"0.0.0.0\",\"server_port\":8388,\"password\":\"pw\",\"method\":\"aes-256-gcm\",\"mode\":\"tcp_and_udp\",\"timeout\":300,\"acl\":\"/etc/shadowsocks-rust/fleet.acl\"{}}}\n",
                if linklocal { ",\"dns\":\"1.1.1.1,8.8.8.8\"" } else { "" }
            );
            assert_eq!((String::from_utf8_lossy(&out.stdout).contains("linklocal_dns=1"), cfg), (linklocal, want), "{conf}");
        }
        for s in [node_ss(&SsInst::host(), URL), super::xray_unit_sh()] {
            assert!(s.contains("IPAddressDeny=link-local 100.100.100.200/32\n") && !s.contains("/^IPAddressDeny=/d"), "{s}");
        }
    }

    /// 线路机装 Xray 单元：DNS 是链路本地地址时写公共解析器的 resolv.conf（单元把它挂到 Xray 的 /etc/resolv.conf 上）、报 linklocal_dns=1；
    /// 不再是时删掉。整段在 bash 里真跑（路径换到临时目录，systemctl 换成空函数）
    #[cfg(unix)]
    #[test]
    fn linklocal_dns_xray_resolv_conf() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().display().to_string();
        let sh = format!(
            "systemctl() {{ :; }}\n{}",
            super::xray_unit_sh()
                .replace("/etc/systemd/system", &d)
                .replace("/etc/fleet-xray", &format!("{d}/fx"))
                .replace("/etc/resolv.conf", &format!("{d}/resolv.conf"))
        );
        assert!(super::XRAY_UNIT.contains("\nBindReadOnlyPaths=-/etc/fleet-xray/resolv.conf:/etc/resolv.conf\n"));
        let ours = dir.path().join("fx/resolv.conf");
        for (conf, linklocal) in [("nameserver 169.254.169.254\n", true), ("nameserver 169.254.169.254\n", true), ("nameserver 10.0.0.2\n", false)] {
            std::fs::write(dir.path().join("resolv.conf"), conf).unwrap();
            let out = std::process::Command::new("bash").arg("-c").arg(&sh).output().unwrap();
            assert!(out.status.success(), "{out:?}");
            assert_eq!(String::from_utf8_lossy(&out.stdout).contains("linklocal_dns=1"), linklocal);
            let want = linklocal.then(|| "nameserver 1.1.1.1\nnameserver 8.8.8.8\nnameserver 2606:4700:4700::1111\n".to_string());
            assert_eq!(std::fs::read_to_string(&ours).ok(), want, "{conf}");
        }
        assert!(!dir.path().join("fx/resolv.conf.tmp").exists() && dir.path().join("fleet-xray.service").exists());
    }

    /// 重跑系统基线时以运行中的 sshd 为准：比配置文件旧（换上之后没来得及重启）也重启，同一秒里的也算旧
    #[test]
    fn baseline_restarts_sshd_older_than_its_config() {
        let b = super::BASELINE;
        assert!(b.contains("since=$(TZ=UTC0 systemctl show -p ExecMainStartTimestamp --value ssh)\n"));
        assert!(b.contains("[ -z \"$since\" ] || [ \"$(date -d \"$since\" +%s)\" -gt \"$(stat -c %Y $F /etc/ssh/sshd_config | sort -n | tail -n1)\" ] || changed=1\n"));
        let (restart, check) = (b.find("if [ $changed = 1 ]; then sshd -t;").unwrap(), b.find("echo \"sshd_password=").unwrap());
        assert!(b.find("since=").unwrap() < restart && restart < check);
    }

    #[test]
    fn ip_node_render_uses_own_instance() {
        let i = SsInst { port: 8389, ip: Some("203.0.113.7".into()) };
        let s = node_ss(&i, URL);
        assert!(!s.contains("__"), "占位符都已替换");
        assert!(s.contains(
            r#"printf '{"server":"203.0.113.7","server_port":%s,"password":"%s","method":"%s","mode":"tcp_and_udp","timeout":300,"acl":"/etc/shadowsocks-rust/fleet.acl","outbound_bind_addr":"203.0.113.7"%s}\n'"#
        ));
        assert!(s.contains("C=/etc/shadowsocks-rust/8389.json\n"));
        assert!(s.contains("U=/etc/systemd/system/fleet-ssserver@.service\n") && s.contains("ExecStart=/usr/local/bin/ssserver -c /etc/shadowsocks-rust/%i.json\n"));
        assert!(s.contains("systemctl enable -q fleet-ssserver@8389\n") && s.contains("|| systemctl restart fleet-ssserver@8389\n"));
        assert!(s.contains("table inet ss_guard_8389 {") && s.contains("nft list set inet ss_guard_8389 relays"));
        assert!(s.contains(r#"echo 'include "/etc/fleet/nft/ss_guard_8389.nft"' >> /etc/nftables.conf"#));
        // 宿主机自己的配置、单元、防火墙一个都不碰
        for host in ["config.json", "fleet-ssserver.service", "fleet-ssserver\n", "ss_guard.nft", "inet ss_guard "] {
            assert!(!s.contains(host), "{host}");
        }
        assert_eq!((i.svc(), i.table()), ("fleet-ssserver@8389".to_string(), "ss_guard_8389".to_string()));
    }

    /// 1:1 NAT 映射的同机 IP 节点：ssserver 监听并从映射到的本地地址出站，公网 IP 不出现在配置里；没有 bind 地址的照旧用公网 IP
    #[test]
    fn nat_ip_node_render_uses_bind_addr() {
        let i = SsInst::ip_node(8390, "209.240.166.50", Some("10.116.0.67"));
        let s = node_ss(&i, URL);
        assert!(s.contains(
            r#"printf '{"server":"10.116.0.67","server_port":%s,"password":"%s","method":"%s","mode":"tcp_and_udp","timeout":300,"acl":"/etc/shadowsocks-rust/fleet.acl","outbound_bind_addr":"10.116.0.67"%s}\n'"#
        ), "{s}");
        assert!(!s.contains("209.240.166.50") && !s.contains("__"), "{s}");
        // 端口、单元、nft 表都按端口，和 bind 地址无关
        assert_eq!((i.svc(), i.table()), ("fleet-ssserver@8390".to_string(), "ss_guard_8390".to_string()));
        assert!(s.contains("tcp dport $SS_PORT ip saddr @relays accept"));
        let direct = SsInst::ip_node(8389, "203.0.113.7", None);
        assert_eq!(direct.ip.as_deref(), Some("203.0.113.7"));
    }

    /// 公网 IP 在网卡上 → 不用 bind 地址；不在时找经 NAT 出去是它的那一个本机地址；找不到、找到几个都报错
    #[test]
    fn bind_addr_from_nat_probe() {
        use super::bind_addr;
        let (local, nat) = ("10.116.0.66,10.116.0.67,172.17.0.1", "10.116.0.66>209.240.166.49,10.116.0.67>209.240.166.50,172.17.0.1>");
        assert_eq!(bind_addr("10.116.0.67", "H", local, ""), Ok(None), "在网卡上的照旧");
        assert_eq!(bind_addr("209.240.166.50", "H", local, nat), Ok(Some("10.116.0.67".into())));
        let e = bind_addr("209.240.166.51", "H", local, nat).unwrap_err();
        assert!(e.contains("IP 209.240.166.51 不在宿主机 H 的网卡上（本机：10.116.0.66, 10.116.0.67, 172.17.0.1）"), "{e}");
        assert!(e.contains("10.116.0.67 → 209.240.166.50、172.17.0.1 → 不通") && e.contains("1:1 NAT"), "{e}");
        let e = bind_addr("209.240.166.51", "H", "", "").unwrap_err();
        assert!(e.contains("（本机：无）") && e.contains("没探测到结果"), "{e}");
        let e = bind_addr("209.240.166.49", "H", local, "10.116.0.66>209.240.166.49,10.116.0.67>209.240.166.49").unwrap_err();
        assert!(e.contains("10.116.0.66, 10.116.0.67") && e.contains("分不清"), "{e}");
        // 不是 IPv4 的本地地址（输出被截断、混进了别的）不当 bind 地址
        assert!(bind_addr("209.240.166.50", "H", local, "10.116.0.67\"x>209.240.166.50").is_err());
    }

    /// 宿主机上的核对脚本整段在 bash 里真跑（ip、curl、ss 换成函数）：公网 IP 在网卡上时不探测，经它自己访问一次；
    /// 不在时各本机地址各探测一次，回显是它的那个就是 bind 地址，own 按 bind 地址找旧实例，不再访问一次
    #[cfg(unix)]
    #[test]
    fn ip_check_probes_nat_only_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().display().to_string();
        std::fs::write(dir.path().join("8390.json"), r#"{"server":"10.116.0.67","server_port":8390}"#).unwrap();
        let run = |ip: &str| {
            let sh = format!(
                "ip() {{ printf '%s\\n' '1: lo    inet 127.0.0.1/8 scope host lo' '2: eth0    inet 10.116.0.66/24 brd 10.116.0.255 scope global eth0' \
                 '3: ens19    inet 10.116.0.67/32 scope global ens19' '4: docker0    inet 172.17.0.1/16 scope global docker0'; }}\n\
                 curl() {{ while [ $1 != --interface ]; do shift; done; echo \"curl $2\" >> {d}/log; \
                 case $2 in 10.116.0.66) echo 209.240.166.49;; 10.116.0.67) echo 209.240.166.50;; 172.17.0.1) echo 'curl: (28) timeout' >&2; return 28;; *) echo \"curl: (45) bind failed\";; esac; }}\n\
                 ss() {{ printf '%s\\n' 'tcp LISTEN 0 128 0.0.0.0:8388 0.0.0.0:*' 'udp UNCONN 0 0 10.116.0.67:8390 0.0.0.0:*'; }}\n\
                 IP={ip} ECHO=http://c/echo\n{}",
                super::IP_CHECK.replace("/etc/shadowsocks-rust", &d)
            );
            let _ = std::fs::remove_file(dir.path().join("log"));
            let out = std::process::Command::new("bash").arg("-c").arg(&sh).output().unwrap();
            assert!(out.status.success(), "{out:?}");
            let o = crate::ssh::Output { code: 0, stdout: String::from_utf8_lossy(&out.stdout).into_owned(), stderr: String::new() };
            let log = std::fs::read_to_string(dir.path().join("log")).unwrap_or_default();
            (o, log)
        };
        let (o, log) = run("10.116.0.67");
        assert_eq!((o.kv("local_ips"), o.kv("nat"), o.kv("echo"), o.kv("own")), (Some("10.116.0.66,10.116.0.67,172.17.0.1"), None, Some("209.240.166.50"), Some("8390")), "{}", o.stdout);
        assert_eq!(log, "curl 10.116.0.67\n");
        let (o, log) = run("209.240.166.50");
        assert_eq!(o.kv("nat"), Some("10.116.0.66>209.240.166.49,10.116.0.67>209.240.166.50,172.17.0.1>"), "{}", o.stdout);
        assert_eq!((o.kv("echo"), o.kv("busy"), o.kv("own")), (Some("209.240.166.50"), Some("8388,8390"), Some("8390")), "{}", o.stdout);
        assert_eq!(log.lines().count(), 3, "每个本机地址探测一次，不再多访问一次：{log}");
        assert_eq!(super::bind_addr("209.240.166.50", "H", o.kv("local_ips").unwrap(), o.kv("nat").unwrap()), Ok(Some("10.116.0.67".into())));
        // 找不到：echo 为空、own 按公网 IP 找（找不到），中控按 bind_addr 报错
        let (o, _) = run("209.240.166.51");
        assert_eq!((o.kv("echo"), o.kv("own")), (Some(""), Some("")), "{}", o.stdout);
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

    /// 主机防火墙段（HOST_FW）整段在 bash 里真跑（ufw、firewall-cmd、iptables 换成记录调用的函数）：ufw 在运行时对 ALLOW 里每个线路机 IP 放行 SS 端口的 tcp、udp，
    /// 删掉这个端口上带 fleet-ss 注释、IP 已不在名单里的旧规则，别人的规则和别的端口的规则不碰；ufw 没在运行时一条都不改；
    /// firewalld 同理（已有的规则不重复加）；只有 iptables 且 INPUT 默认丢弃时只报告
    #[cfg(unix)]
    #[test]
    fn host_fw_syncs_ufw_and_firewalld_rules() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().display().to_string();
        let run = |fakes: &str| -> (String, Vec<String>) {
            let _ = std::fs::remove_file(dir.path().join("log"));
            let sh = format!("{fakes}\nSS_PORT=8388 ALLOW='192.0.2.1, 192.0.2.2'\nset -eu\n{}", super::HOST_FW.replace("/run/fleet-hostfw.lock", &format!("{d}/lock")));
            let out = std::process::Command::new("bash").arg("-c").arg(&sh).output().unwrap();
            assert!(out.status.success(), "{out:?}");
            let log = std::fs::read_to_string(dir.path().join("log")).unwrap_or_default();
            let o = crate::ssh::Output { code: 0, stdout: String::from_utf8_lossy(&out.stdout).into_owned(), stderr: String::new() };
            (o.kv("host_fw").unwrap_or("").to_string(), log.lines().filter(|l| !l.ends_with(" status")).map(String::from).collect())
        };
        let ufw = |status: &str| {
            std::fs::write(dir.path().join("status"), status).unwrap();
            format!("ufw() {{ echo \"ufw $*\" >> {d}/log; [ \"$1\" != status ] || cat {d}/status; return 0; }}")
        };
        let active = "Status: active\n\nTo                         Action      From\n--                         ------      ----\n\
            22/tcp                     ALLOW       Anywhere\n\
            8388/tcp                   ALLOW       192.0.2.1                  # fleet-ss\n\
            8388/tcp                   ALLOW       198.51.100.9               # fleet-ss\n\
            8388/udp                   ALLOW       198.51.100.9               # fleet-ss\n\
            8388/tcp                   ALLOW       203.0.113.99\n\
            8389/tcp                   ALLOW       198.51.100.8               # fleet-ss\n";
        let (fw, calls) = run(&ufw(active));
        let allow = |ip: &str, p: &str| format!("ufw allow from {ip} to any port 8388 proto {p} comment fleet-ss");
        let del = |ip: &str, p: &str| format!("ufw --force delete allow from {ip} to any port 8388 proto {p}");
        assert_eq!(fw, "ufw");
        assert_eq!(
            calls,
            [allow("192.0.2.1", "tcp"), allow("192.0.2.1", "udp"), allow("192.0.2.2", "tcp"), allow("192.0.2.2", "udp"), del("198.51.100.9", "tcp"), del("198.51.100.9", "udp")],
            "只加名单里的、只删这个端口上带 fleet-ss 且已不在名单里的"
        );
        // ufw 没启用：一条规则都不改
        assert_eq!(run(&ufw("Status: inactive\n")), ("none".to_string(), vec![]));

        // firewalld：已有的规则不重复加，旧线路机的规则删掉，别的端口的不碰；运行时和永久各做一遍
        std::fs::write(
            dir.path().join("rules"),
            "rule family=\"ipv4\" source address=\"192.0.2.1\" port port=\"8388\" protocol=\"tcp\" accept\n\
             rule family=\"ipv4\" source address=\"198.51.100.9\" port port=\"8388\" protocol=\"udp\" accept\n\
             rule family=\"ipv4\" source address=\"198.51.100.8\" port port=\"8389\" protocol=\"tcp\" accept\n",
        )
        .unwrap();
        let fwd = format!(
            "ip() {{ echo 'default via 10.0.0.1 dev eth0 proto dhcp'; }}\n\
             firewall-cmd() {{ echo \"firewall-cmd $*\" >> {d}/log; case \"$*\" in *--state*) echo running;; *--get-zone-of-interface=eth0*) echo public;; \
             *--list-rich-rules*) cat {d}/rules;; *--query-rich-rule=*192.0.2.1*tcp*) return 0;; *--query-rich-rule=*) return 1;; esac; return 0; }}"
        );
        let (fw, calls) = run(&fwd);
        let rule = |ip: &str, p: &str| format!("rule family=\"ipv4\" source address=\"{ip}\" port port=\"8388\" protocol=\"{p}\" accept");
        let add = |perm: &str, ip: &str, p: &str| format!("firewall-cmd -q {perm}--zone=public --add-rich-rule={}", rule(ip, p));
        let rm = |perm: &str| format!("firewall-cmd -q {perm}--zone=public --remove-rich-rule={}", rule("198.51.100.9", "udp"));
        assert_eq!(fw, "firewalld");
        for perm in ["", "--permanent "] {
            assert!(calls.contains(&add(perm, "192.0.2.2", "tcp")) && calls.contains(&add(perm, "192.0.2.1", "udp")) && calls.contains(&rm(perm)), "{perm}{calls:#?}");
            assert!(!calls.contains(&add(perm, "192.0.2.1", "tcp")), "已有的规则不重复加");
        }
        assert_eq!(calls.iter().filter(|c| c.contains("--remove-rich-rule")).count(), 2, "只删旧线路机在这个端口上的规则：{calls:#?}");

        // 没有 ufw / firewalld、iptables 的 INPUT 默认丢弃：只报告
        for (ipt, want) in [("-P INPUT DROP", "iptables-drop"), ("-P INPUT ACCEPT\n-A INPUT -p tcp --dport 22 -j ACCEPT\n-A INPUT -j REJECT --reject-with icmp-host-prohibited", "iptables-drop"), ("-P INPUT ACCEPT", "none")] {
            std::fs::write(dir.path().join("ipt"), format!("{ipt}
")).unwrap();
            let (fw, calls) = run(&format!("iptables() {{ cat {d}/ipt; }}"));
            assert_eq!((fw.as_str(), calls.is_empty()), (want, true), "{ipt}");
        }
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

    /// 基线补装 mtr-tiny（fleetctl diag 用）：装不上不让基线失败（set -e 下每条 apt-get 都有兜底），只报 mtr=missing
    #[test]
    fn baseline_installs_mtr_best_effort() {
        let b = super::BASELINE;
        assert!(b.contains("timeout 120 apt-get install -y -qq mtr-tiny >/dev/null 2>&1 </dev/null || {"));
        assert!(b.contains("timeout 120 apt-get install -y -qq mtr-tiny >/dev/null 2>&1 </dev/null || true; }"));
        assert!(b.contains("if command -v mtr >/dev/null; then echo mtr=ok; else echo mtr=missing; fi"));
        // 在 sshd 那段之前：sshd -t 失败时脚本退出，mtr 的结果也照样报过了
        assert!(b.find("mtr-tiny").unwrap() < b.find("sshd_config.d").unwrap());
    }

    #[test]
    fn relay_xray_installs_logrotate() {
        let s = super::relay_xray();
        assert!(!s.contains("__"), "占位符都已替换");
        assert!(s.contains("/var/log/fleet-xray/access.log {") && s.contains("copytruncate") && s.contains("    maxsize 200M\n"));
        // 写、换都查结果：升级时这段包在 `( … ) || echo` 里，set -e 不起作用
        assert!(s.contains("install -d -m 755 /etc/logrotate.d || exit 1\n") && s.contains("cat > $L.tmp <<'LOGROTATE' || exit 1\n"));
        assert!(s.contains("cmp -s $L.tmp $L && rm -f $L.tmp || mv $L.tmp $L || exit 1\n") && s.contains("{ mv $T.tmp $T || exit 1; LR=1; }"));
        // 每小时只跑这一份配置的定时器：单元变了才 daemon-reload
        assert!(s.contains("ExecStart=/usr/sbin/logrotate /etc/logrotate.d/fleet-xray\n") && s.contains("OnCalendar=*-*-* 01..23:00:00\n"));
        assert!(s.contains("[ $LR = 0 ] || systemctl daemon-reload\nsystemctl enable -q --now fleet-xray-logrotate.timer\n"));
        assert!(!s.contains("logrotate=error"), "上架时装不上要让步骤失败");
        assert!(super::BASELINE.contains(r#"command -v logrotate >/dev/null || need="$need logrotate""#));
        let c = super::RELAY_CLEANUP;
        assert!(c.contains("/etc/logrotate.d/fleet-xray") && c.contains("systemctl disable --now fleet-xray-logrotate.timer"));
        assert!(c.contains("/etc/systemd/system/fleet-xray-logrotate.service /etc/systemd/system/fleet-xray-logrotate.timer"));
        // 动态用户的日志目录在 /var/log/private 下，/var/log/fleet-xray 只是符号链接
        assert!(c.contains(" /var/log/fleet-xray /var/log/private/fleet-xray "));
    }

    /// 升级时留下的旧 agent（.prev）退役清理时一起删
    #[test]
    fn cleanup_removes_previous_agent() {
        for s in [super::NODE_CLEANUP, super::RELAY_CLEANUP] {
            assert!(s.contains(" /usr/local/bin/fleet-agent /usr/local/bin/fleet-agent.prev"), "{s}");
        }
        for s in [super::agent(), super::agent_rollback()] {
            assert!(!s.contains("__") && s.contains("\necho \"agent=$st\"\n"), "{s}");
        }
    }

    #[test]
    fn agent_logrotate_failure_does_not_fail_step() {
        let s = super::relay_logrotate();
        assert!(s.starts_with("(\ninstall -d -m 755 /etc/logrotate.d || exit 1\n"), "{s}");
        assert!(s.ends_with("\n) || echo logrotate=error\n"), "{s}");
        assert!(!s.contains("__"));
    }

    /// 升级 agent 时收敛 fleet-xray 单元：与上架装的同一段，单元变了只 daemon-reload；只有链路本地 DNS 的挂载没生效时重启 Xray
    /// （见 linklocal_dns_restarts_xray_once）；失败不让步骤失败
    #[test]
    fn agent_upgrade_converges_xray_unit_without_restart() {
        let s = super::relay_xray_unit();
        let body = super::xray_unit_sh();
        assert_eq!(s, format!("(\n{}\n) && (\n{}\n) || echo xray_unit=error\n", body.trim(), super::XRAY_RESOLV_SH.trim()));
        assert_eq!(s.matches("systemctl restart").count(), 1);
        // Xray 以动态用户运行、配置经标准输入交给它，连不上本地套接字、到不了元数据服务
        assert!(body.contains("DynamicUser=yes\nStandardInput=file:/etc/fleet-agent/xray.json\nExecStart=/usr/local/bin/xray run -c stdin:\n"), "{body}");
        assert!(body.contains("\nRestrictAddressFamilies=AF_INET AF_INET6 AF_NETLINK\n") && body.contains("\nIPAddressDeny=link-local 100.100.100.200/32\n"));
        assert!(body.contains("InaccessiblePaths=-/etc/fleet-agent -/var/lib/fleet-agent -/etc/ssh\n"), "{body}");
        assert!(!body.contains("\nUser=") && !body.contains("\nTemporaryFileSystem="));
        // agent 要读动态用户写的 0600 访问日志
        assert!(super::AGENT_UNIT.contains("\nCapabilityBoundingSet=CAP_DAC_READ_SEARCH\n"));
        // 写、换都查结果：包在 `( … ) || echo` 里 set -e 不起作用，写失败的空文件不能被当成新单元换上
        assert!(body.contains("cat > $U.tmp <<'UNIT' || exit 1\n"));
        assert!(body.contains("cmp -s $U.tmp $U && rm -f $U.tmp || { mv $U.tmp $U || exit 1; systemctl daemon-reload; }\n"));
        assert!(super::relay_xray().contains(body.trim()) && !s.contains("__"), "{s}");
        let s = super::relay_xray();
        assert!(!s.contains("systemctl restart") && !s.contains("__"), "{s}");
    }

    /// 线路机装 agent：先装 Xray 单元、日志轮转，后换 agent（新 agent 起来时 Xray 已经用上公共解析器）
    #[test]
    fn relay_agent_installs_xray_unit_before_agent() {
        let s = super::relay_agent();
        let (unit, lr, agent) = (s.find(&super::relay_xray_unit()).unwrap(), s.find(&super::relay_logrotate()).unwrap(), s.find(&super::agent()).unwrap());
        assert!(unit < lr && lr < agent && s.contains("systemctl restart fleet-agent"), "{s}");
        // AnyTLS 外壳：/etc/fleet-singbox 在换 agent 之前建好（agent 单元的 ReadWritePaths 要它在）；失败不让步骤失败
        let sb = super::relay_singbox();
        assert!(s.find(&sb).unwrap() < agent && sb.ends_with("\n) || echo singbox_unit=error\n") && !sb.contains("__"), "{sb}");
        assert!(sb.starts_with("(\ninstall -d -m 700 /etc/fleet-singbox || exit 1\n"), "{sb}");
        assert!(sb.contains("DynamicUser=yes\nStandardInput=file:/etc/fleet-singbox/config.json\nExecStart=/usr/local/bin/sing-box run -c stdin --disable-color\n"), "{sb}");
        assert!(super::AGENT_UNIT.contains("\nReadWritePaths=/var/lib/fleet-agent /etc/fleet-agent -/etc/fleet-singbox\n"));
        assert!(super::RELAY_CLEANUP.contains("systemctl disable --now fleet-singbox") && super::RELAY_CLEANUP.contains("/usr/local/bin/sing-box"));
    }

    /// 链路本地 DNS：运行中的 Xray 看到的 /etc/resolv.conf 不是公共解析器那份时重启一次 Xray；重启过（看到的一致了）、
    /// 不是链路本地 DNS、Xray 没在跑、单元没装上时都不重启。整段在 bash 里真跑（路径换到临时目录，Xray 看到的那份换成临时文件，systemctl 换成函数）
    #[cfg(unix)]
    #[test]
    fn linklocal_dns_restarts_xray_once() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().display().to_string();
        let run = |resolv: &str, pid: &str, sees: &str, unit_ok: bool| {
            std::fs::write(dir.path().join("resolv.conf"), resolv).unwrap();
            std::fs::write(dir.path().join("sees.conf"), sees).unwrap();
            let sh = format!(
                "systemctl() {{ case $1 in show) echo {pid};; restart) echo \"restart $2\" >> {d}/log;; esac; }}\n{}{}",
                if unit_ok { "" } else { "cat() { return 1; }\n" },
                super::relay_xray_unit()
                    .replace("/proc/$pid/root/etc/resolv.conf", &format!("{d}/sees.conf"))
                    .replace("/etc/systemd/system", &d)
                    .replace("/etc/fleet-xray", &format!("{d}/fx"))
                    .replace("/etc/resolv.conf", &format!("{d}/resolv.conf"))
            );
            let _ = std::fs::remove_file(dir.path().join("log"));
            let out = std::process::Command::new("bash").arg("-c").arg(&sh).output().unwrap();
            assert!(out.status.success(), "{out:?}");
            (String::from_utf8_lossy(&out.stdout).into_owned(), std::fs::read_to_string(dir.path().join("log")).unwrap_or_default())
        };
        let (ll, public) = ("nameserver 169.254.169.254\n", "nameserver 1.1.1.1\nnameserver 8.8.8.8\nnameserver 2606:4700:4700::1111\n");
        // Xray 还看着链路本地的那份：重启一次
        let (out, log) = run(ll, "42", ll, true);
        assert!(out.contains("linklocal_dns=1\n") && out.contains("xray_restart=resolv\n") && log == "restart fleet-xray\n", "{out}{log}");
        // 重启过（看到的就是公共解析器那份）、不是链路本地、Xray 没在跑、单元没装上：都不重启
        for (resolv, pid, sees, unit_ok) in [(ll, "42", public, true), ("nameserver 10.0.0.2\n", "42", "nameserver 10.0.0.2\n", true), (ll, "0", ll, true), (ll, "42", ll, false)] {
            let (out, log) = run(resolv, pid, sees, unit_ok);
            assert!(!out.contains("xray_restart") && log.is_empty(), "{resolv} {pid} {unit_ok}: {out}{log}");
        }
    }
}
