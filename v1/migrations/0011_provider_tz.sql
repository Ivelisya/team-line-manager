-- 0011：供应商时区以前是自由文本，上架时会被抄进账单时区；按 check_tz 同一标准（at time zone 认不认）把写错的改回列默认值 UTC
do $$
declare r record;
begin
    for r in select distinct timezone from provider loop
        begin
            perform now() at time zone r.timezone;
        exception when others then
            update provider set timezone = 'UTC' where timezone = r.timezone;
        end;
    end loop;
end
$$;
