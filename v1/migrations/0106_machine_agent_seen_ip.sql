-- 0106：中控收到这台机器 agent 上报时看到的来源 IP（取法同 /echo：auth::client_ip），变了才更新。
-- 出口动态（egress_dynamic）的节点出站 IP 会变，只开 AnyTLS 的线路机要放行它穿透 bridge 的来源：期望出口 IP 取
-- coalesce(agent_seen_ip, egress_ip, primary_ip)（entries::desired_state）。节点 agent 与 bridge 同机同出口，所以这就是线路机会看到的 bridge 来源 IP。
-- 不改 revision。可重复执行。
alter table machine add column if not exists agent_seen_ip inet;
