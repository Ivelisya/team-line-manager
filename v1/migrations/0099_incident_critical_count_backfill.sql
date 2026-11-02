-- 0099：告警 detail 里的严重次数统一用 critical_count，去掉旧的 critical 标记与读取时的回退。可重复执行。
-- 以前没有 critical_count 的旧告警，读取时按「有 critical 标记或 severity = 'critical' 时算 1 + reopen_count 次」退回；这里把这个值冻结写进 critical_count
-- （之后复发重开由代码在它上面累加），并删掉 critical 标记。已有 critical_count 的保持原值，只删标记；第二次执行没有符合条件的行即无操作。
update incident
set detail = (detail - 'critical')
    || case
           when detail ? 'critical_count' then '{}'::jsonb
           else jsonb_build_object('critical_count', 1 + reopen_count)
       end
where detail ? 'critical' or (severity = 'critical' and not detail ? 'critical_count');
