-- 0109：按供应商面板校准已用重新启用（0040 停用过）。面板读数和中控实测对不上时（计量网卡 / 规则确认没配错之后），
-- PUT /machines/{id}/subscription 带 traffic_used_gb 录「此刻面板上本周期已用」：traffic_used_base = round(GB × 1e9)、
-- traffic_used_base_at = now()；null 清掉，不带不动。读路径（views.rs）：月度周期 used = 读数 + 录入之后的实测，下次重置后失效；
-- 滚动 30 天的读数是录入时刻往前 30 天的量，按均匀分布逐日线性衰减（quota::calibrated），30 天后完全失效。
-- 只改列注释，不改表结构。可重复执行。
comment on column subscription.traffic_used_base is '按供应商面板校准：录入时刻面板上本周期已用（字节，十进制 GB × 1e9）。月度周期到下次重置失效；滚动 30 天逐日线性衰减，30 天后失效';
comment on column subscription.traffic_used_base_at is '按面板校准的录入时刻（与 traffic_used_base 同为 null 或同不为 null）';
