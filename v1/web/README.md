# 团队线路管理 v1 · Web 前端

Vite + React 19 + TypeScript（strict）+ Tailwind CSS 3 + shadcn/ui（Radix 原语，组件在 `src/components/ui`；主题变量见 `styles/theme.css`）+ TanStack Query / Table + React Router 6 + Recharts + lucide-react。依赖版本全部精确锁定。

视觉沿用原版精修稿：token 移植自 `main:frontend/prototype/original/tokens.css`（默认粉紫，顶栏“配色”按钮可切回原版靛蓝；浅色 / 深色 / 跟随系统）。

## 运行

```bash
pnpm install
pnpm dev:mock      # 浏览器内 mock（= VITE_MOCK=1 pnpm dev），不需要后端，登录任意用户名密码
pnpm dev           # 连真实后端：/api 与 /echo 代理到 http://192.168.31.60:8080（见 vite.config.ts）
pnpm build         # tsc -b + vite build，输出 dist/
```

Mock 的客户页链接和计费台账保存在当前站点的浏览器 localStorage 中，支持新标签页打开与刷新；其他模拟数据仍在内存里。客户链接需在生成它的同一浏览器、同一地址（含主机名与端口）打开。仅用于本地演示。

生产环境用 `BrowserRouter`，后端静态托管需要把未知路径回退到 `index.html`。

## 检查

先开 `pnpm dev:mock`，再：

```bash
pnpm shots         # tools/shoot.mjs：各页面 1440 / 390 截图到 shots/ + 卡片分段对齐检查（输出 misaligned=0 才算通过）
pnpm shots:ui      # tools/shoot-ui.mjs：交互态截图（命令面板、悬浮卡、右键菜单、作业进度、设置页、通知、深色）；BASE=… OUT=<目录> node tools/shoot-ui.mjs [场景名片段]
pnpm smoke         # tools/smoke.mjs：退役确认流程、批量上架（粘贴 → 检查 → 唯一确认 → 作业）
```

`CLIP=x,y,w,h pnpm shots <场景名片段>` 只截局部。需要本机装有 Chrome 或 Edge（`CHROME` 环境变量可指定）。

## 结构

```
src/
  main.tsx, App.tsx          入口、路由（/ /nodes /relays /provision /jobs[/:id] /catalog /incidents /login）
  styles/                    tokens.css（原版 token）、theme.css（shadcn 主题变量 → token）、base（外壳）、components、cards（subgrid 卡片）、table、pages
  lib/                       api.ts（fetch 封装）、queries.ts（查询与写操作 hook）、stream.ts（实时流 WebSocket）、live.ts（metrics 写缓存 + 实时迹线 useLiveSeries）、
                             derive.ts（卡片/表格数据推导）、format.ts、sort.ts（默认排序）、drawer.ts（?m= 抽屉）、theme.ts
  components/ui/             shadcn 风格基础组件：button form select dialog(+Sheet) overlays(tabs/menu/tooltip/popover/Segmented) chart toast
  components/app/            唯一实现的业务组件：primitives（Pill/Chip/KpiTile/LabelValue/SectionHeader/Panel/Meter/Beat…）、
                             NodeCard、dialogs（ConfirmDialog / TypedConfirmDialog）、EntryActions、TimeChart
  features/                  NodeDrawer（6 个页签）、ManageTab、BillingTab、provision-model（上架行模型/粘贴解析）
  pages/                     Home Nodes Relays Provision Jobs Catalog Incidents Login
  mock/                      fixtures.ts（27 台机器，IP 仅用 203.0.113.0/24、198.51.100.0/24）、server.ts（fetch 拦截 + mock 实时流，有状态）
tools/                       shoot.mjs、smoke.mjs（puppeteer-core）
```

## 设计要点

- **卡片对齐**：`.cards` 网格 + 每张卡 `grid-row: span N; grid-template-rows: subgrid`。槽位固定（大 10 / 小 7 / 紧凑 6），无主机的家宽代理在“资源”槽显示同高占位、“网卡”槽换成业务流量表，同一行分段永远对齐。
- **台账与客户页排序**：节点按本期生效日期从早到晚，同日按节点编号排列；标记已支付后保持原位置。“待支付”标题后以灰色小字说明“按照节点生效日期收费”；操作记录每页 10 条，支持上一页 / 下一页。待支付摘要固定只显示有待支付节点的月份，不补入当前选中的零节点月份。
- **默认排序**：线路机 → 团队（sort_order，再名称）→ 编号升序（`lib/sort.ts`）；故障/离线不下沉；其它排序是用户在下拉/表头里的显式选择。
- 表格 `table-layout: fixed` + `colgroup`；数字 `tabular-nums`；长文字省略。
- 已退役机器不进首页与 KPI；机器页用“范围”开关查看，退役机器的管理页只有“彻底删除”。
- 写请求自动带 `X-Fleet-Request: 1`；实时流 WebSocket `/api/v1/stream`：资源变更按事件类型失效对应查询，`metrics` 增量直接写进缓存（不再定时整表重取）；流断开时各查询 30s 轮询兜底。

## 对 API.md 的假定（后端落地后请核对）

文档没写明形状的部分，前端按下面假定（集中在 `src/lib/types.ts`）：

- `GET /machines/{id}/series`：`{metric, range, points:[{ts, …}]}`，points 为扁平对象。host：`cpu_pct mem_pct disk_pct load1 net_rx_bps net_tx_bps`；traffic：`up down`（时间桶内字节数）；probe：`tcp_ms egress_ms`（失败为 null）。
- `GET /incidents`：`{id, subject:{id,name}|null, rule, severity, state, summary, started_at, resolved_at, acked_at, acked_by}`。
- `GET /silences`：`{id, subject_id, subject_name, rule, starts_at, ends_at, is_maintenance, comment}`；`GET /audit`：`{id, ts, actor, action, target_name, detail}`；`GET /machines/{id}/renewals`：`{id, old_due, new_due, amount_cents, note, created_by, created_at}`。
- `/jobs`：`state` 取 `queued|running|done|failed|canceled`，步骤 state 取 `pending|running|done|failed|skipped`。日志 level 取 `info|warn|error`。
- 入口 `lifecycle` 用 `disabled` 表示已停用。
- 机器没有地区信息，卡片标题的“国旗位”改为类型图标（线路机 / VPS 节点 / 家宽代理）。
- 流量额度“本周期已用” = 服务端 `quota.used`：有流量重置日的按账单时区里最近一次重置日起，否则本月；含已移除入口；“已上架”时长取 `activated_at`。
