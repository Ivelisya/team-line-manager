-- 0034：操作记录的关键词搜索只搜界面上看得到的内容。原来每行展开整个 detail（jsonb_path_query 'strict $.**'）逐值匹配：
-- 既慢（整表扫、每行都要展开），又会命中摘要里看不到的值（如 detail 里记着、界面不展示的团队名）。
-- 新列 search 存这一条的标题与摘要（中控写审计时按 api::misc::audit_text 生成，没有 exec 权限的人看到的那份：命令已隐藏），
-- 旧记录为 null，由中控启动时回填（db::backfill_audit_search）。可重复执行。

alter table audit_log add column if not exists search text;
