#!/bin/bash
# 编译并（重新）部署实验室中控 fleet-dev 上的 fleet-server。幂等：首次运行会建用户、目录、密钥、配置、管理员账号。
# 管理员初始密码写在 fleet-dev:/root/fleet-admin-password（0600），不打印。
# 同生产：装 /usr/local/bin/fleetctl，建或修复它的全权密钥 central-local（仍有效不换），写给 agent 的 /root/AGENTS.md、/root/CLAUDE.md
# 与 fleet-ops 技能（docs/OPS.md §14）。
# 编译主机：默认就在 fleet-dev 上编（/root/src）；FLEET_BUILD=docker 时在 docker 编译主机上编（lab/build-lib.sh），
# 编好的三个二进制连同 deploy/ 经本机传到 fleet-dev:/root/fleet-prebuilt 再安装。部署目标总是 fleet-dev。
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
. "$HERE/build-lib.sh"
LAB="ssh -F $HOME/.ssh/fleetlab_config fleet-dev"
# 远端目录没有 .git：提交哈希在本机取，经环境变量交给远端的 cargo，agent 与中控的版本号成为 0.1.0+<哈希>。
# 取最后一次改动编译输入的提交：纯前端、纯文档的提交不换哈希，不会让 server、agent 全量重编
GIT_HASH=$(git -C "$HERE/.." log -1 --format=%h -- Cargo.toml Cargo.lock crates migrations deploy)
echo "== 编译 ${GIT_HASH}（${FB_DESC}）"
fb_sync
fb_build_release "$GIT_HASH"
# 安装用的文件：lab 直接用 fleet-dev:/root/src 里的；docker 把二进制与 deploy/ 传到 fleet-dev:/root/fleet-prebuilt
PREBUILT=
if [ "$FB_MODE" = docker ]; then
  PREBUILT=/root/fleet-prebuilt
  STAGE=$(mktemp -d)
  trap 'rm -rf "$STAGE"' EXIT
  mkdir -p "$STAGE/bin"
  fb_bins_tar fleet-server fleet-agent fleetctl | tar --no-same-owner -xzf - -C "$STAGE/bin"
  cp -R "$HERE/../deploy" "$STAGE/deploy"
  fb_tar -C "$STAGE" -czf - bin deploy \
    | $LAB "rm -rf $PREBUILT && mkdir -p $PREBUILT && cd $PREBUILT && tar --no-same-owner -xzf - && $FB_RM_APPLEDOUBLE"
fi
$LAB "PREBUILT=$PREBUILT bash -s" <<'REMOTE'
set -euo pipefail
dpkg -s sshpass openssh-client >/dev/null 2>&1 || { apt-get install -y -qq sshpass openssh-client >/dev/null && apt-get clean; }   # 不留 .deb 缓存
# 二进制与 deploy/ 的位置：fleet-dev 上编的在 /root/src；docker 编译主机编的在 $PREBUILT（平铺的 bin/ 与 deploy/）
if [ -n "$PREBUILT" ]; then SRV=$PREBUILT/bin/fleet-server MUSL=$PREBUILT/bin DEPLOY=$PREBUILT/deploy
else SRV=/root/src/target/release/fleet-server MUSL=/root/src/target/x86_64-unknown-linux-musl/release DEPLOY=/root/src/deploy
fi
# 中控本机的 fleetctl（同生产，docs/OPS.md §14）：配置与全权密钥由最后的 local-admin 写
install -m 755 "$MUSL/fleetctl" /usr/local/bin/fleetctl.new && mv -f /usr/local/bin/fleetctl.new /usr/local/bin/fleetctl
install -d -m 755 /opt/fleet/dist
install -m 755 "$MUSL/fleet-agent" /opt/fleet/dist/fleet-agent
echo "fleet-agent $(/opt/fleet/dist/fleet-agent version 2>/dev/null | head -1)"
id fleet >/dev/null 2>&1 || useradd --system --home-dir /var/lib/fleet --shell /usr/sbin/nologin fleet
usermod -aG systemd-journal fleet   # fleetctl server-logs 需要读本机 journal
# 同 deploy/central/install.sh：/etc/fleet 归 root、fleet 只读；fleet 可写的 /var/lib/fleet 里由 fleet 自己建文件
install -d -m 750 -o root -g fleet /etc/fleet /etc/fleet/ssh
install -d -m 750 -o fleet -g fleet /var/lib/fleet
install -d -m 755 /opt/fleet /opt/fleet/dist /opt/fleet/web
[ -f /etc/fleet/secret.key ] || { "$SRV" gen-key /etc/fleet/secret.key; chown fleet:fleet /etc/fleet/secret.key; }
[ -f /etc/fleet/ssh/id_ed25519 ] || { ssh-keygen -q -t ed25519 -N "" -C fleet-central-lab -f /etc/fleet/ssh/id_ed25519; chown fleet:fleet /etc/fleet/ssh/id_ed25519*; }
runuser -u fleet -- touch /var/lib/fleet/known_hosts
[ -f /etc/fleet/fleet.toml ] || cat > /etc/fleet/fleet.toml <<'TOML'
listen = "0.0.0.0:8080"
database_url = "postgres://fleet:fleet@127.0.0.1/fleet"
secret_key_file = "/etc/fleet/secret.key"
public_url = "http://192.168.31.60:8080"
web_dir = "/opt/fleet/web"
ssh_key = "/etc/fleet/ssh/id_ed25519"
known_hosts = "/var/lib/fleet/known_hosts"
dist_dir = "/opt/fleet/dist"
job_workers = 8

