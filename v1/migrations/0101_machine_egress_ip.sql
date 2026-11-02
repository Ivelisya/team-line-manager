-- 0101：机器的出口 IP。端口转发型 NAT 节点（供应商给一个公网入口 IP + 几个同号映射端口）的出站公网 IP 和入口 IP 不是同一个：
-- 线路机连 primary_ip（入口），客户看到的出口是 egress_ip。null = 出口就是 primary_ip（现有机器全部如此）。
-- 出口探测 / 出口验收期望的 IP 取 coalesce(egress_ip, primary_ip)。只用于 VPS 节点，同机 IP 节点（有 host_id）不用。可重复执行。
alter table machine add column if not exists egress_ip inet;
