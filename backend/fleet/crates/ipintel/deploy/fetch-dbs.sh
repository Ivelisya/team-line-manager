#!/bin/sh
# fetch-dbs.sh — 下载 / 更新 IP 情报离线库（fleetd ipintel）。
#
#   sh fetch-dbs.sh [DEST]          默认 DEST=/var/lib/fleetd/ipintel
#
# 由 fleetd-ipintel-fetch.timer 每月运行（User=fleetd），也可以手动跑。只从官方地址下载：
#   ip2region_v4.xdb / ip2region_v6.xdb   raw.githubusercontent.com/lionsoul2014/ip2region/master/data/   Apache-2.0
#       校验：GitHub contents API 公布的 size 与 git blob sha1
#   dbip-city-lite-YYYY-MM.mmdb           download.db-ip.com/free/dbip-city-lite-YYYY-MM.mmdb.gz          CC BY 4.0
#   dbip-asn-lite-YYYY-MM.mmdb            download.db-ip.com/free/dbip-asn-lite-YYYY-MM.mmdb.gz           CC BY 4.0
#       校验：db-ip.com 下载页公布的 SHA1SUM（对解压后的 .mmdb）；gzip -t；文件尾有 MaxMind 元数据标记
#   ip2asn-combined.tsv                   iptoasn.com/data/ip2asn-combined.tsv.gz                          PDDL 1.0
#       校验：Content-Length、gzip -t、行数 ≥ 300000、首行格式（官方不公布哈希）
#
# 原子性：全部先下到 DEST 下的临时目录，逐个校验通过后 rename 进 DEST；任何一个失败都保留旧文件（退出码非 0，
# 其余已通过校验的库照常更新）。fleetd 每 10 分钟比对 mtime 自动重新 mmap，已映射的旧 inode 不受影响。
# 本机月份的 DB-IP 还没发布时（每月 1 日前后），自动退回上个月。
set -eu

DEST=${1:-${DEST:-/var/lib/fleetd/ipintel}}
UA="fleetd-ipintel-fetch/1 (+team-ops)"
KEEP_DBIP=1          # DB-IP 只留最新一个月（约 137 MB）；想留回滚副本改成 2
MIN_IP2REGION_V4=5000000
MIN_IP2REGION_V6=10000000
MIN_DBIP_CITY=60000000
MIN_DBIP_ASN=4000000
MIN_IPTOASN_ROWS=300000

umask 027
mkdir -p "$DEST"
TMP=$(mktemp -d "$DEST/.fetch.XXXXXX")
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT INT TERM

log() { printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" >&2; }
fail=0
bad() { log "失败：$*"; fail=1; }

fetch() { # url out
  curl -fsSL --proto '=https' --tlsv1.2 --retry 3 --retry-delay 10 --connect-timeout 20 -m 1800 -A "$UA" -o "$2" "$1"
}
size_of() { wc -c < "$1" | tr -d ' '; }
sha1_of() { sha1sum "$1" | cut -d' ' -f1; }
git_blob_sha1() { # git 对象哈希：sha1("blob <size>\0" + 内容)
  { printf 'blob %s\000' "$(size_of "$1")"; cat "$1"; } | sha1sum | cut -d' ' -f1
}
install_file() { # src name
  chmod 0640 "$1"
  mv -f "$1" "$DEST/$2"
  log "已更新 $2（$(size_of "$DEST/$2") 字节）"
}

# ── ip2region ───────────────────────────────────────────────────────────────────────────────────
API=$(curl -fsSL --proto '=https' -m 60 -A "$UA" -H 'Accept: application/vnd.github+json' \
  "https://api.github.com/repos/lionsoul2014/ip2region/contents/data?ref=master" 2>/dev/null || true)
for v in v4 v6; do
  name="ip2region_$v.xdb"
  out="$TMP/$name"
  if ! fetch "https://raw.githubusercontent.com/lionsoul2014/ip2region/master/data/$name" "$out"; then
    bad "$name 下载失败"; continue
  fi
  # 从 contents API 里取这一项的 sha / size（不依赖 jq：按对象切块再取字段）
  meta=$(printf '%s' "$API" | tr -d '\n' | sed 's/},/}\n/g' | grep "\"name\": *\"$name\"" || true)
  want_sha=$(printf '%s' "$meta" | sed -n 's/.*"sha": *"\([0-9a-f]\{40\}\)".*/\1/p')
  want_size=$(printf '%s' "$meta" | sed -n 's/.*"size": *\([0-9]*\).*/\1/p')
  got_size=$(size_of "$out")
  if [ -n "$want_sha" ]; then
    got_sha=$(git_blob_sha1 "$out")
    if [ "$got_sha" != "$want_sha" ] || [ "$got_size" != "$want_size" ]; then
      bad "$name 校验不符：size $got_size/$want_size sha $got_sha/$want_sha"; continue
    fi
    log "$name 校验通过（git blob sha1 $got_sha）"
  else
    log "警告：拿不到 GitHub 公布的哈希（API 限流？），只做大小与文件头检查"
    min=$MIN_IP2REGION_V4; [ "$v" = v6 ] && min=$MIN_IP2REGION_V6
    if [ "$got_size" -lt "$min" ]; then bad "$name 太小（$got_size 字节）"; continue; fi
  fi
  # xdb 头：structure 2 或 3（小端 u16）
  st=$(od -An -tu2 -N2 "$out" | tr -d ' ')
  if [ "$st" != 2 ] && [ "$st" != 3 ]; then bad "$name 文件头不是 xdb（structure=$st）"; continue; fi
  install_file "$out" "$name"
done

# ── DB-IP Lite（city / asn） ─────────────────────────────────────────────────────────────────────
M0=$(date -u +%Y-%m)
M1=$(date -u -d "$(date -u +%Y-%m-01) -1 month" +%Y-%m 2>/dev/null || echo "")
for kind in city asn; do
  page="https://db-ip.com/db/download/ip-to-$kind-lite"
  done_kind=0
  for m in $M0 $M1; do
    [ -n "$m" ] || continue
    base="dbip-$kind-lite-$m"
    if [ -f "$DEST/$base.mmdb" ]; then log "$base.mmdb 已是最新"; done_kind=1; break; fi
    gz="$TMP/$base.mmdb.gz"
    if ! fetch "https://download.db-ip.com/free/$base.mmdb.gz" "$gz" 2>/dev/null; then
      log "$base 还没发布或下载失败，试上个月"; continue
    fi
    if ! gzip -t "$gz"; then bad "$base.mmdb.gz 损坏"; continue; fi
    gzip -dc "$gz" > "$TMP/$base.mmdb"
    rm -f "$gz"
    # 下载页：每个下载按钮前面是它的 MD5SUM / SHA1SUM（对解压后的 .mmdb，已实测）；取 “… Lite MMDB” 按钮前最近的 SHA1
    html=$(curl -fsSL --proto '=https' -m 60 -A "$UA" "$page" 2>/dev/null || true)
    want=$(printf '%s' "$html" | sed -e 's/<[^>]*>/ /g' | tr -s ' \t\r' '\n' \
      | awk '/^SHA1SUM$/{getline; sha=$0; next}
             { p4=p3; p3=p2; p2=p1; p1=$0 }
             p1=="MMDB" && p2=="Lite" && p4=="to" && sha ~ /^[0-9a-f]{40}$/ {print sha; exit}' || true)
    got=$(sha1_of "$TMP/$base.mmdb")
    if [ -z "$want" ]; then
      log "警告：下载页没解析到 SHA1，只做 gzip 与格式检查"
    elif [ "$want" = "$got" ]; then
      log "$base.mmdb SHA1 校验通过（$got，与 db-ip.com 下载页一致）"
    elif printf '%s' "$html" | grep -q "$base.mmdb.gz"; then
      bad "$base.mmdb SHA1 不符：$got ≠ $want"; continue
    else
      log "警告：下载页公布的已不是 $m 的文件，无法比对 SHA1，只做格式检查"
    fi
    min=$MIN_DBIP_CITY; [ "$kind" = asn ] && min=$MIN_DBIP_ASN
    sz=$(size_of "$TMP/$base.mmdb")
    if [ "$sz" -lt "$min" ]; then bad "$base.mmdb 太小（$sz 字节）"; continue; fi
    if ! tail -c 131072 "$TMP/$base.mmdb" | grep -aq 'MaxMind.com'; then bad "$base.mmdb 没有 MaxMind 元数据标记"; continue; fi
    install_file "$TMP/$base.mmdb" "$base.mmdb"
    done_kind=1
    # 只保留最近 KEEP_DBIP 个月
    ls -1 "$DEST"/dbip-$kind-lite-*.mmdb 2>/dev/null | sort -r | tail -n +$((KEEP_DBIP + 1)) | while read -r old; do
      rm -f "$old" && log "删除旧库 $(basename "$old")"
    done
    break
  done
  [ "$done_kind" = 1 ] || bad "DB-IP $kind 没有可用的版本"
done

# ── iptoasn ─────────────────────────────────────────────────────────────────────────────────────
url="https://iptoasn.com/data/ip2asn-combined.tsv.gz"
gz="$TMP/ip2asn-combined.tsv.gz"
want_len=$(curl -fsSIL --proto '=https' -m 60 -A "$UA" "$url" 2>/dev/null | tr -d '\r' | awk 'tolower($1)=="content-length:"{v=$2} END{print v}')
if fetch "$url" "$gz"; then
  got_len=$(size_of "$gz")
  if [ -n "$want_len" ] && [ "$want_len" != "$got_len" ]; then
    bad "ip2asn-combined.tsv.gz 长度不符：$got_len ≠ $want_len"
  elif ! gzip -t "$gz"; then
    bad "ip2asn-combined.tsv.gz 损坏"
  else
    gzip -dc "$gz" > "$TMP/ip2asn-combined.tsv"
    rows=$(wc -l < "$TMP/ip2asn-combined.tsv" | tr -d ' ')
    head1=$(head -n1 "$TMP/ip2asn-combined.tsv")
    if [ "$rows" -lt "$MIN_IPTOASN_ROWS" ]; then
      bad "ip2asn-combined.tsv 行数太少（$rows）"
    elif ! printf '%s' "$head1" | grep -Eq '^[0-9a-fA-F.:]+	[0-9a-fA-F.:]+	[0-9]+	'; then
      bad "ip2asn-combined.tsv 首行格式不对"
    else
      log "ip2asn-combined.tsv $rows 行"
      install_file "$TMP/ip2asn-combined.tsv" "ip2asn-combined.tsv"
    fi
  fi
else
  bad "ip2asn-combined.tsv.gz 下载失败"
fi

# ── 清单 ────────────────────────────────────────────────────────────────────────────────────────
{
  printf '# fetch-dbs.sh %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  for f in "$DEST"/ip2region_v4.xdb "$DEST"/ip2region_v6.xdb "$DEST"/dbip-*-lite-*.mmdb "$DEST"/ip2asn-combined.tsv; do
    [ -f "$f" ] && printf '%s  %s  %s\n' "$(sha256sum "$f" | cut -d' ' -f1)" "$(size_of "$f")" "$(basename "$f")"
  done
} > "$TMP/MANIFEST" && mv -f "$TMP/MANIFEST" "$DEST/MANIFEST"
du -sh "$DEST" >&2 || true
exit $fail
