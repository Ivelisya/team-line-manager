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
      │      │  lifecycle: draft → provisioning → active → retiring → retired；provisioning ⇄ failed（上架作业失败；重试回到上架中）
      │      │  lifecycle: draft → provisioning → active → retiring → retired
      │      ├─< agent_token        （绑定 machine_id，退役时自动撤销）
      │      ├─< metric_* / probe_* （按天分区，过期整区删除）
      │      └─< incident
relay(machine) ─< entry >─ node(machine)
                   lifecycle: pending → active ⇄ disabled → removing → removed
                   port 在 (relay_id) 内唯一（仅限未 removed 的入口）
job ─< job_step ─< job_log      audit_log      user / session / api_key
```

- **machine**：`id, role, team_id, number, label, primary_ip (inet), ssh_port, provider_id, host_id, lifecycle, lifecycle_reason, created_at, activated_at, retired_at`。
  - 显示名 = `team.name + "-" + 两位编号`（如 `团队1-06`），**每次读取时计算**，不存储。
  - `(team_id, number)` 在未退役的节点中唯一；`primary_ip` 在未退役的机器中唯一（查重键）。
  - 编号保持连续：节点退役（或上架取消）后，该团队剩下的节点自动压缩成 01..N；新上架接在末尾（N+1），也可手动指定空号。每次重编都写审计（旧名 → 新名）。身份是 machine.id，重编号不换令牌、不断连接，线路机备注自动同步。
  - **同机 IP 节点**：一台落地机上配了几个公网 IP 时，每个附加的 IP 是一台独立的节点（`kind = vps`，自己的团队、编号、`primary_ip` = 它的出口 IP、`node_outbound`、入口），`host_id` 指向持有 SSH 与 agent 的那台落地机（宿主机，必须是在用、自身 `host_id` 为空的落地机）。
    它没有自己的 agent 和 SSH：作业、远程执行都登录宿主机，视图里的 agent 状态与主机读数借宿主机的（总览网速不重复算），agent 离线、时钟、持续高负载只由宿主机报，入口探测、无入口、续费照常自己报；宿主机维护时它也维护。
    `ssh_port` 存宿主机的（只展示），供应商取宿主机的、不带套餐、默认不记费用（可在它的「费用」里单独填）；宿主机的流量额度按它的网卡计量，同机节点的流量也走这块网卡（共用额度）；同机节点自己的「已用」是它入口的业务流量。
    节点在宿主机上是 ssserver 模板单元的一个实例 `fleet-ssserver@<SS 端口>`（端口从 8389 起由中控分配）：监听并从自己的 IP 出站（`outbound_bind_addr`），nft 表 `ss_guard_<端口>` 只放行它自己的线路机；宿主机原来的 `fleet-ssserver` / `ss_guard`（8388）原样不动。
- **entry**：线路机上的一个入口（VLESS REALITY 入站 → SS 出站到节点）。`id, relay_id, node_id, port, client_uuid, short_id, lifecycle, applied_generation`。备注由节点显示名生成，改名后随期望状态自动同步。
  直出入口：`node_id = relay_id`（节点就是线路机自己），freedom 出站，从线路机自己的 IP 出去；每台线路机至多一条（同一个 (node_id, relay_id) 唯一索引），备注「线路机名-直出」，随线路机退役摘除。
- **subscription**：机器的账单。`plan_id, price_cents, currency, cycle, due_date, billing_tz, auto_renew, remind_days`；续费记录单独一张表。
- **job**：`kind (provision_vps | provision_proxy | provision_relay | provision_ip | restore_* | attach_entry | move_entry | retire | purge | upgrade_agent), target, params, state, steps[]`。每一步记录开始、结束、输出、错误类别；失败可从该步重试（升级 agent 从推送重来），取消会执行已完成步骤的补偿。`params.after` 指向要等的作业：它结束前不领取，它在推送之后失败（看它的 `params.pushed`）或被取消就直接取消，推送之前就失败的不算（「全部升级 agent」的金丝雀与线路机逐台）；`params.rollout` 记发起时 dist 里 agent 的 sha256 与版本号，开始前与推送时核对，换了就停。

## 3. 关键流程

**上架节点（一次确认）**：填 IP、SSH 端口、登录方式、供应商/套餐、团队（编号自动）、目标线路机 →
预检（连通、指纹、系统）→ 基线（sshd、nftables、sysctl/BBR）→ 装 ssserver → 装 fleet-agent 并签发令牌 → 等第一批上报 →
创建入口（写期望状态）→ 等线路机回报已应用 → 端到端验收 → `active`。批量上架就是多行同时提交。

**上架同机 IP 节点**（`provision_ip`，填宿主机 + 新 IP）：在宿主机上核对 IP（要直接配在宿主机网卡上；经它访问中控 `/echo` 回显的必须是它自己，否则是出站路由不对；暂定的 SS 端口被占用就改用最小空闲端口）→
推 ssserver、装自己的实例和防火墙表（不做系统基线、不装 agent）→ 创建入口 → 等线路机应用 → 出口验收（期望它自己的 IP）→ `active`。恢复（`restore_ip`）同样的步骤，要求宿主机在用，SS 端口重新分配。

**退役**：入口 → `removing`（期望状态去掉）→ 等线路机回报 → 撤销令牌 → （节点可达时）停掉节点上的 agent 和 ssserver → `retired`。
退役机器从首页、告警、统计、费用里消失，在"已退役"筛选里可查历史；可"恢复"或"彻底删除"（purge 级联删除遥测，保留审计）。
退役宿主机时它上面的同机 IP 节点**一并退役**（同一事务里置为退役中、作废它们进行中的作业、关告警，各建一个只摘入口不登录清理的退役作业；宿主机的清理连同所有实例一起删）；单独退役同机 IP 节点只删它自己的实例。彻底删除宿主机要等它的同机 IP 节点（含已退役的）先删。

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
- 时序表（主机指标、入口流量、探测、访问日志）按天原生分区，保留期到期直接 `DROP` 分区；小时汇总表长期保留，访问监控的小时汇总只留 31 天（查询窗口最长 30d）。
  原始时序保留 `retention.raw_days`（默认 3 天：读原始点的曲线最多 24 小时，7d / 30d 走小时汇总，agent 离线队列 6 小时就淘汰），
  访问日志保留 `retention.access_days`（默认 7 天）；上报接收窗口、分区维护、彻底删除的时间窗都按这两个值推出。
- 迁移用 sqlx migrations，版本化。

## 6. 中控：fleet-server（Rust / axum / sqlx）

- REST `/api/v1/*` + WebSocket `/api/v1/stream`（资源变更通知 + 实时指标增量，前端不轮询）。
- 作业执行器、告警引擎、续费与流量额度提醒都在同一进程内，以 PG 为唯一状态（`SELECT … FOR UPDATE SKIP LOCKED` 领取任务）。
- 鉴权：账号密码（argon2），会话 Cookie；API 密钥带作用域；所有写操作入审计。

## 7. 前端（React 19 + TypeScript + Vite；shadcn/ui 主题 + 实时 WebSocket）

- 保留原版卡片外观与配色（设计令牌移植），组件统一：一种卡片（固定分段槽位，用 subgrid 对齐同行卡片）、一种表格（固定列宽）、一种段落头、一种状态标。
- 数据层 TanStack Query：变更通知让查询失效重取，实时指标直接写缓存。

## 8. 实验室（PVE）

| CT | IP | 用途 |
|---|---|---|
| 160 fleet-dev | 192.168.31.60 | 编译、fleet-server、PostgreSQL |
| 161 lab-relay1 | 192.168.31.61 | 线路机：Xray + fleet-agent |
| 163–165 lab-node1..3 | 192.168.31.63–65 | 节点：ssserver + fleet-agent |
