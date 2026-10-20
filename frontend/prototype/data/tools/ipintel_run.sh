#!/bin/sh
# IP intelligence round trip. Real IPs stay on a395: raw.json and the replayed build live in a tmpfs dir
# there and are deleted at the end; only the anonymized ipintel.json comes back.
#
#   sh tools/ipintel_run.sh <a395-ssh-host> <out.json>
#   python3 tools/ipintel_merge.py <out.json> .
#
# Needs on a395: python3 (stdlib only) and the offline databases in ~/.cache/team-ops-ipintel
# (see README: ip2region_v4/v6.xdb, dbip-city-lite-YYYY-MM.mmdb, dbip-asn-lite-YYYY-MM.mmdb, ip2asn-combined.tsv.gz).
set -eu
HOST=${1:?a395 ssh host}
OUT=${2:?output json}
HERE=$(cd "$(dirname "$0")" && pwd)
DATA=$(cd "$HERE/.." && pwd)
TMP=$(ssh "$HOST" 'mktemp -d /tmp/ipintel.XXXXXX')
trap 'ssh "$HOST" "rm -rf $TMP"' EXIT
ssh "$HOST" "mkdir -p $TMP/pub"
scp -q "$HERE/extract.py" "$HERE/build.py" "$HERE/ipdb.py" "$HERE/ipintel_remote.py" "$HOST:$TMP/"
scp -q "$DATA/snapshot.json" "$DATA/detail.json" "$DATA/extra.json" "$HOST:$TMP/pub/"
ssh "$HOST" "cd $TMP && nice -n 10 python3 - < extract.py > raw.json && nice -n 10 python3 ipintel_remote.py raw.json build.py pub ~/.cache/team-ops-ipintel > ipintel.json"
scp -q "$HOST:$TMP/ipintel.json" "$OUT"
