-- 0045：调优档 tune-v2 起缓冲区上限按机器推导（jobs/tune.rs）。tune_params 记最近一次成功调优用的推导参数与结果，
-- 网页、fleetctl show 据此显示：{"bw","bw_plan","bw_source":"plan|default","rtt","rtt_source":"sampled|default","rtt_samples","mem_mb","buf_max"}。
-- null = 没调优过、tune-v1（固定值，没有推导参数）或已撤销调优。
alter table machine add column if not exists tune_params jsonb;
