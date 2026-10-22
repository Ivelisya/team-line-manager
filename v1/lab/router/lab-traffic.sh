#!/bin/sh
# 实验室软路由的流量发生器（procd 服务 lab-traffic 常驻跑这个脚本，/usr/share/lab-router/lab-traffic.sh）。
# 经 OpenClash 的 mixed-port 访问出口规则里的域名：gen-exits.sh 给每个出口分了一个镜像站（DOMAIN-SUFFIX,<域名>,节点0x），
# 所以 /connections 里每个出口都有连接与流量：
# - 每个域名一条慢速长连接（限速 RATE，最长 HOLD 秒，断了下一轮重连）；
# - 每分钟每个域名一次短请求（取 8 KB）。
# 来源地址轮流用三个「模拟客户端」（加在 lan 空网桥上的 192.168.124.101–103），连接的 sourceIP 像局域网里的设备；
# 它们只是路由自己的地址，不在 DHCP 租约与邻居表里。
# 流量全经实验室线路机与节点走家里的宽带：每个出口长连接约 1 KB/s，四个出口一天约 0.5 GB。不用时 /etc/init.d/lab-traffic stop。

PROXY=http://127.0.0.1:7890
CLIENTS="192.168.124.101 192.168.124.102 192.168.124.103"
FILE=/debian/dists/trixie/main/binary-amd64/Packages.xz   # 各镜像站都有，约 9 MB，长连接下不完
RATE=1k
HOLD=600
RUN=/tmp/lab-traffic

conf=$(uci -q get openclash.config.config_path)

# 第 $1 个模拟客户端（循环取）
client() {
	local k=$1
	set -- $CLIENTS
	shift $((k % $#))
	echo "$1"
}

# 起一个 curl 交给 init 收养（不留僵尸进程）；长连接把 pid 记到 $1
fetch() {
	local pidfile=$1
	shift
	(
		curl -s -o /dev/null -x "$PROXY" "$@" &
		[ -z "$pidfile" ] || echo $! > "$pidfile"
	)
}

stop_all() {
	for f in "$RUN"/*.pid; do
		[ -f "$f" ] && kill "$(cat "$f")" 2>/dev/null
	done
	rm -rf "$RUN"
	for c in $CLIENTS; do ip addr del "$c/32" dev br-lan 2>/dev/null; done
	exit 0
}
trap stop_all TERM INT

mkdir -p "$RUN"
round=0
while :; do
	# OpenClash 没在听 mixed-port（开机时两个服务差不多同时起、或被停了）就等它，免得这一轮全白跑
	until netstat -ltn | grep -q ":${PROXY##*:} "; do
		sleep 2 &
		wait $!
	done
	for c in $CLIENTS; do ip addr add "$c/32" dev br-lan 2>/dev/null; done
	n=0
	for host in $(grep -o 'DOMAIN-SUFFIX,[^,]*' "$conf" 2>/dev/null | cut -d, -f2); do
		pid=$(cat "$RUN/$host.pid" 2>/dev/null)
		if [ -z "$pid" ] || ! kill -0 "$pid" 2>/dev/null; then
			fetch "$RUN/$host.pid" --interface "$(client $n)" --limit-rate "$RATE" -m "$HOLD" "https://$host$FILE"
		fi
		fetch "" --interface "$(client $((n + round + 1)))" -r 0-8191 -m 30 "https://$host$FILE"
		n=$((n + 1))
	done
	round=$((round + 1))
	sleep 60 &
	wait $!
done
