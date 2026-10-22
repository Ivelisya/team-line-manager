-- 0023：退役时记下的主机公钥（中控 known_hosts 里这台机器的记录行）。退役、上架/恢复取消会把这台机器从 known_hosts 删掉，
-- 恢复时先按它放回再核对：IP 换了主人时预检失败，不会按首次信任把 root 密码和中控密钥交出去。恢复成功后清空。
-- 同机 IP 节点、家宽代理没有；这次之前退役的机器为空（恢复时照旧按现在的公钥重新记录）
alter table machine add column retired_host_keys text;
