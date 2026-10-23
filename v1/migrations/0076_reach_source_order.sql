-- 0076：主探测来源从「一个探测点（或中控）」改成有顺序的探测来源列表（app_setting reach_source）。可重复执行。
-- {"primary": <探测点 id | null>, "central": bool} → {"order": [<探测点 id>…], "central": bool}：按顺序用第一个能用的探测点，
-- 都用不了时中控的检查开着就由中控代测（见 probe_points/ 的头注释）。原来的主来源成为列表里唯一的一项，主来源是中控的为空列表。
update app_setting
set value = jsonb_build_object(
        'order', case when value->>'primary' ~ '^[0-9a-f-]{36}$' then jsonb_build_array(value->>'primary') else '[]'::jsonb end,
        'central', coalesce(value->'central', 'true'::jsonb))
where key = 'reach_source' and not value ? 'order';
