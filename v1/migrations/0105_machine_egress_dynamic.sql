-- 0105：机器的出口 IP 可标「动态」。共享 / 轮换出口的供应商（例如日本 NAT「Rakuten Shared」）出口 IP 会自己变，
-- 固定期望 IP 会一直报「出口不通」。egress_dynamic = true 时出口探测只验收「能经节点出网」，回显到任何公网 IP 都算成功；
-- 此时 egress_ip 为 null（二者互斥，由 API 保证）。只用于 VPS 节点，同机 IP 节点不用。可重复执行。
alter table machine add column if not exists egress_dynamic boolean not null default false;
