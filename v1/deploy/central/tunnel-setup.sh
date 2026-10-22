#!/bin/bash
# 软路由反向隧道在中控 sshd 这一侧要装的（幂等，root 执行；deploy/central/install.sh 与 lab/deploy-server.sh 都调它）：
# - 账号 fleet-tunnel：系统账号、nologin、家目录不存在（不能写）；密码栏设成 *（没有能用的密码，也不算锁住：不开 PAM 的 sshd 会拒绝锁住的账号）；
# - /usr/local/lib/fleet/tunnel-keys：sshd 的 AuthorizedKeysCommand（root 所有、0755，路径上每一级目录 root 所有、别人不可写）；
# - /etc/ssh/sshd_config.d/fleet-tunnel.conf：只对 fleet-tunnel 生效的 Match 段（只许反向转发到 127.0.0.1、不能开会话）。
# 先确认 23000–23999 没被别的程序占着（已经连上的隧道是 sshd 自己在听，不算），sshd -t 通过才 reload，不通过换回原来的、退出 1。
# 用法：tunnel-setup.sh <放着 tunnel-keys 与 sshd-fleet-tunnel.conf 的目录>
set -euo pipefail
SRC=${1:?用法：tunnel-setup.sh <放着 tunnel-keys 与 sshd-fleet-tunnel.conf 的目录>}
id fleet >/dev/null 2>&1 || { echo "没有 fleet 账号：先装 fleet-server"; exit 1; }
id fleet-tunnel >/dev/null 2>&1 || useradd --system --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin fleet-tunnel
usermod -p '*' fleet-tunnel

busy=$(ss -Hltnp '( sport >= :23000 and sport <= :23999 )' 2>/dev/null | grep -v -E '"sshd(-session)?"' || true)
if [ -n "$busy" ]; then
  echo "23000–23999 里有端口被别的程序占着（软路由隧道要用这一段，先把它们挪开）："
  echo "$busy"
  exit 1
fi

install -d -m 755 -o root -g root /usr/local/lib/fleet
install -m 755 -o root -g root "$SRC/tunnel-keys" /usr/local/lib/fleet/tunnel-keys

CONF=/etc/ssh/sshd_config.d/fleet-tunnel.conf
if ! cmp -s "$SRC/sshd-fleet-tunnel.conf" "$CONF"; then
  install -d -m 755 /etc/ssh/sshd_config.d
  [ ! -f "$CONF" ] || cp -p "$CONF" "$CONF.prev"
  install -m 644 -o root -g root "$SRC/sshd-fleet-tunnel.conf" "$CONF"
  if ! out=$(sshd -t 2>&1); then
    echo "sshd -t 不通过，fleet-tunnel.conf 换回原来的：$out"
    if [ -f "$CONF.prev" ]; then mv -f "$CONF.prev" "$CONF"; else rm -f "$CONF"; fi
    exit 1
  fi
  rm -f "$CONF.prev"
  # 在跑才 reload（Debian 13 的 ssh.socket 会按需拉起 ssh.service，没在跑时下一个连接拉起它就读到新配置）
  systemctl try-reload-or-restart ssh.service 2>/dev/null || systemctl try-reload-or-restart sshd.service
fi
grep -qE '^[[:space:]]*Include[[:space:]]+/etc/ssh/sshd_config\.d/\*\.conf' /etc/ssh/sshd_config \
  || echo "注意：/etc/ssh/sshd_config 里没有 Include /etc/ssh/sshd_config.d/*.conf，fleet-tunnel 那一段不会生效"
echo "软路由隧道：fleet-tunnel 账号、tunnel-keys、sshd 片段已就绪"
