-- 旧链接没有密码，公开查询会拒绝，重新生成时写入新令牌和密码。
alter table customer_page add column if not exists password_enc bytea;
