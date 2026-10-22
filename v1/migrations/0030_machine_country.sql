-- 0030：机器的国家（ISO 3166-1 两位代码），网页在名字旁显示国旗。上架、修改时可以手动指定（接口按 ISO 表校验）；
-- 为空 = 按机器地址查离线 IP 库自动定（不落库，库更新后跟着变）
alter table machine add column country text check (country ~ '^[A-Z]{2}$');
