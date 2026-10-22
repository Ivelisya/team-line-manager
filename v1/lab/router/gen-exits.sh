#!/bin/sh
# 生成实验室软路由的 OpenClash 源配置（lab/mkrouter.sh 调用，在路由上跑：/usr/share/lab-router/gen-exits.sh）。
# 标准输入每行「vless:// 分享链接 说明」：实验室中控里在用的入口（最多 6 条），按顺序命名 节点01、节点02…；
# 另加一个连不上的 节点99（server 是文档保留地址 192.0.2.1），测出口故障告警。
# proxies 用生产那种块格式（name 下面缩进 type / server / … / reality-opts 两层）；没有代理组（同生产，只有内置的 GLOBAL），
# 规则把每个出口分到一个镜像站域名（DOMAIN-SUFFIX → 节点0x），lab-traffic 照这些规则造流量。
# 写到 uci openclash.config.config_path（0600），先用核心 -t 校验；内容没变就不动。只打印出口名、server:port 与说明，不打印凭据。
set -eu
export PATH=/usr/sbin:/usr/bin:/sbin:/bin
umask 077
# 第 i 个出口的流量走第 i 个镜像站（清华 tuna / bfsu 对 curl 的 GET 回 403，不用）
HOSTS="mirrors.aliyun.com mirrors.huaweicloud.com mirrors.cloud.tencent.com mirrors.ustc.edu.cn mirrors.nju.edu.cn mirrors.volces.com"
DEAD_HOST=www.example.com
DEAD_SERVER=192.0.2.1
CORE=/etc/openclash/core/clash_meta
CONF=$(uci get openclash.config.config_path)
TMP=$CONF.new

die() {
	echo "gen-exits：$*" >&2
	rm -f "$TMP" "$TMP.proxies" "$TMP.rules"
	exit 1
}

# 查询参数 $2 的值（分享链接里只有域名、base64url、十六进制，不用解码）
param() {
	printf '&%s\n' "$1" | sed -n "s/.*&$2=\([^&]*\).*/\1/p"
}

# 只放行预期的字符，免得怪值把 YAML 弄坏
check() { # 值 允许的字符 说明
	case $1 in
	'' | *[!$2]*) die "第 $n 条的 $3 不对" ;;
	esac
}

proxy() { # 名字 server port uuid sni pbk sid
	cat <<EOF
  - name: $1
    type: vless
    server: $2
    port: $3
    uuid: $4
    tls: true
    udp: true
    flow: xtls-rprx-vision
    network: tcp
    servername: $5
    client-fingerprint: chrome
    reality-opts:
      public-key: $6
      short-id: "$7"
EOF
}

mkdir -p "$(dirname "$CONF")"
: > "$TMP.proxies"
: > "$TMP.rules"
set -- $HOSTS
n=0
while read -r url label; do
	[ -n "$url" ] || continue
	n=$((n + 1))
	[ $# -gt 0 ] || die "最多 6 个出口"
	case $url in vless://*) ;; *) die "第 $n 条不是 vless:// 链接" ;; esac
	u=${url#vless://}
	uuid=${u%%@*}
	u=${u#*@}
	hp=${u%%\?*}
	server=${hp%:*}
	port=${hp##*:}
	query=${u#*\?}
	query=${query%%#*}
	[ "$(param "$query" security)" = reality ] && [ "$(param "$query" type)" = tcp ] &&
		[ "$(param "$query" flow)" = xtls-rprx-vision ] || die "第 $n 条不是 VLESS Reality（tcp + xtls-rprx-vision）"
	sni=$(param "$query" sni)
	pbk=$(param "$query" pbk)
	sid=$(param "$query" sid)
	check "$uuid" 0-9a-f- uuid
	check "$server" 0-9A-Za-z.- server
	check "$port" 0-9 端口
	check "$sni" 0-9A-Za-z.- sni
	check "$pbk" 0-9A-Za-z_- 公钥
	[ -z "$sid" ] || check "$sid" 0-9a-f short-id
	name=$(printf '节点%02d' $n)
	proxy "$name" "$server" "$port" "$uuid" "$sni" "$pbk" "$sid" >> "$TMP.proxies"
	echo "  - DOMAIN-SUFFIX,$1,$name" >> "$TMP.rules"
	echo "$name  $server:$port  $label"
	[ $n -gt 1 ] || { dead_sni=$sni; dead_pbk=$pbk; dead_sid=$sid; }
	shift
done
[ $n -gt 0 ] || die "没有收到实验室入口的分享链接"
# 节点99：参数抄节点01 的（核心要校验格式），只是 server 连不上
proxy 节点99 "$DEAD_SERVER" 20099 00000000-0000-4000-8000-000000000099 "$dead_sni" "$dead_pbk" "$dead_sid" >> "$TMP.proxies"
echo "  - DOMAIN-SUFFIX,$DEAD_HOST,节点99" >> "$TMP.rules"
echo "节点99  $DEAD_SERVER:20099  故意连不上（文档保留地址）"

{
	cat <<'EOF'
# 实验室软路由的出口（lab/router/gen-exits.sh 生成）：节点01… 是实验室中控的入口，节点99 故意连不上
mixed-port: 7890
allow-lan: true
bind-address: '*'
lan-allowed-ips:
  - 127.0.0.0/8
  - 192.168.124.0/24
mode: rule
log-level: warning
ipv6: false
proxies:
EOF
	cat "$TMP.proxies"
	echo "rules:"
	cat "$TMP.rules"
	echo "  - MATCH,DIRECT"
} > "$TMP"
rm -f "$TMP.proxies" "$TMP.rules"
"$CORE" -t -d /etc/openclash -f "$TMP" > "$TMP.log" 2>&1 || { tail -3 "$TMP.log" >&2; rm -f "$TMP.log"; die "核心校验没通过"; }
rm -f "$TMP.log"
if cmp -s "$TMP" "$CONF"; then
	rm -f "$TMP"
	echo "出口配置没变：$CONF"
else
	mv "$TMP" "$CONF"
	echo "已写入出口配置：$CONF"
fi
