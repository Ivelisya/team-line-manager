-- 0019：同机多 IP 节点。一台落地机上配了几个公网 IP 时，每个附加的 IP 是一台独立的节点（kind = vps，有自己的团队、编号、
-- primary_ip（它的出口 IP）、node_outbound 和入口）；host_id 指向持有 SSH 与 agent 的那台落地机（宿主机），附加节点自己没有 agent，
-- SSH 也登录宿主机。宿主机要是在用、自身 host_id 为空的落地机（应用层校验）。现有机器 host_id 全为空，不回填
alter table machine add column host_id uuid references machine;
alter table machine add constraint machine_host_vps check (host_id is null or (kind = 'vps' and host_id <> id));
create index machine_host_idx on machine (host_id) where host_id is not null;
