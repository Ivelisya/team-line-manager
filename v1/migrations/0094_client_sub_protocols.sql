-- 0094：客户端订阅按协议出条目（线路机入口加了 AnyTLS，见 fleet-agent 的 singbox.rs）。
-- reality（默认，与之前一样）/ anytls / both。AnyTLS 条目只在线路机报了 sing-box 在跑、入口有 AnyTLS 端口时才出（entries::anytls_live）：
-- 选 anytls 时，sing-box 没在跑的线路机上的入口不出现在订阅里
alter table client_sub add column if not exists protocols text not null default 'reality';
alter table client_sub drop constraint if exists client_sub_protocols_check;
alter table client_sub add constraint client_sub_protocols_check check (protocols in ('reality', 'anytls', 'both'));
