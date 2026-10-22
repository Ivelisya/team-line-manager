-- 0039：机器的系统调优记录。调优档 tune-v1 起由中控固定执行（上架时、作业 tune），见 jobs/tune.rs。
-- tune_profile：最近一次成功应用的调优档（如 tune-v1）；null = 没调优过（迁移前上线的机器）。同机 IP 节点不填（系统设置是宿主机的）。
-- tuned_at：那次的完成时间。不是用户编辑，不动 revision。
alter table machine add column if not exists tune_profile text;
alter table machine add column if not exists tuned_at timestamptz;
