# 团队线路管理 v1 — 设计

干净重写。不兼容旧系统的任何接口、库表、协议和命名；旧代码已删除（历史在 `main` 分支的初始提交里）。

## 1. 设计原则

1. **机器有稳定 ID，名字只是显示。** 一切（令牌、上报、入口、告警、历史、账单）都挂在 `machine.id`（UUIDv7）上。改名、改编号、换团队只改显示字段，不动任何身份。
2. **一份数据。** 只有 PostgreSQL 一个库；没有目录文件、没有第二套资产表。
3. **声明式下发。** 中控保存每台线路机"应该有哪些入口"（期望状态 + 代数 generation），线路机 agent 自己对齐 Xray 并回报已应用的代数。删掉一条入口 = 从期望状态里去掉它；线路机上不会留下孤儿入站、出站和规则。
4. **一切变更都是作业（job）。** 上架、退役、换线路机、轮换凭据、重新编号，都是可续跑、幂等、留日志的作业；界面上只有一次确认。
5. **生命周期是一等公民。** 机器、入口都有状态机；非 `active` 的对象自动退出监控、告警、统计和费用汇总，历史保留。
6. **资源自动回收。** 端口、令牌、主机记录、探测目标随生命周期释放，不需要人记得去删。

## 2. 领域模型

```
team ─┐
      ├─< machine (role=node|relay) ── subscription ── plan ── provider
      │      │  lifecycle: draft → provisioning → active → retiring → retired
      │      ├─< agent_token        （绑定 machine_id，退役时自动撤销）
      │      ├─< metric_* / probe_* （按天分区，过期整区删除）
      │      └─< incident
relay(machine) ─< entry >─ node(machine)
                   lifecycle: pending → active ⇄ disabled → removing → removed
                   port 在 (relay_id) 内唯一（仅限未 removed 的入口）
job ─< job_step ─< job_log      audit_log      user / session / api_key
```

- **machine**：`id, role, team_id, number, label, primary_ip (inet), ssh_port, provider_id, lifecycle, lifecycle_reason, created_at, activated_at, retired_at`。
  - 显示名 = `team.name + "-" + 两位编号`（如 `团队1-06`），**每次读取时计算**，不存储。
  - `(team_id, number)` 在未退役的节点中唯一；`primary_ip` 在未退役的机器中唯一（查重键）。
  - 编号保持连续：节点退役（或上架取消）后，该团队剩下的节点自动压缩成 01..N；新上架接在末尾（N+1），也可手动指定空号。每次重编都写审计（旧名 → 新名）。身份是 machine.id，重编号不换令牌、不断连接，线路机备注自动同步。
- **entry**：线路机上的一个入口（VLESS REALITY 入站 → SS 出站到节点）。`id, relay_id, node_id, port, client_uuid, short_id, lifecycle, applied_generation`。备注由节点显示名生成，改名后随期望状态自动同步。
- **subscription**：机器的账单。`plan_id, price_cents, currency, cycle, due_date, billing_tz, auto_renew, remind_days`；续费记录单独一张表。
- **job**：`kind (provision_node | provision_relay | retire | move_entry | rotate_entry | renumber | purge), target, params, state, steps[]`。每一步记录开始、结束、输出、错误类别；失败可从该步重试，取消会执行已完成步骤的补偿。

## 3. 关键流程

**上架节点（一次确认）**：填 IP、SSH 端口、登录方式、供应商/套餐、团队（编号自动）、目标线路机 →
预检（连通、指纹、系统）→ 基线（sshd、nftables、sysctl/BBR）→ 装 ssserver → 装 fleet-agent 并签发令牌 → 等第一批上报 →
创建入口（写期望状态）→ 等线路机回报已应用 → 端到端验收 → `active`。批量上架就是多行同时提交。

**退役**：入口 → `removing`（期望状态去掉）→ 等线路机回报 → 撤销令牌 → （节点可达时）停掉节点上的 agent 和 ssserver → `retired`。
退役机器从首页、告警、统计、费用里消失，在"已退役"筛选里可查历史；可"恢复"或"彻底删除"（purge 级联删除遥测，保留审计）。

**改名 / 重新编号 / 换团队**：只改 `team_id/number`，同步作业刷新线路机备注；不换令牌、不断连接。"重新编号"先出预览（旧名 → 新名），确认后一次生效。

**换线路机**：新线路机建入口 → 验收 → 旧入口 removing；分享链接变化会在界面提示。

## 4. 边缘：fleet-agent（单一二进制，两种角色）

- 只主动连中控（HTTPS），不开任何入站管理端口。
- 上报：`POST /agent/v1/report`，批量、带单调序号，断线本地缓冲，中控按 `(machine_id, seq)` 去重。
- 下发（仅线路机）：`GET /agent/v1/desired?after=<generation>` 长轮询；拿到期望状态后对齐 Xray（API 热增删 + 落盘配置以便重启后一致），回报 `applied_generation` 与逐条结果。
- 身份：令牌只认 `machine_id`；令牌在退役作业里自动撤销。
- 节点角色：主机指标（CPU、内存、磁盘、网卡、负载、开机 ID）。线路机角色：再加 Xray 进程状态、每入口流量与连接数、到各节点的 TCP/出口探测、访问日志。

## 5. 存储（PostgreSQL）

- 业务表普通建表 + 外键 + 约束（唯一性在库里保证，不靠代码）。
- 时序表（主机指标、入口流量、探测、访问日志）按天原生分区，保留期到期直接 `DROP` 分区；小时汇总表长期保留。
- 迁移用 sqlx migrations，版本化。

## 6. 中控：fleet-server（Rust / axum / sqlx）

- REST `/api/v1/*` + SSE `/api/v1/stream`（资源变更推送，前端不轮询）。
- 作业执行器、告警引擎、续费提醒都在同一进程内，以 PG 为唯一状态（`SELECT … FOR UPDATE SKIP LOCKED` 领取任务）。
- 鉴权：账号密码（argon2）+ 可选 TOTP，会话 Cookie；API 密钥带作用域；所有写操作入审计。

## 7. 前端（React + TypeScript + Vite）

- 保留原版卡片外观与配色（设计令牌移植），组件统一：一种卡片（固定分段槽位，用 subgrid 对齐同行卡片）、一种表格（固定列宽）、一种段落头、一种状态标。
- 数据层 TanStack Query + SSE 失效刷新。

## 8. 实验室（PVE）

| CT | IP | 用途 |
|---|---|---|
| 160 fleet-dev | 192.168.31.60 | 编译、fleet-server、PostgreSQL |
| 161 lab-relay1 | 192.168.31.61 | 线路机：Xray + fleet-agent |
| 163–165 lab-node1..3 | 192.168.31.63–65 | 节点：ssserver + fleet-agent |
