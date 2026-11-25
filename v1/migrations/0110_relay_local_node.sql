-- 0110：本机节点。可重复执行。
-- 团队想直接用线路机所在机房的 IP（不经家宽、不另开一台 VPS）时，节点就放在线路机上：kind = vps，host_id 指向那台线路机，
-- primary_ip = 线路机的 IP，有自己的团队、编号、入口；入口的出站是 freedom（同线路机直出），不经 ssserver，所以没有 node_outbound 行。
-- 它没有自己的 SSH 与 agent（同同机 IP 节点，借宿主机的），计费、访问策略、订阅、软路由出口都按节点走。
-- relay_local：这台节点是本机节点。单独一列而不是每次连宿主机看类型：IP 查重的唯一索引要按它放行（本机节点与线路机同 IP，
--       一台线路机上可以有几个团队各一台本机节点），索引的条件只能看本行。
alter table machine add column if not exists relay_local boolean not null default false;
alter table machine drop constraint if exists machine_relay_local_check;
alter table machine add constraint machine_relay_local_check check (not relay_local or (kind = 'vps' and host_id is not null));
-- IP 查重不含本机节点（它的 IP 就是宿主线路机的）
drop index if exists machine_ip_live;
create unique index machine_ip_live on machine (primary_ip) where lifecycle <> 'retired' and not relay_local;
