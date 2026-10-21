#!/bin/bash
# 在 fleet-dev 上把官方发布的 xray、ssserver（及 geo 数据）下载到 /opt/fleet/dist，校验官方 sha256。
# fleet-agent 由 deploy-agent 步骤另行放入。
# 版本固定为实验室在用、验证过的；升级时改下面两行，实验室跑通后再部署生产（deploy/central/deploy.sh 只带变了的文件）。
set -euo pipefail
XRAY_VERSION=v26.3.27
SS_VERSION=v1.25.0
ssh -F "$HOME/.ssh/fleetlab_config" fleet-dev "XV=$XRAY_VERSION SV=$SS_VERSION bash -s" <<'REMOTE'
set -euo pipefail
dpkg -s unzip xz-utils curl >/dev/null 2>&1 || apt-get install -y -qq unzip xz-utils curl >/dev/null 2>&1
D=/opt/fleet/dist; W=$(mktemp -d); trap 'rm -rf "$W"' EXIT; cd "$W"
# Xray
curl -fsSLO "https://github.com/XTLS/Xray-core/releases/download/$XV/Xray-linux-64.zip"
curl -fsSLO "https://github.com/XTLS/Xray-core/releases/download/$XV/Xray-linux-64.zip.dgst"
want=$(grep -i 'SHA2-256' Xray-linux-64.zip.dgst | head -1 | awk '{print $NF}')
got=$(sha256sum Xray-linux-64.zip | cut -d' ' -f1)
[ "$want" = "$got" ] || { echo "xray sha256 不符"; exit 1; }
unzip -q -o Xray-linux-64.zip xray geoip.dat   # 配置只用 geoip:private，不需要 geosite.dat
install -m 755 xray $D/xray; install -m 644 geoip.dat $D/; rm -f $D/geosite.dat
# shadowsocks-rust
T="shadowsocks-$SV.x86_64-unknown-linux-musl.tar.xz"
curl -fsSLO "https://github.com/shadowsocks/shadowsocks-rust/releases/download/$SV/$T"
curl -fsSLO "https://github.com/shadowsocks/shadowsocks-rust/releases/download/$SV/$T.sha256"
sha256sum -c "$T.sha256" >/dev/null || { echo "ssserver sha256 不符"; exit 1; }
tar --no-same-owner -xJf "$T" ssserver
install -m 755 ssserver $D/ssserver
echo "xray=$XV ssserver=$SV"
ls -la $D
REMOTE
