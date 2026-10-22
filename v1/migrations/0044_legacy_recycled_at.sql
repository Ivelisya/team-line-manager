-- 0044：可重复执行。旧面板导入回收站的行（source = legacy），回收时刻改成它的退役时刻。
-- 旧面板没有「回收」这一步，退役就是终点；导入时 recycled_at 取了默认的 now()，网页、fleetctl 把导入那天当成回收日，
-- 按回收时间排序时导入的一批全排在最前。导入接口已改为 coalesce(retired_at, now())（api/recycle.rs），这里修正已经导入的。
-- 没有退役时刻的保持导入时刻。生产还没导入过，是空操作。
update recycle_bin set recycled_at = retired_at where source = 'legacy' and retired_at is not null and recycled_at <> retired_at;
