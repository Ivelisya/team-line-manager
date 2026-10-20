#!/bin/bash
# relays.sh [rollout.sh 的其他选项] —— 三台线路机的 relayd 一起发布（线路机自检 + 互探参数写在这里）
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
L1=8a2b6a08-f0ee-429d-9003-b795feb7d311   # 线路机1 line1  23.148.204.129:20005
L2=2e8dca76-a24f-45a6-94eb-788ee659f814   # 线路机2 central 38.58.57.22:20001
L3=cb4408b7-326e-4c8b-8a8f-a951a4baa9c8   # 线路机3 line3  38.58.57.69:20001
ONLY=${ONLY:-line3 line1 central}
for r in $ONLY; do
  case $r in
    line1)   "$HERE/rollout.sh" --relay "$@" --asset-uuid $L1 --public-ip 23.148.204.129 --master-port 20005 --peer "$L2@38.58.57.22:20001" --peer "$L3@38.58.57.69:20001" line1 23.148.204.129 direct ;;
    central) "$HERE/rollout.sh" --relay "$@" --asset-uuid $L2 --public-ip 38.58.57.22 --master-port 20001 --peer "$L1@23.148.204.129:20005" --peer "$L3@38.58.57.69:20001" central 38.58.57.22 direct ;;
    line3)   "$HERE/rollout.sh" --relay "$@" --asset-uuid $L3 --public-ip 38.58.57.69 --master-port 20001 --peer "$L1@23.148.204.129:20005" --peer "$L2@38.58.57.22:20001" line3 38.58.57.69 direct ;;
  esac
done
