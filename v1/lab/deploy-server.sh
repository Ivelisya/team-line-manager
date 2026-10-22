#!/bin/bash
# 在 fleet-dev 上编译并（重新）部署 fleet-server。幂等：首次运行会建用户、目录、密钥、配置、管理员账号。
# 管理员初始密码写在 fleet-dev:/root/fleet-admin-password（0600），不打印。
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
bash "$HERE/sync-server.sh"
# 远端目录没有 .git：提交哈希在本机取，经环境变量交给远端的 cargo，agent 与中控的版本号成为 0.1.0+<哈希>。
# 取最后一次改动编译输入的提交：纯前端、纯文档的提交不换哈希，不会让 server、agent 全量重编
GIT_HASH=$(git -C "$HERE/.." log -1 --format=%h -- Cargo.toml Cargo.lock crates migrations deploy)
ssh -F "$HOME/.ssh/fleetlab_config" fleet-dev "FLEET_GIT_HASH=$GIT_HASH bash -s" <<'REMOTE'
set -euo pipefail
export PATH=$HOME/.cargo/bin:$PATH
dpkg -s sshpass openssh-client >/dev/null 2>&1 || { apt-get install -y -qq sshpass openssh-client >/dev/null && apt-get clean; }   # 不留 .deb 缓存
cd /root/src
# 成功只打最后一行（用时），失败打全部输出：编译错误在前面
build() { local out; out=$(cargo build "$@" 2>&1) || { printf '%s\n' "$out"; return 1; }; printf '%s\n' "$out" | tail -1; }
build -p fleet-server --release
build -p fleet-agent --release --target x86_64-unknown-linux-musl
install -d -m 755 /opt/fleet/dist
install -m 755 target/x86_64-unknown-linux-musl/release/fleet-agent /opt/fleet/dist/fleet-agent
echo "fleet-agent $(/opt/fleet/dist/fleet-agent version 2>/dev/null | head -1)"
id fleet >/dev/null 2>&1 || useradd --system --home-dir /var/lib/fleet --shell /usr/sbin/nologin fleet
usermod -aG systemd-journal fleet   # fleetctl server-logs 需要读本机 journal
# 同 deploy/central/install.sh：/etc/fleet 归 root、fleet 只读；fleet 可写的 /var/lib/fleet 里由 fleet 自己建文件
install -d -m 750 -o root -g fleet /etc/fleet /etc/fleet/ssh
install -d -m 750 -o fleet -g fleet /var/lib/fleet
install -d -m 755 /opt/fleet /opt/fleet/dist /opt/fleet/web
[ -f /etc/fleet/secret.key ] || { /root/src/target/release/fleet-server gen-key /etc/fleet/secret.key; chown fleet:fleet /etc/fleet/secret.key; }
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

[numbering]
auto_compact = true
TOML
chown fleet:fleet /etc/fleet/fleet.toml; chmod 640 /etc/fleet/fleet.toml
install -m 755 /root/src/target/release/fleet-server /opt/fleet/fleet-server.new && mv -f /opt/fleet/fleet-server.new /opt/fleet/fleet-server
cat > /etc/systemd/system/fleet-server.service <<'UNIT'
[Unit]
Description=fleet-server（团队线路管理 v1）
# 等时钟同步再起：启动时立刻按墙钟删过期分区，开机时钟大幅超前会误删数据
After=network-online.target postgresql.service time-sync.target
Wants=network-online.target time-sync.target
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
systemctl is-active fleet-server
curl -s -o /dev/null -w 'api=%{http_code}\n' http://127.0.0.1:8080/api/v1/auth/me
REMOTE
