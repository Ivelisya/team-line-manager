-- 0029：数据模型归一。
-- 1. 退役（及上架 / 恢复取消）不再删出站参数行，只清秘密：NAT 映射的 SS 端口、上游代理的协议 / 地址 / 端口 / 用户名留着，
--    恢复时默认沿用（请求里给了的照旧覆盖），秘密照旧重新生成或重新填。之前退役的机器没有这一行，恢复时照旧要给
alter table node_outbound alter column secret_enc drop not null;

-- 2. 同机 IP 节点的 SSH 端口不再抄一份宿主机的（宿主机改了端口副本就旧了）：存 null，读的时候取宿主机的 coalesce(h.ssh_port, m.ssh_port)。
--    供应商（provider_id）另行处理，这里不动
alter table machine alter column ssh_port drop not null;
update machine set ssh_port = null where host_id is not null;
alter table machine add constraint machine_ssh_port_host check ((host_id is null) = (ssh_port is not null));

-- 3. 币种统一成 3 个大写字母（ISO 4217）。先去空白、转大写；续费记录记下登记时订阅的币种（之后改订阅币种，历史金额不跟着变）
update plan set currency = upper(btrim(currency)) where currency <> upper(btrim(currency));
update subscription set currency = upper(btrim(currency)) where currency <> upper(btrim(currency));
alter table renewal add column currency text;
-- 取订阅的生效币种（0026 的 subscription_eff：没单独设置的跟随套餐）；续费要先有订阅、订阅不单独删，都没有正常不会出现，兜底用 USD
update renewal r set currency = e.currency from subscription_eff e where e.machine_id = r.machine_id;
update renewal set currency = 'USD' where currency is null;
alter table renewal alter column currency set not null;

-- 4. 通知渠道只有代码支持的三种（email 从来没实现过，接口也建不出来）
alter table notify_channel drop constraint notify_channel_kind_check;

-- 3、4 的检查约束先 not valid 加上（新写入立即受约束），再逐个验证存量；存量里还有不合规的（接口以前收 1–8 个字母的套餐币种、
-- 订阅币种不校验）不让迁移失败：约束保持 not valid 并告警，改掉那几行后 `alter table … validate constraint …` 即可
alter table plan add constraint plan_currency_iso check (currency ~ '^[A-Z]{3}$') not valid;
alter table subscription add constraint subscription_currency_iso check (currency ~ '^[A-Z]{3}$') not valid;
alter table renewal add constraint renewal_currency_iso check (currency ~ '^[A-Z]{3}$') not valid;
alter table notify_channel add constraint notify_channel_kind_check check (kind in ('telegram', 'wecom', 'webhook')) not valid;
do $$
declare c record;
begin
    for c in select * from (values ('plan', 'plan_currency_iso'), ('subscription', 'subscription_currency_iso'),
                                   ('renewal', 'renewal_currency_iso'), ('notify_channel', 'notify_channel_kind_check')) v(t, n) loop
        begin
            execute format('alter table %I validate constraint %I', c.t, c.n);
        exception when check_violation then
            raise warning '0029：% 里有不合规的存量行，约束 % 暂不验证（新写入已受约束）', c.t, c.n;
        end;
    end loop;
end
$$;

-- 5. 静默的对象只能是机器（接口校验过）：补外键，彻底删除机器时一并删掉它的静默（purge 本来就删）。
--    对象已不存在的静默没有意义，先删掉。告警（incident.subject_id）的对象可以是机器、入口、通知渠道或中控（全零 uuid），不加外键
delete from silence s where s.subject_id is not null and not exists (select 1 from machine m where m.id = s.subject_id);
alter table silence add constraint silence_subject_fk foreign key (subject_id) references machine on delete cascade;
create index silence_subject on silence (subject_id) where subject_id is not null;
