-- 0087：探测点与三网 PING 的操作记录补了中文标题与摘要（api/audit.rs），之前写下的这几种旧记录 search 列只有动作名（标题就是动作名、摘要为空），按探测点名、线路机名搜不到。
-- 摘要在 Rust 里生成、SQL 复现不全：这里只把标题和名字补进 search（搜名字够用）；界面显示的摘要是读出来时现算的，不受影响。
-- 设置类（settings.ping_targets、settings.reach_source）没有名字可搜，按标题搜本来就走动作名，不动。可重复执行（只改还停在动作名的）。
update audit_log set search = case action
        when 'probe_point.create' then '新建探测点'
        when 'probe_point.update' then '修改探测点'
        when 'probe_point.delete' then '删除探测点'
        when 'probe_point.upgrade' then '升级探测点'
        when 'probe_point.install' then '安装探测点'
        when 'probe_point.bootstrap_used' then '探测点取走安装脚本'
        else '修改线路机三网 PING 覆盖'
    end || E'\n' || coalesce(detail->>'name', '')
where action in ('probe_point.create', 'probe_point.update', 'probe_point.delete', 'probe_point.upgrade', 'probe_point.install', 'probe_point.bootstrap_used', 'relay.ping_override')
  and search = action || E'\n';
