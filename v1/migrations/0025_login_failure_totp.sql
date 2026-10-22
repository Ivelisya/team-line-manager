-- 0025：两步验证码输错单独计数。知道密码的人从很多来源（各个 /64）猜 6 位验证码时，按来源的上限挡不住：
-- 同一用户名 1 小时内输错验证码超过 10 次，所有来源（含本人登录成功过的）一律先等，见 auth::blocked_secs
alter table login_failure add column wrong_code boolean not null default false;
