-- 0037：团队可以标成「备用」（spare）。替换下来的机器放进备用团队；备用团队里在用的机器订阅过期后再留 1 天，
-- 即账单时区的今天晚于「到期日 + 1 天」时，由中控每小时的维护任务自动退役（带清理）、退役完成且允许彻底删除时接着彻底删除（见 spare.rs）。
-- 续费顺延了到期日自然不删。可重复执行。
alter table team add column if not exists spare boolean not null default false;
