-- 0089：临时切换到时自动切回，原出口还没恢复时推迟（routers/switch.rs due）：back_at 往后推 1 小时，这里记为什么推迟、从什么时候起在等。
-- 都为空 = 没推迟过；再切一次（新的 back_at）或切回时清掉。
alter table router_switch add column if not exists defer_why text, add column if not exists deferred_at timestamptz;
