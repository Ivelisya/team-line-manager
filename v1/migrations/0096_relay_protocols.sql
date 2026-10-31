-- 0096：线路机的入口协议。both（默认，同以前：Reality 与 AnyTLS 都开）/ reality（不下发 AnyTLS）/ anytls（只留 AnyTLS：
-- Reality 入站改听 127.0.0.1，分享链接、订阅只给 AnyTLS，探测点改测 AnyTLS 端口）。改成 anytls 的校验见 api/relays.rs。可重复执行
alter table relay add column if not exists protocols text not null default 'both';
alter table relay drop constraint if exists relay_protocols_check;
alter table relay add constraint relay_protocols_check check (protocols in ('both', 'reality', 'anytls'));
