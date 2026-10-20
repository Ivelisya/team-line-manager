#!/bin/bash
# 重建实验机（161 线路机、163–165 节点）为「供应商刚交付」的干净状态：
#   lab-relay1 / lab-node1：只给 root 密码（允许密码登录），测引导；
#   lab-node2：预放中控公钥，测密钥直登；
#   lab-node3：跑一个 SOCKS5 上游（microsocks，带用户名密码），模拟上游代理节点。
# 随机密码写到 fleet-dev:/root/lab-root-passwords/<host>（0600），不打印。
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
F="-F $HOME/.ssh/fleetlab_config"
CENTRAL_PUB=$(ssh $F fleet-dev cat /etc/fleet/ssh/id_ed25519.pub)
ssh -o BatchMode=yes pve 'bash -s' <<'PVE'
set -euo pipefail
for id in 161 163 164 165; do pct status $id >/dev/null 2>&1 && { pct stop $id >/dev/null 2>&1 || true; pct destroy $id --purge >/dev/null; echo "destroyed $id"; }; done
PVE
ssh -o BatchMode=yes pve 'bash -s' < "$HERE/../lab/mkct.sh" 2>&1 | grep -E '^created|^skip' || true
# 等容器起来
for h in lab-relay1 lab-node1 lab-node2 lab-node3; do
  ip=$(awk -v h="$h" '$1=="Host" && $2==h {f=1} f && $1=="HostName" {print $2; exit}' "$HOME/.ssh/fleetlab_config")
  ssh-keygen -f "$HOME/.ssh/fleetlab_known_hosts" -R "$ip" >/dev/null 2>&1 || true
  for i in $(seq 1 30); do ssh $F "$h" true 2>/dev/null && break; sleep 2; done
done
ssh $F fleet-dev 'install -d -m 700 /root/lab-root-passwords; : > /var/lib/fleet/known_hosts'
for h in lab-relay1 lab-node1; do
  pw=$(ssh $F fleet-dev "head -c 18 /dev/urandom | base64 | tr -d '/+=' | tee /root/lab-root-passwords/$h; chmod 600 /root/lab-root-passwords/$h")
  printf 'root:%s\n' "$pw" | ssh $F "$h" 'chpasswd && printf "PermitRootLogin yes\nPasswordAuthentication yes\n" > /etc/ssh/sshd_config.d/10-lab-password.conf && systemctl restart ssh && echo "$(hostname): password login on"'
  unset pw
done
echo "$CENTRAL_PUB" | ssh $F lab-node2 'cat >> /root/.ssh/authorized_keys && echo "$(hostname): central key authorized"'
ssh $F lab-node3 'export DEBIAN_FRONTEND=noninteractive; apt-get update -qq >/dev/null && apt-get install -y -qq microsocks >/dev/null && echo ok' | tail -1
ssh $F fleet-dev 'head -c 12 /dev/urandom | base64 | tr -d "/+=" > /root/lab-root-passwords/socks-upstream; chmod 600 /root/lab-root-passwords/socks-upstream'
ssh $F fleet-dev 'cat /root/lab-root-passwords/socks-upstream' | ssh $F lab-node3 'read -r p; cat > /etc/systemd/system/lab-socks.service <<UNIT
[Unit]
Description=lab upstream socks5 (simulates a vendor proxy)
[Service]
ExecStart=/usr/bin/microsocks -i 0.0.0.0 -p 1080 -u labuser -P $p
Restart=always
[Install]
WantedBy=multi-user.target
UNIT
chmod 600 /etc/systemd/system/lab-socks.service; systemctl daemon-reload; systemctl enable --now -q lab-socks; sleep 1; ss -Hltn "sport = :1080" | grep -q . && echo "$(hostname): socks5 up"'
