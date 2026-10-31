#!/bin/bash
# 在 fleet-dev 上把官方发布的 xray、ssserver（及 geo 数据）、sing-box 下载到 /opt/fleet/dist，校验 sha256。
# fleet-agent 由 deploy-agent 步骤另行放入。
# 版本固定为实验室在用、验证过的；升级时改下面几行，实验室跑通后再部署生产（deploy/central/deploy.sh 只带变了的文件）。
# sing-box（线路机入口的 AnyTLS 外壳）：官方发布没有校验文件，sha256 取 GitHub 发布资产的 digest 写死在这里（升级时一起改）。
# 用 musl 静态版（同 ssserver），不依赖线路机的 glibc；默认的 linux-amd64 包是动态链接的
set -euo pipefail
XRAY_VERSION=v26.3.27
SS_VERSION=v1.25.0
SB_VERSION=1.14.4
SB_SHA256=8f7c5f0651238850d96a0e488e5473cd77ec1a0098f63942e943f475a12057ca
ssh -F "$HOME/.ssh/fleetlab_config" fleet-dev "XV=$XRAY_VERSION SV=$SS_VERSION BV=$SB_VERSION BS=$SB_SHA256 bash -s" <<'REMOTE'
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
# sing-box
T="sing-box-$BV-linux-amd64-musl.tar.gz"
curl -fsSLO "https://github.com/SagerNet/sing-box/releases/download/v$BV/$T"
[ "$(sha256sum "$T" | cut -d' ' -f1)" = "$BS" ] || { echo "sing-box sha256 不符"; exit 1; }
tar --no-same-owner -xzf "$T" "sing-box-$BV-linux-amd64-musl/sing-box"
install -m 755 "sing-box-$BV-linux-amd64-musl/sing-box" $D/sing-box
echo "xray=$XV ssserver=$SV sing-box=$BV"
ls -la $D
REMOTE
