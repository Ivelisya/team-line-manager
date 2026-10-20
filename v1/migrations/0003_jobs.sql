-- 0003：作业引擎改进

-- 步骤失败的错误类别：network 网络/连接、auth 登录被拒、host_key 主机指纹、remote 远端脚本、timeout 超时、internal 中控自身
alter table job_step add column error_kind text
    check (error_kind in ('network', 'auth', 'host_key', 'remote', 'timeout', 'internal'));

-- 清理任务按结束时间扫描
create index job_finished_idx on job (finished_at) where state in ('succeeded', 'cancelled', 'failed');
