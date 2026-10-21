-- 0013：机器显示名的 SQL 版，与 names::display_name 一致（线路机用 label，节点用 团队-编号、编号不足两位补零，否则 label，都没有为「未命名」）；
-- kind 为空（left join 没连上机器）返回 null。各列表查询共用，不再各写一份。
create function display_name(kind text, label text, team text, number int) returns text language sql immutable parallel safe as $$
    select case
        when kind is null then null
        when kind = 'relay' and label is not null then label
        when team is not null and number is not null then team || '-' || case when number < 10 then '0' || number::text else number::text end
        when label is not null then label
        else '未命名'
    end
$$;

-- TOTP 从未实现，去掉占位列
alter table app_user drop column totp_secret_enc;
