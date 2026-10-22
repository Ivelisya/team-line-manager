#!/bin/bash
# 自动更新大陆优化入口 cn.kismet613.com 的优选 IP（docs/OPS.md §11），由 fleet-cf-yx.timer 每 6 小时运行，也可手动 systemctl start。
# IP 来源是社区按运营商在国内实测维护的优选域名：电信 ct.090227.xyz、移动 cmcc.090227.xyz（联通走 CF 普遍不如直连，联通用户用
# fleet.kismet613.com）。只接受落在 Cloudflare 官方网段、且经它访问 https://<入口>/cdn-cgi/trace 真能拿到本区响应的 IP，
# 电信、移动各取第一个合格的写成灰云 A 记录——解析权在自己手里，对方给出非 CF 的 IP 也不会被采用（不 CNAME 过去，防劫持）。
# 一个合格的都没有就不动；先加新记录再删旧的，记录集不会出现空窗。
# 令牌 /etc/fleet/cf-dns.token：只能改 kismet613.com 的 DNS、只允许中控的 IPv4 调用，所以访问 CF 接口一律 curl -4。
set -euo pipefail
HOST=${CF_YX_HOST:-cn.kismet613.com}
ZONE=${CF_YX_ZONE:-31c0f01f9b02a89ffc3c24999bc3f661}
SOURCES=${CF_YX_SOURCES:-"ct.090227.xyz cmcc.090227.xyz"}
API=https://api.cloudflare.com/client/v4
TOKEN=$(cat /etc/fleet/cf-dns.token)

cf() { curl -4 -fsS -m 20 -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' "$@"; }
resolve() { # 名字 第几次：轮流问 1.1.1.1 与 8.8.8.8（各自缓存的随机子集不同，并集更全）
  local url="https://1.1.1.1/dns-query?name=$1&type=A"
  [ $(($2 % 2)) = 1 ] && url="https://8.8.8.8/resolve?name=$1&type=A"
  curl -4 -fsS -m 10 -H 'accept: application/dns-json' "$url" | jq -r '.Answer[]? | select(.type == 1) | .data'
}
# Cloudflare 官方 IPv4 网段：在线取，取不到用内置的（与 install.sh 的 CF_NETS 同源）
CF_V4=$(curl -4 -fsS -m 10 https://www.cloudflare.com/ips-v4 || echo "173.245.48.0/20 103.21.244.0/22 103.22.200.0/22 103.31.4.0/22 141.101.64.0/18 108.162.192.0/18 190.93.240.0/20 188.114.96.0/20 197.234.240.0/22 198.41.128.0/17 162.158.0.0/15 104.16.0.0/13 104.24.0.0/14 172.64.0.0/13 131.0.72.0/22")
in_cf() { python3 -c 'import ipaddress, sys; ip = ipaddress.ip_address(sys.argv[1]); sys.exit(0 if any(ip in ipaddress.ip_network(n) for n in sys.argv[2].split()) else 1)' "$1" "$CF_V4"; }
serves() { curl -4 -s -m 8 --resolve "$HOST:443:$1" "https://$HOST/cdn-cgi/trace" | grep -qx "h=$HOST"; }

cur=$(cf "$API/zones/$ZONE/dns_records?type=A&name=$HOST" | jq -r '.result[] | "\(.id) \(.content)"')
have=$(awk '{print $2}' <<<"$cur" | sort | xargs)

want=()
for src in $SOURCES; do
  pick=""
  # 社区域名每次只从更大的池子里随机返回两三个：查 20 次取并集当名单；现有记录还在名单里就优先留着，避免每次运行都换
  ans=$(for i in $(seq 20); do resolve "$src" "$i" || true; done | awk '!seen[$0]++')
  for ip in $(grep -wF -f <(tr ' ' '\n' <<<"$have" | grep .) <<<"$ans" || true) $ans; do
    [[ " ${want[*]} " == *" $ip "* ]] && continue
    if ! in_cf "$ip"; then echo "$src：$ip 不在 Cloudflare 网段，跳过"; continue; fi
    if ! serves "$ip"; then echo "$src：$ip 访问 $HOST 不通，跳过"; continue; fi
    pick=$ip; break
  done
  if [ -n "$pick" ]; then want+=("$pick"); echo "$src → $pick"; else echo "$src：没有合格的 IP"; fi
done
[ ${#want[@]} -gt 0 ] || { echo "一个合格的 IP 都没有，$HOST 的记录保持不动"; exit 0; }

if [ "$have" = "$(printf '%s\n' "${want[@]}" | sort | xargs)" ]; then echo "$HOST 已是 ${want[*]}，不用改"; exit 0; fi
for ip in "${want[@]}"; do
  grep -qw "$ip" <<<"$have" && continue
  cf -X POST "$API/zones/$ZONE/dns_records" --data "$(jq -nc --arg n "$HOST" --arg ip "$ip" --arg c "优选 IP，fleet-cf-yx 自动维护 $(date +%F)" '{type: "A", name: $n, content: $ip, proxied: false, ttl: 300, comment: $c}')" >/dev/null
  echo "新增 $HOST A $ip"
done
while read -r id ip; do
  [ -n "$id" ] || continue
  [[ " ${want[*]} " == *" $ip "* ]] && continue
  cf -X DELETE "$API/zones/$ZONE/dns_records/$id" >/dev/null
  echo "删除 $HOST A $ip"
done <<<"$cur"
echo "$HOST：$have → ${want[*]}"