[retention]
raw_days = 3
access_days = 7

TOML
chown fleet:fleet /etc/fleet/fleet.toml; chmod 640 /etc/fleet/fleet.toml
# 软路由的反向隧道：同 deploy/central/install.sh（fleet-tunnel 账号、tunnel-keys、sshd 的 Match 段；幂等）
bash "$DEPLOY/central/tunnel-setup.sh" "$DEPLOY/central"
install -m 755 "$SRV" /opt/fleet/fleet-server.new && mv -f /opt/fleet/fleet-server.new /opt/fleet/fleet-server
cat > /etc/systemd/system/fleet-server.service <<'UNIT'
[Unit]
Description=fleet-server（团队线路管理 v1）
After=network-online.target postgresql.service
Wants=network-online.target
[Service]
User=fleet
Group=fleet
ExecStart=/opt/fleet/fleet-server serve -c /etc/fleet/fleet.toml
Restart=always
RestartSec=3
Environment=RUST_LOG=info,sqlx=warn,tower_http=warn
# 沙箱与 deploy/central/install.sh 里生产的单元逐行一致，实验室先跑一遍
NoNewPrivileges=yes
ProtectSystem=strict
ReadWritePaths=/var/lib/fleet
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
CapabilityBoundingSet=
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectKernelLogs=yes
ProtectControlGroups=yes
ProtectClock=yes
ProtectHostname=yes
ProtectProc=invisible
RestrictNamespaces=yes
RestrictRealtime=yes
RestrictSUIDSGID=yes
LockPersonality=yes
MemoryDenyWriteExecute=yes
SystemCallArchitectures=native
SystemCallFilter=@system-service
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK
UMask=0077
[Install]
WantedBy=multi-user.target
UNIT
systemctl daemon-reload
systemctl enable -q fleet-server
systemctl restart fleet-server
# 启动时先跑迁移再监听（首次建管理员要等迁移跑完）：最多等 15 秒
for _ in $(seq 15); do curl -s -o /dev/null http://127.0.0.1:8080/api/v1/auth/me && break; sleep 1; done
if [ ! -f /root/fleet-admin-password ]; then
  (umask 077; head -c 18 /dev/urandom | base64 | tr -d '/+=' > /root/fleet-admin-password)
  /opt/fleet/fleet-server user-set -c /etc/fleet/fleet.toml admin < /root/fleet-admin-password
fi
echo "fleet-server $(/opt/fleet/fleet-server version 2>/dev/null | awk '{print $2}') $(systemctl is-active fleet-server)"
curl -s -o /dev/null -w 'api=%{http_code}\n' http://127.0.0.1:8080/api/v1/auth/me
# 实验室中控也能 ssh 进来让 agent 全量运维（同 deploy/central/install.sh）：全权密钥 central-local（仍有效不换）、给 agent 的说明
/opt/fleet/fleet-server local-admin -c /etc/fleet/fleet.toml
t=$(mktemp)
sed -e "s|__SITE__|实验室中控 fleet-dev|" \
    -e "s|__NOTE__|这是实验室中控（fleet-dev，192.168.31.60）：机器都是实验室的（含模拟机）；没有 Caddy 与每日备份，下文这两项不适用；别的会话也在用这台机器，重新部署、重启 fleet-server 前先问用户。|" \
    "$DEPLOY/central/central-agents.md" > "$t"
for f in AGENTS.md CLAUDE.md; do install -m 644 "$t" "/root/$f"; done
rm -f "$t"
REMOTE
# fleet-ops 技能不在 v1 里（sync-server.sh 不带），单独传
$LAB 'install -d -m 755 /root/.claude/skills/fleet-ops && cat > /root/.claude/skills/fleet-ops/SKILL.md && chmod 644 /root/.claude/skills/fleet-ops/SKILL.md' \
  < "$HERE/../../.claude/skills/fleet-ops/SKILL.md"
