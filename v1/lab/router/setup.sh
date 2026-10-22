#!/bin/sh
# 在实验室软路由里执行（lab/mkrouter.sh 把 lab/router 与 OpenClash 核心放到 /tmp/lab-router 后经 pct exec 调用）：
# 装软件包、放模拟 OpenClash / router-tunnel 空壳 / 流量发生器、改 uci。可反复执行：内容没变的文件不动，需要时才重启服务。
# 标准输入两行：中控登录公钥（fleet-dev:/etc/fleet/ssh/id_ed25519.pub）、root 密码（fleet-dev:/root/lab-root-passwords/lab-router1）。都不打印。
set -eu
export PATH=/usr/sbin:/usr/bin:/sbin:/bin
SRC=$(cd "$(dirname "$0")" && pwd)
LIB=/usr/share/lab-router
read -r central_pub
read -r root_pw
[ -n "$central_pub" ] && [ -n "$root_pw" ] || { echo "setup：标准输入应是中控公钥、root 密码两行" >&2; exit 1; }

# 换上内容有变的文件；换了返回 0，没变返回 1
put() { # 源 目标 权限
	cmp -s "$1" "$2" && return 1
	cp "$1" "$2.tmp"
	chmod "$3" "$2.tmp"
	mv "$2.tmp" "$2"
}

installed() { opkg status "$1" | grep -q '^Status:.* installed'; }

# 1. LXC 里 ujail 建不了 jail（挂载 /dev/log 被拒），dnsmasq 一直起不来、本机没有 DNS：去掉 procd-ujail，服务直接跑
if installed procd-ujail; then
	opkg remove procd-ujail >/dev/null
	/etc/init.d/dnsmasq restart >/dev/null 2>&1   # 它先在空网桥 lan 上探一次别的 DHCP 服务，udhcpc 的输出不用看
fi
for _ in 1 2 3 4 5 6 7 8 9 10; do nslookup downloads.openwrt.org 127.0.0.1 >/dev/null 2>&1 && break; sleep 1; done

# 2. 软件包（官方源，opkg 校验签名）：OpenSSH 客户端与 ssh-keygen、curl，同生产
missing=
for p in openssh-client openssh-keygen curl; do
	installed "$p" || missing="$missing $p"
done
if [ -n "$missing" ]; then
	opkg update >/dev/null
	opkg install $missing >/dev/null
fi

# 3. 登录：中控公钥（接入脚本以后也会追加同一把，已有就不重复加）、root 密码
(
	umask 077
	mkdir -p /etc/dropbear
	touch /etc/dropbear/authorized_keys
)
chmod 600 /etc/dropbear/authorized_keys
grep -qxF "$central_pub" /etc/dropbear/authorized_keys || echo "$central_pub" >> /etc/dropbear/authorized_keys
printf '%s\n%s\n' "$root_pw" "$root_pw" | passwd root >/dev/null 2>&1 || { echo "setup：设置 root 密码失败" >&2; exit 1; }

# 4. 防火墙：wan 是家里的局域网，放行那里来的 SSH（fleet-dev 直连用；生产经隧道登录，wan 不开 22）
if [ "$(uci -q get firewall.lab_ssh)" != rule ]; then
	uci -q batch <<-'EOF'
		set firewall.lab_ssh=rule
		set firewall.lab_ssh.name='Allow-SSH-Lab'
		set firewall.lab_ssh.src='wan'
		set firewall.lab_ssh.src_ip='192.168.31.0/24'
		set firewall.lab_ssh.proto='tcp'
		set firewall.lab_ssh.dest_port='22'
		set firewall.lab_ssh.target='ACCEPT'
		commit firewall
	EOF
	/etc/init.d/firewall reload
fi

# 5. 模拟 OpenClash：核心（PVE 上生产同款 mihomo，mkrouter.sh 校验过 sha256）、符号链接 clash、init 脚本、uci
mkdir -p /etc/openclash/core /etc/openclash/config
oc=0
put "$SRC/clash_meta" /etc/openclash/core/clash_meta 755 && oc=1
if [ "$(readlink /etc/openclash/clash)" != core/clash_meta ]; then
	ln -sfn core/clash_meta /etc/openclash/clash
	oc=1
fi
put "$SRC/openclash.init" /etc/init.d/openclash 755 && oc=1
# uci 按模板重写，只保留已有的 dashboard_password（随机 16 位）
pw=$(uci -q get openclash.config.dashboard_password || true)
case $pw in '' | __DASHBOARD_PASSWORD__) pw=$(hexdump -n 8 -e '8/1 "%02x"' /dev/urandom) ;; esac
(
	umask 077
	sed "s/__DASHBOARD_PASSWORD__/$pw/" "$SRC/uci/openclash" > "$SRC/openclash.uci"
)
put "$SRC/openclash.uci" /etc/config/openclash 600 || true
/etc/init.d/openclash enable
if [ $oc = 1 ] && /etc/init.d/openclash running; then
	/etc/init.d/openclash restart
fi

# 6. router-tunnel 空壳：同名服务，默认不启用
put "$SRC/router-tunnel.init" /etc/init.d/router-tunnel 755 || true
mkdir -p /etc/router-tunnel
[ -f /etc/router-tunnel/ssh_config ] || cat > /etc/router-tunnel/ssh_config <<'EOF'
# 实验室空壳：生产上这里是连 ivekasy 的反向隧道配置（/etc/init.d/router-tunnel 用）。
# 192.0.2.1 是文档保留地址（TEST-NET-1），连不上；服务默认不启用。
Host router-relay
	HostName 192.0.2.1
	User router-tunnel
	RemoteForward 127.0.0.1:22022 127.0.0.1:22
	ServerAliveInterval 30
EOF
/etc/init.d/router-tunnel disable

# 7. 生成出口配置、自检用的脚本，流量发生器（开机自启；第一次由 mkrouter.sh 在 OpenClash 起来后启动）
mkdir -p "$LIB"
put "$SRC/gen-exits.sh" "$LIB/gen-exits.sh" 755 || true
put "$SRC/check.sh" "$LIB/check.sh" 755 || true
if put "$SRC/lab-traffic.sysctl" /etc/sysctl.d/90-lab-traffic.conf 644; then
	sysctl -p /etc/sysctl.d/90-lab-traffic.conf >/dev/null
fi
lt=0
put "$SRC/lab-traffic.sh" "$LIB/lab-traffic.sh" 755 && lt=1
put "$SRC/lab-traffic.init" /etc/init.d/lab-traffic 755 && lt=1
/etc/init.d/lab-traffic enable
if [ $lt = 1 ] && /etc/init.d/lab-traffic running; then
	/etc/init.d/lab-traffic restart
fi

. /etc/openwrt_release
echo "路由已就绪：$DISTRIB_DESCRIPTION，$(ssh -V 2>&1 | cut -d, -f1)，$(curl -V | head -1 | cut -d' ' -f1-2)"
