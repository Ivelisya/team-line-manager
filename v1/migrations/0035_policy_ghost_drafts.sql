-- 0035：清掉规范化后与生效值相同的草稿（不用域名的模式不比域名，同 policy::Data::normalized）。
-- 中控读规则时把这类草稿当作没有，库里的 draft 列却还留着：发布只清有草稿的行，它们一直留着；丢弃按 draft is not null 清，会多清、多报。
-- 以后保存草稿、回滚都不再留下这类草稿。只清不算改动，revision 不变。可重复执行。
update policy set draft = null
where draft is not null
  and draft->>'mode' = mode
  and coalesce(draft->>'note', '') = note
  and (mode not in ('allowlist', 'denylist')
       or array(select jsonb_array_elements_text(coalesce(draft->'domains', '[]'))) = domains);
