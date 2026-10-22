-- 0036：同机 IP 节点支持 1:1 NAT 映射的附加 IP。有的供应商把附加的公网 IP 映射到宿主机网卡上的一个内网地址（所有端口都转进来，
-- 经它出站的流量也换成这个公网 IP）：公网 IP 不在网卡上，ssserver 要监听并从这个本地地址出站。bind_ip 记这个本地地址，
-- 上架 / 恢复的第一步（在宿主机上核对 IP）探测出来写入；为空 = 公网 IP 直接配在网卡上（之前上架的全是这种，不回填）。
-- 线路机连节点、入口的地址仍是公网 IP（primary_ip / node_outbound.host），只有宿主机上的 ssserver 配置用它
alter table machine add column bind_ip inet;
alter table machine add constraint machine_bind_ip_host check (bind_ip is null or host_id is not null);
