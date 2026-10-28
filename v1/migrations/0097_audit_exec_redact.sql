-- P-21：执行命令的审计不再留命令原文（与 SSH 登录「内容不记录」一致）。存量 exec 记录：先记原文 sha256，再按
-- api::audit::REDACT_RULES 同一组规则（同顺序、不区分大小写）把凭据换成 ***；search 里的摘要同样打码
create function pg_temp.redact(s text) returns text language sql immutable as $$
  select regexp_replace(regexp_replace(regexp_replace(regexp_replace(regexp_replace(s,
    '([a-z0-9_.-]*(pass|pwd|secret|token|key|auth|credential)[a-z0-9_.-]*=)("[^"]*"|''[^'']*''|[^[:space:];&|"'']+)', '\1***', 'gi'),
    '(--?(password|passwd|pass|token|secret|api-key|apikey)[[:space:]]+)("[^"]*"|''[^'']*''|[^[:space:];&|"'']+)', '\1***', 'gi'),
    '(authorization:[[:space:]]*((bearer|basic|token)[[:space:]]+)?)[^[:space:]"'']+', '\1***', 'gi'),
    '(://[^:/@[:space:]]+:)[^@/[:space:]]+@', '\1***@', 'gi'),
    '[a-z0-9+/=_-]{40,}', '***', 'gi')
$$;

update audit_log
   set detail = detail || jsonb_build_object(
         'command', pg_temp.redact(detail->>'command'),
         'command_sha256', encode(sha256(convert_to(detail->>'command', 'UTF8')), 'hex')),
       search = pg_temp.redact(search)
 where action = 'exec' and jsonb_typeof(detail->'command') = 'string' and not detail ? 'command_sha256';
