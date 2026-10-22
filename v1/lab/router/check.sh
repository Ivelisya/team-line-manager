#!/bin/sh
# 实验室软路由自检（lab/mkrouter.sh 最后跑；/usr/share/lab-router/check.sh）：采集脚本要读的东西是不是都在、出口通不通。
# 不打印任何密钥。延迟测试最多等 5 秒（节点99 一定失败）。
export PATH=/usr/sbin:/usr/bin:/sbin:/bin
port=$(uci -q get openclash.config.cn_port)
secret=$(uci -q get openclash.config.dashboard_password)
api() { curl -fsS -m 20 -H "Authorization: Bearer $secret" "http://127.0.0.1:$port$1"; }
# OpenClash 刚起来时等它的接口
for _ in 1 2 3 4 5 6 7 8 9 10; do api /version >/dev/null 2>&1 && break; sleep 1; done

. /etc/openwrt_release
wan=$(ubus call network.interface.wan status)
echo "系统        $DISTRIB_DESCRIPTION，内核 $(uname -r)，主机名 $(cat /proc/sys/kernel/hostname)"
echo "wan         up=$(echo "$wan" | jsonfilter -e '@.up')，$(echo "$wan" | jsonfilter -e '@["ipv4-address"][0].address')，$(echo "$wan" | jsonfilter -e '@.device')"
echo "工具        $(ssh -V 2>&1 | cut -d, -f1)，$(curl -V | head -1 | cut -d' ' -f1-2)，$(for c in ucode jsonfilter ubus uci dropbear; do command -v $c; done | tr '\n' ' ')"
echo "OpenClash   enable=$(uci -q get openclash.config.enable)，en_mode=$(uci -q get openclash.config.en_mode)，pidof clash=$(pidof clash)，pidof clash_meta=$(pidof clash_meta)，$(api /version | jsonfilter -e @.version)"
echo "出口        $(api /proxies | jsonfilter -e '@.proxies[@.type="Vless"].name' | sort | tr '\n' ' ')"
echo "延迟（毫秒）$(api '/group/GLOBAL/delay?url=https://www.gstatic.com/generate_204&timeout=5000')"
echo "连接        $(api /connections | jsonfilter -e '@.connections[*].chains[0]' | sort | uniq -c | awk '{printf "%s×%s ", $2, $1}')"
echo "conntrack   $(cat /proc/sys/net/netfilter/nf_conntrack_count 2>&1) / $(cat /proc/sys/net/netfilter/nf_conntrack_max 2>&1)"
echo "服务        openclash $(/etc/init.d/openclash enabled && echo 启用 || echo 未启用)，lab-traffic $(/etc/init.d/lab-traffic enabled && echo 启用 || echo 未启用)，router-tunnel $(/etc/init.d/router-tunnel enabled && echo 启用 || echo 未启用)"
echo "登录公钥    $(grep -c . /etc/dropbear/authorized_keys) 把（/etc/dropbear/authorized_keys）"
