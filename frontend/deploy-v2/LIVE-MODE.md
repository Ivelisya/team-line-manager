# v2 只读预览 · 实时模式（LIVE-MODE）

`/admin/v2/` 是新增的只读预览：精修原版界面（`prototype/original-refined/`）跑在生产 team-ops 的只读接口上，旧页面一个都不动。这份文档写清楚三件事：

1. **PHASE 1（已完成）**：`build.sh` 怎么把原型变成 dist，实时数据怎么接进去而不改页面代码，请求预算，CSP，冒烟结果。
2. **审计**：界面里每一个写操作、每一处演示/合成数据、每一个把数据拼进 HTML 的地方，在实时模式下分别怎么处理。
3. **PHASE 2 改动计划**：要改哪些文件的哪些函数，改动集中在一个新模块 `js/live-mode.js`（草稿在 `deploy/v2/phase2/live-mode.js`）。

行号以构建 `c1f010ea0f6d` 时的源码为准（另一条精修流水线还在改 `original-refined/`，行号可能漂移，函数名不会）。

**数字一览**：写操作 **39 个**（+ 退出登录 1 个会话操作），在 12 个接入点拦截（§10）；本地便利操作 **全部保留**；演示 / 合成数据来源 **16 处**，其中不点写操作也会出现的 **6 处**（#1 #2 #3 #11 #12 #15），#1 模拟器、#2 模拟时钟已在 PHASE 1 解决；深链 **34 条**（写操作 30 + 只读跳转 2 + 登录 / 退出 2）+ 侧栏原生页 **25 条**；XSS：**2 处必须修**、**~30 处依赖后端类型**（一行 `esc()` 即可）；CSP：dist 里 **0 处**违规。

---

## 1. PHASE 1：构建与接线

### 1.1 `build.sh` → `dist/`

```
dist/
  app/        original-refined 的 index.html、css/、js/、assets/（白名单；README、critique/、shots/、tools/、compare*、final/ 都不带）
              + js/theme-init.js（原 index.html 内联主题脚本，CSP 不允许内联）+ js/live-flag.js（TEAMOPS_LIVE = true）
  shared/     prototype/shared 去掉 *.test.mjs、smoke.html、README；index.js 是生成的包装（见 1.2），原桶文件改名 index.proto.js
  original/   tokens.css + fonts/inter-latin-wght.woff2      ← app/css/app.css 的 @import ../../original/tokens.css
  data/       plan-catalog.json                              ← app/js/plans.js 的 ../../data/plan-catalog.json
  live/       adapter.js、synth.js、boot.js                   ← shared/index.js 的 ../live/boot.js
```

- **可复现、幂等**：先 `rm -rf dist`，只按内容拷贝（不带 mtime、`.DS_Store`、`._*`），`<meta name="teamops-build">` 是全树内容哈希；连跑两次得到同一个 build id。
- **index.html 改写**（`tools/dist.mjs index`）：内联 `<script>` → `js/theme-init.js`（原样搬出，仍在 `<head>` 里、CSS 之前执行，不闪）；删掉 `../data/snapshot.json` 预加载；模块脚本前加 `js/live-flag.js`；加 build id。
- **检查**（`tools/dist.mjs check`，失败即退出）：
  - 每个相对 URL（静态 / 动态 import、`new URL(…, import.meta.url)`、CSS `@import` / `url()`、HTML src/href）都能在 dist 里找到：166 个，全部解析；
  - 没有带 `?` 的模块 URL，除生成的 `shared/index.js` 外没有模块直接引 `index.proto.js`：42 个模块，每个只有一个 URL，不会出现两份模块实例（store / 事件总线唯一）；
  - 没有 `snapshot|detail|extra.json`、`series/`、`fixtures/`（与 `deploy/prod/deploy.sh` 同一规则）、测试文件、smoke 页；
  - 没有真实 IP / UUID：把 `live/fixtures/` 里出现过的全部 IPv4 与 UUID 当黑名单扫 dist，另列出文档段（RFC 5737 / 2544）之外的全部 IPv4 字面量——结果为无；
  - CSP 隐患：内联脚本、内联事件属性（HTML 与模板字符串）、`eval` / `new Function`、外部源——结果为无。

### 1.2 实时接线：不改页面代码

页面只 `import … from '../../shared/index.js'`。dist 里这个文件是生成的：

```js
export * from './index.proto.js';                       // 原型桶文件，原样
import { loadSnapshot as protoLoadSnapshot } from './index.proto.js';
import { liveLoadSnapshot, liveCreateLive } from '../live/boot.js';
export const loadSnapshot = (opts) => liveLoadSnapshot(protoLoadSnapshot, opts);   // fetch: liveFetch
export const createLive = (store, opts) => liveCreateLive(store, opts);            // → createLivePoller
```

本地声明的导出优先于 `export *`，所以只替换这两个名字。shared 里没有模块反向 import `index.js`（`build.sh` 会检查），`data.js`、`live.js` 等仍是同一个 URL、同一份实例。`app.js` 的启动顺序本来就是 `loadSnapshot()` → `applyPlans()` → `createLive(store).start()` → `route()`，所以轮询器在任何 `store.detail()` / `store.series()` 调用之前就装好了（README §1 的前提）。

`live/boot.js` 另外做了三件数据层的事：

| 事项 | 做法 | PHASE 2 |
|---|---|---|
| 状态回放不能覆盖实时状态 | `store.replays()` 返回 `[]`，`applyReplay` / `clearReplay` 空操作 | `?demo=1` 条在实时模式下不渲染 |
| 401 | `liveLoadSnapshot` 把失败变成 `Error('请先登录管理员账户')`，带 `.auth`、`.link='/admin'`；轮询中 401 → `TEAMOPS.on('auth')` + `window` 事件 `teamops:auth` | `app.js` 启动失败分支与 `installLiveMode` 显示登录对话框 |
| 需要任务详情的路由 | `#/lines`、`#/onboarding` 进入时 `poller.need('jobs')` | 移到两个页面的 `mount` 里，删掉 boot.js 的 `JOB_ROUTES` |

`globalThis.TEAMOPS`：`{ live, readOnly, poller, source, authError, on('auth'|'error'|'recover'), assetOf(id) }`。`assetOf` 把节点 id 换成原版后台的 `asset`（Komari uuid，快照里不带），深链要用。

### 1.3 测试服与 CSP

`staging/stage_server.py` 对 `/admin/v2/*` 发与生产 Caddy 完全相同的头（`Content-Security-Policy` 逐字抄 `deploy/prod/deploy.sh`、`X-Frame-Options: SAMEORIGIN`、`Cache-Control: no-cache`），dist 布局下 `/admin/v2`、`/admin/v2/` 302 到 `/admin/v2/app/`，目录不带斜杠时补斜杠——与生产同形，冒烟能在测试服抓到 CSP 问题。

---

## 2. 只读策略

- **v2 永远不发写请求**。适配层只发 GET，对数据文件的非 GET 直接抛错；测试服对非 GET 返回 403；冒烟统计 `writes: 0`。
- **写操作按钮保留原样**（外观、位置、快捷键都不变，界面密度不丢），点了统一出一条 info toast：**“只读预览：请在原版后台操作（停用入口）”**，带链接按钮“打开原版”，新标签页打开对应的原版页面（§4 深链）。不弹确认框、不改内存、不出假的成功提示。
- **L1 / L2 确认框不再出现**：`confirmAction()` 在实时模式下直接返回 `false` 并出同一条提示（一处拦截覆盖全部 L1 / L2）；L0 操作（启用、备注、验证、确认故障、标记已查看、保存草稿……）在各页面的委托入口处用 `guard(key, ctx)` 拦一行。
- **本地便利照常**：复制（IP、凭据前缀、链接、日志）、导出 CSV / JSON、主题、密度、筛选、排序、分组、搜索、视图、命令面板跳转、快捷键、抽屉与页签、图表范围与表格视图、手动刷新（改为真的调 `live.refresh()`）。
- **演示数据不能冒充真实数据**：模拟器已换成轮询器；上架流水线只显示真实的接入任务（`/jobs`），演示批次整块不出现；其余演示内容只在点了已被拦截的操作后才会出现，因此一并消失（§5）。
- **退出登录**：原版顶栏就是 `GET /api/logout`，属于会话操作而不是数据写入，实时模式下照原版行为跳转（这是唯一放行的“动作”，如需也拦，改成 `guard('logout')` 一行）。

---

## 3. 写操作清单（39 个）

列说明：**现在** = 原型里的行为；**实时** = PHASE 2 后的行为（全部是“只读提示 + 打开原版”，只写差异）；**原版位置** 见 §4 的 URL 模板。L0 = 直接执行 + toast，L1 = 确认框，L2 = 输入名称确认。

### 3.1 外壳与命令面板（`app.js`）

| key | 标签 | 现在 | 实时 | 原版位置 |
|---|---|---|---|---|
| `palette.verify` | ⌘K“重新验证 X 的链路”（抽屉打开时，`app.js:333`） | L0，toast“…（原型演示）” | 提示 | 节点抽屉 actions |
| `palette.restartXray` | ⌘K“重启 X 的 Xray”（`:334`，danger 二次回车） | 只出 toast“会打开确认对话框（原型演示）” | 提示 | `/admin/lines` 线路机卡片 |
| （会话）`shell.logout` | 顶栏退出（`[data-top]` `:305`） | toast“原型不会真的退出登录” | `location.assign('/api/logout')`（同原版） | 顶栏 |

### 3.2 新增节点 / 线路机向导（`provision.js openProvisionWizard`，4 个入口汇到这一个函数）

| key | 入口 | 现在 | 实时 |
|---|---|---|---|
| `provision.node` | 运营总览“新增节点”（`dashboard.js:244` → `enroll-wizard.js`）、节点页“新增节点”（`nodes.js:323`，空态 `:264`）、线路页“新增家宽节点”（页头、每台线路机、空态，`lines.js:771`） | 3 步向导 → 模拟执行（`Math.random` 步骤耗时），任务进线路页“接入任务”带 `演示` | 不开向导，提示；原版 `/admin/lines`（新增家宽节点，可预选线路机）或 `/admin/dashboard`（新增节点） |
| `provision.relay` | 线路页“新增线路机”（`lines.js:772`） | 同上（11 步） | 提示，原版 `/admin/lines` |
| `provision.enrollExisting` | 向导里“登记已存在的入口”（`provision.js:291`） | info toast“原型未包含” | 不可达（向导不打开） |

### 3.3 线路页（`pages/lines.js`，委托入口 `:764–791`）

| key | 标签 | 现在 | 实时 |
|---|---|---|---|
| `lines.entry.disable` | 停用（行内 ⏻、展开行，`disableEntry :843`） | L1；改 `e.enable/active` 与节点 `connection`；toast“（原型演示）”+撤销 | `confirmAction` 拦截 → 提示；原版：绑定节点的抽屉 `tab=actions`，主节点 / 未绑定 → `/admin/lines` |
| `lines.entry.enable` | 启用（`enableEntry :854`） | L0；改内存；toast + 撤销 | 委托入口 `guard` |
| `lines.entry.reset` | 重置用量（`resetEntry :858`） | L2；累计清零 | `confirmAction` 拦截 |
| `lines.entry.remark` | 编辑备注 / 保存（`:780/:781`、Enter `:757`，`saveRemark :869`） | L0 行内编辑；改 `e.remark` | 点“编辑备注”即 `guard`（不进入编辑态） |
| `lines.entry.rotate` | 更换连接凭据（`rotateEntry :879`） | L2；**随机生成 UUID** 并显示“原型演示：随机生成” | `confirmAction` 拦截；随机凭据永不出现 |
| `lines.entry.verify` | 重新验证链路（`verifyEntry :896`） | L0；1.4 s 转圈后把当前值当“验证结果” | `guard`（原版是 `probe_now`，会真的发探测） |
| `lines.relay.restartXray` | 重启 Xray（`restartXray :907`） | L1；toast“已重启…（原型演示）” | `confirmAction` 拦截；原版 `/admin/lines` 卡片底部 |
| `lines.relay.retryPanel` | 面板不可达提示条里的“重试”（`:776`） | 900 ms 后 bad toast“（原型演示）” | 改为 `live.refresh()`（重读 overview，不是写）；不出假 toast |
| `lines.relay.logs` | 查看 Xray 日志（`xrayLogs :916`） | 对话框里是**由访问记录拼成的假日志** | `guard`：原版读日志是审计动作（`panel.xray_logs`，POST），只读预览不做；提示 + `/admin/lines` |

### 3.4 节点抽屉（`node-drawer.js onAction :486`，`body.onclick` `:366`）

| key | 页签 / 标签 | 现在 | 实时 |
|---|---|---|---|
| `drawer.disable` | 管理 · 停用入口（`:492`） | L1；toast“（原型演示，未改数据）” | `confirmAction` 拦截；原版 `/admin/nodes?asset=<uuid>&tab=actions` |
| `drawer.enable` | 管理 · 启用（`:495`） | L0 toast | `guard`（`onAction` 第一行统一拦） |
| `drawer.reset` | 管理 · 重置（`:497`） | L2 | 同上 |
| `drawer.remark` | 管理 · 编辑备注（`:508`，对话框） | 对话框“原型演示，不保存” | 不开对话框，提示 |
| `drawer.rotate` | 管理 · 更换（`:500`） | L2 | 提示 |
| `drawer.verify` | 管理 · 重新验证（`:503`） | L0，1.4 s 假结果 | 提示 |
| `drawer.monitor` | 管理 · 监控与维护 · 设置（`:508`） | 对话框（阈值 + 维护截止）→“已保存” | 提示；原版同在 `tab=actions`（维护是截止时间，不是开关） |
| `drawer.ackAll` | 管理 · 确认故障（`:518`） | L0，按钮变“已知悉”，不改数据 | 提示；原版 `tab=actions` |
| `drawer.ack` | 连接质量 · 故障行“标记已知悉”（`:227` → `:518`） | 同上 | 提示；原版 `tab=availability` |
| `drawer.editAsset` | 费用 · 编辑资产（`:330`） | 对话框 →“已保存” | 提示；原版 `tab=billing` |
| `drawer.renew` | 费用 · 登记续费（`:330`） | 对话框 →“已保存” | 提示；原版 `tab=billing`（不要链 Komari 主机统计的“我已续费”，它只改 Komari 的 expired_at） |

### 3.5 节点行为（`pages/behavior.js`，委托入口 `:642–662`）

| key | 标签 | 现在 | 实时 |
|---|---|---|---|
| `behavior.event.ack` | 行为变化 · 标记已查看（`ack :788`） | L0；改 `acked`、未读数；toast + 撤销 | `guard`；原版 `/admin/behavior?view=events&asset=<uuid>` |
| `behavior.rules.saveDraft` | 访问规则 · 保存草稿（`saveDraft :831`，表单 submit `:663`） | 草稿存模块级 Map | `guard`（原版草稿存服务端 `/policies/draft`）；原版 `?tab=rules&asset=<uuid>` |
| `behavior.rules.apply` | 预览影响对话框里的“应用预览中的配置”（`:866`） | 勾选后 info toast“原型不会真的应用” | 预览仍可打开（纯本地计算），“应用”按钮 `guard` |
| `behavior.rules.discard` | 丢弃全部草稿（`discardDrafts :874`） | L1 | `confirmAction` 拦截 |

### 3.6 上架流水线（`pages/onboarding.js`、`onboarding-drawer.js`、`onboarding-dialog.js`）

实时模式下演示批次整块不渲染（§5 #3），下面除“新建批次”外都不可达；仍逐个登记，免得以后接真批次时漏拦。

| key | 标签 | 现在 | 实时 |
|---|---|---|---|
| `onboarding.batch.new` | 新建批次（页头 `:49`、快捷键 `n` `:601` → `openNewBatch`） | 4 步向导 → 模型里开跑 | `openNewBatch` 第一行 `guard`；提示写“批量上架随新后台上线（阶段 4），单台请在原版线路页新增家宽节点”，链 `/admin/lines` |
| `onboarding.batch.pause` / `.resume` | 暂停批次 / 继续批次（`:538/:539`） | L0 + 撤销 | 不可达 |
| `onboarding.batch.cancelUnstarted` | 取消未开始的行（`:540`） | L1 | 不可达（`confirmAction` 也会拦） |
| `onboarding.row.retry` | 从这一步重试 / 重新盘点（`act :462`） | L0 | 不可达 |
| `onboarding.row.waitMore` | 再等 10 分钟（`:463`） | L0 + 撤销 | 不可达 |
| `onboarding.row.skip` | 跳过这一步（`:464`） | L0 | 不可达 |
| `onboarding.row.submitSecret` | 提交密码（`:555` / `act :467`） | L0；**是密码输入框** | 不可达；密码框不渲染 |
| `onboarding.row.changeIp` | 改 IP（`changeIpDialog :479`） | 对话框 | 不可达 |
| `onboarding.row.abandon` | 放弃此行（`:473`） | L2 | 不可达 |

计数：命令面板 2 + 向导 3 + 线路 9 + 抽屉 11 + 行为 4 + 上架 10 = **39**（另有退出登录）。`ACTIONS` 深链表有 30 个 key：上架流水线的 9 个行操作在实时模式下不可达，共用 `onboarding.batch.new` 的提示与链接；向导的 4 个入口共用 `provision.node`。

---

## 4. 深链：原版后台的真实地址

来源：`upstream-snapshot/komari-admin-live/assets/*v5.js` 的路由树与侧栏、`plugin-live/public/{app,production,interaction,behavior,navigation,apikeys}.js`、`server-config.txt`。`/admin/plugin-page?short=team-ops&file=…#…` 现在只是跳转到下面这些地址的旧链接，**不要用**；运营页也不能脱离外壳单独打开（`navigation.js:54` 会 `location.replace` 回 `/admin/<view>`）。

| 区域 | URL |
|---|---|
| 首页 / 运营总览 | `/admin/home` · `/admin/dashboard` |
| 节点列表 / 节点抽屉 | `/admin/nodes?view=all\|attention\|unknown\|due\|unbound&team=&search=` · `/admin/nodes?asset=<uuid>&tab=overview\|availability\|traffic\|billing\|activity\|actions` |
| 线路（含接入任务、新增向导、Xray 重启 / 日志） | `/admin/lines`；审计 `/admin/lines?tab=audit` |
| 节点行为 / 访问规则 / 访问记录 | `/admin/behavior?view=targets\|events\|sources\|nodes&team=&asset=&seconds=&search=` · `/admin/behavior?tab=rules&asset=<uuid>` · `/admin/activity?asset=&team=&seconds=&search=&host=&source_ip=` |
| API 密钥 | `/admin/api-keys` |
| 登录 / 退出 | 任意 `/admin/*` 未登录时显示登录卡（没有单独的登录路由）→ 链 `/admin` · `GET /api/logout` |

写操作 → 地址（`ACTIONS` 表，`asset` 由 `TEAMOPS.assetOf(node.id)` 给出）：

| 操作 | 原版地址 |
|---|---|
| 入口停用 / 启用 / 重置用量 / 更换凭据 / 编辑备注 / 重新验证 | 绑定节点 `/admin/nodes?asset=<uuid>&tab=actions`；线路机主节点、未绑定入口 → `/admin/lines`（原版不能深链到某一行入口） |
| 重启 Xray、Xray 日志、面板重试、新增家宽节点、新增线路机、新建批次 | `/admin/lines`（原版不能自动打开向导） |
| 监控与维护、确认故障 | `/admin/nodes?asset=<uuid>&tab=actions` |
| 故障行标记已知悉 | `/admin/nodes?asset=<uuid>&tab=availability` |
| 编辑资产、登记续费 | `/admin/nodes?asset=<uuid>&tab=billing` |
| 行为事件标记已查看 | `/admin/behavior?view=events&asset=<uuid>` |
| 访问规则保存 / 应用 / 丢弃 | `/admin/behavior?tab=rules&asset=<uuid>`（团队范围不能用 URL 指定，默认 `team:团队1`） |
| 查看访问记录（只读，v2 没有这一页） | `/admin/activity?asset=<uuid>` |

侧栏原生页（实时模式下侧栏这些项直接是链接，不再显示“原型未包含此页”）：主机配置 `/admin/servers`、主机统计 `/admin/metrics-overview`、延迟监测 `/admin/ping`、远程执行 `/admin/exec`、会话管理 `/admin/sessions`、API 密钥 `/admin/api-keys`、工作台 `/terminal`；通知 `/admin/notification/{channels,general,offline,load}`；主题 `/admin/themes`、`/admin/market/themes`；插件 `/admin/plugins`、`/admin/market/plugins`、`/admin/plugins/config`；系统 `/admin/settings/{site,general,sign-on,xtermjs,metrics}`、`/admin/logs`、`/admin/pprof`；账户 `/admin/account`、`/admin/about`。

原版深链的限制：抽屉开关与页签切换不写回 URL；同一 SPA 会话里再点一次同样的 hash 会被忽略——所以“打开原版”一律开新标签页（完整加载，可靠）。

---

## 5. 演示 / 合成数据（16 处）

| # | 内容 | 位置 | 触发 | 首屏自动出现？ | 实时模式 |
|---|---|---|---|---|---|
| 1 | **模拟器**：每 2 s 编造速率、主机、探测、计数，约 5 分钟一次随机故障（`live-*` 事件、假出口 IP `203.0.113.99`），并装上“快照 as_of + 流逝”的假时钟 | `app.js:396` `createLive` | 启动 | **是** | **PHASE 1 已替换**：`createLive` → 轮询器，时钟 = 服务器时间 + 本地流逝，`triggerEvent` 空操作 |
| 2 | 鲜度与数据源时间来自模拟时钟 | `app.js freshTick`、线路页面板时间 | 启动 | 是 | PHASE 1 起是真实的 `sources()`；间隔改用 `live.intervalS`（§7） |
| 3 | **上架演示批次** “09-23 新机 ×8”（团队1-37…44，198.51.100.201–208）+ 每秒推进的模拟引擎、合批倒计时 | `onboarding-model.js demoBatch/advanceBatch`、`onboarding.js setInterval(tick)` | 打开 `#/onboarding` | **是** | 实时快照 `demo_batch: null` → **PHASE 1 冒烟里这一页报错**（`renderAll` 读 `batch.id`）；PHASE 2：只显示“全部批次”里的真实任务，页头提示条（§7.4），批次卡 / KPI / 合批 / 矩阵不渲染，计时器不启动 |
| 4 | 步骤日志、输入输出、固定的 RTT / ASN / 规格 / 系统、操作者 `admin` | `onboarding-model.js script/logFor/ioFor/successDetail` | 打开演示批次的格子 | 否（URL `row=&step=`） | 随 #3 消失 |
| 5 | 旧任务日志“由各步耗时与错误整理” | `onboarding-model.js legacyLog`、`onboarding-drawer.js:278` | 打开旧任务 | 否 | 保留：数据是真实的步骤与错误，只是整理成日志形状；标题注明来源 |
| 6 | 新建批次的示例邮件 / CSV（`demo-root-*` 密码） | `onboarding-dialog.js:25–40` | “填入示例” | 否 | 不可达（向导被拦） |
| 7 | 向导的模拟执行、`DEMO_KEY` 公钥、结果文案、`演示` 任务行 | `provision.js`、`lines.js jobsList` | 开始接入 | 否 | 不可达（向导被拦）；`demoJobs` 永远为空 |
| 8 | 假 Xray 日志“由最近访问记录拼成” | `lines.js xrayLogs/logLinesFor` | 日志按钮 | 否 | 不可达（§3.3） |
| 9 | 更换凭据后的随机 UUID | `lines.js:886` | L2 确认后 | 否 | 不可达 |
| 10 | 假验证 / 假刷新：验证链路 1.4 s 后把当前值当结果；各页 ↻ 是假等待 | `node-drawer.js:503`、`lines.js:896/:776`、`dashboard.js manualRefresh`、`nodes/lines/behavior` refresh、抽屉 ↻ | 点击 | 否 | 验证被拦；↻ 改为 `await app.live.refresh()`（真的重读），转圈到完成 |
| 11 | `?demo=1` 演示条：回放 5 个历史时刻、触发一次状态变化；线路页演示条（面板不可达、Xray 停止…） | `app.js renderDemo`、`lines.js:559` | URL 带 `demo=1` | 是（仅该 URL） | PHASE 1：回放在数据层失效；PHASE 2：两处演示条不渲染 |
| 12 | 侧栏 20 多个原生页显示“原型未包含此页” | `app.js placeholderPage` | 点侧栏 | 是（点了就出现） | 侧栏项直接链接原版（§4） |
| 13 | 行为页范围提示“原型快照只有最近 24 小时”；规则预览“模拟阻止”列恒为 — | `behavior.js:421/:855` | 选 1 h / 7 d / 14 d；打开预览 | 否 | 文案改为“接口只提供最近 24 小时汇总”；“模拟阻止”列在实时模式下不显示 |
| 14 | “查看访问记录”说“访问记录页不在本轮原型” | `behavior.js moreMenu :676`、证据对话框 `:778/:781` | 点击 | 否 | 改为打开原版 `/admin/activity?asset=<uuid>` |
| 15 | 启动失败文案“读取 data/snapshot.json 失败…所有页面都依赖这份快照” | `app.js:389` | 接口失败 | 失败时 | “读取实时数据失败：<原因>”；401 走登录对话框 |
| 16 | 调试全局 `__app`、`app.__nodesExport` | `app.js:174`、`nodes.js:560` | — | 控制台 | 保留 `__app`（排障用，只读数据）；`__nodesExport` 去掉 |

另外两类“看上去像演示”的实时差异不是编造，照实显示：`ipintel` 与来源画像字段为 null（离线库只在 a395）→ 界面显示“—”；延迟心跳条从打开页面起逐格积累。

---

## 6. 请求预算（生产 team-ops `CPUQuota=50%`）

适配层改动（`live/adapter.js`，测试 `tests/budget.test.mjs`、工具 `tests/burst.mjs`）：

- **全局调度器**：所有请求（首屏、轮询、按需、后台）共用 `maxInflight = 3` 个槽，按优先级出队：核心 / 轮询 → 用户刚打开的（抽屉、图表）→ 后台；同优先级先来先走。
- **后台车道推迟到首屏之后**：`loadCore()` 不再启动车道；轮询器 `start()` 后 `lanesDelayMs = 2500` + `requestIdleCallback` 才开始。
- **只取看得见的**（新默认值）：
  - `trafficLane: 'needed'`：逐节点 24 h 流量只取**无主机的 8 台**（首页卡片“今日入口流量”弹层要它们的 24 h 峰值；有主机的节点弹层走 `host_24h`，悬停时才取）+ 打开过抽屉的节点；本月首小时只给打开过的节点取（按月缓存）；10 分钟刷新也只刷这些。
  - `jobsLane: 'needed'`：任务详情只在 `need('jobs')` 后取（线路页、上架流水线；永久缓存）。
  - `hostSpark: 'visible'`：主机迷你图种子只在 `need('sparks', ids)` 后取。**当前界面没有任何地方画 `host_spark`**，所以一个都不取（旧版每次首访 33 个、每个约 100 KB）。
  - 打开抽屉：`enrichNode` 顺带 `need('traffic', [id])`，抽屉“流量”页签的本月 / 近 24 小时 / 峰值立即有值。
- **缓存**：本月首小时（localStorage，按月）、已结束任务（localStorage，永久）、`maxAge` 短时复用、in-flight 去重，都保留。

| 首次访问（空缓存） | 旧版 | 现在 |
|---|---|---|
| 首屏前 | 10（10 个同时发出） | 10（同时最多 3 个，overview + health 先走） |
| 首屏后后台（首页） | +137：逐节点流量 41 + 本月首小时 41 + 任务详情 22 + 迷你图种子 33 | **+8**（无主机节点的 24 h 流量） |
| 首页合计 | 147 | **18** |
| 进线路页 / 上架流水线 | 同上 | +22（任务详情，只第一次；再访问 0） |
| 打开一个节点抽屉 | +7 | +9（多了该节点流量与本月首小时，原来这两项靠后台全量） |
| 每 10 分钟 | 41 | 8 + 打开过的节点 |
| 同时在途峰值 | 10（首屏）/ 4（车道） | **3** |

真实浏览器里（冒烟，a395 测试服，空 profile）：三种视口首屏到可交互 640–1120 ms，前 9 秒 **18 个** API 请求，峰值 **3**，写请求 **0**。每 10 s 的 `overview + health`（约 46 KB gzip）不变——`/overview` 每次在服务端重算，是 CPU 大头，只能靠新架构推送解决；轮询在标签页隐藏时暂停。

---

## 7. 顶栏徽标、鲜度、离线、登录失效（原版外观，只用现有组件）

### 7.1 顶栏徽标

`.topbar-right` 最前面放 `tonePill('只读预览 · 实时数据', 'info')`（现有 pill：20 高、10% 底、6 px 实心圆点、`--info` 色），悬停说明“数据来自运营服务的只读接口，每 10 秒刷新；写操作请在原版后台进行”。不新增颜色、字号、圆角。唯一新增 CSS 是窄屏规则（`shell.css` 视口 ≤760）：`.live-badge .pill` 只留“只读”两个字的宽度，与搜索按钮收成图标同一时机。连接状态不在徽标里重复，交给页头鲜度。

### 7.2 鲜度（`app.js freshTick`）

- `interval: live.intervalS ?? 2`（轮询器 10，模拟器 2）。按 `fmt.freshness` 规则：≤20 s“更新于 HH:MM:SS”，20–50 s“延迟 · N 秒前”，>50 s“已断开 · N 秒前”。PHASE 1 冒烟里页头会在两次轮询之间显示“延迟 · 6 秒前”，就是因为这里还写死 2。
- `age()` 从首次加载起算（PHASE 1 已改，之前首个 10 s 是 null → “已断开”）。
- 悬停说明加一行“轮询 每 10 秒 · 已请求 N 次”（`live.status().requests`）。

### 7.3 接口失败 / 离线

- 数据保留在 store 里不清空；`age()` 不再更新 → 鲜度按上面的阈值自然变成“延迟”再“已断开”；节点的 `staleOf()` 按服务器时钟（本地流逝）判断，超过 50 s 心跳格变空心、数值变“数据过期”灰——这些都是现有规则，不用新写。
- 连续失败 3 次（30 s）出一条 bad toast“实时数据读取失败：<原因>。页面保留最后一次的数据”（bad toast 不自动消失，符合 brief E2）；恢复时出 ok toast“实时数据已恢复”。事件来自 `TEAMOPS.on('error' | 'recover')`，由轮询器 `onError` / `onRecover` 发出。
- `freshTick` 的 `failed` 取 `live.status().failing >= 3`。

### 7.4 登录失效（401）

启动时 401：`app.js` 的启动失败分支看到 `e.auth` → 用 `openDialog` 出“请先登录管理员账户”（sm 对话框，正文“登录状态已失效，实时数据已停止刷新。登录后回到本页即可继续。”，底栏一个主按钮“去登录”→ `/admin`），内容区保留骨架。轮询中 401：轮询器自停，`TEAMOPS.on('auth')` 出同一个对话框，页面数据保留。

### 7.5 上架流水线的页头说明

`note({ tone: 'info', title: '批量上架随新后台上线', body: '新的 16 步上架流水线属于阶段 4（architecture/SUMMARY-for-user.md §5）。这里显示旧流程的真实接入任务；新增单台请在原版线路页操作。', action: 打开原版线路页 })`，下面是“全部批次”表（22 条真实任务，点开是旧任务抽屉：步骤、日志、输入输出）。

---

## 8. XSS：把数据拼进 HTML 的地方

界面把模板字符串写进 `innerHTML`。实时数据里很多字符串可被外部影响：Xray 访问日志里的目标域名 / SNI、来源 IP、客户端标识 / 邮箱、入口备注、资产备注、任务标题与步骤错误、故障原因、审计详情、标签、出站目标。

**转义工具**：`esc`（= `fmt.esc`，`format.js:776`）转 `& < > " '`，对文本和带引号的属性足够；对 `href` 协议、`style` 值不够。`num()`、`txt()`、`attrs()` 的值都经过 `esc`；tooltip（`data-tip`）用 `textContent` 显示，不是第二个注入点；`toast`、`confirmAction`、`pageError`、`empty`、命令面板、`logView`、来源画像、审计 JSON（`esc(JSON.stringify(…))`）都转义。**直接传 HTML 的参数**（调用方负责转义）：`openDialog` 的 body / foot、`openPopover` html、`note` body、`kv` / `dlRows` 的 value、`panel` 的 sub / tools / body、`dataTable` 的 `cell()` / `sub()` 输出、`timeline` 的 title / meta / desc。

逐项检查过、已转义的：Reality / SNI、访问目标、证据对话框、Xray 日志行、入口备注、审计的操作者 / 动作 / 对象 / 详情、全部 URL 参数回显（`q=`、`asset=`、筛选 chip）、供应商面板外链（限定 `^https?:`、`rel="noopener"`）。约 980 处安全插值（lines 118、node-drawer 112、behavior 106、onboarding 70、ui/* 约 250……）。

**必须修（PHASE 2）**

| 位置 | 字段 | 问题 |
|---|---|---|
| `onboarding-drawer.js:135`（文本在 `hitsText :182`） | 日志搜索词 `state.query` | 原样写进 innerHTML“没有找到“${q}””，每次模型变化重画；自我 XSS，但是真实注入点 → `esc(q)` |
| `onboarding-drawer.js:157` | 步骤 `estWord`（快照字符串）；`data-live="tl-${x.step}"` | 未转义的字符串数据 → `esc()` |

**依赖后端类型（后端恒给数字 / 干净 ID 时才安全；被攻破的线路机、3x-ui 面板或 Komari agent 可借此注入）** —— 一律包 `esc()` 或 `num(fmt.int(…))`：

| 位置 | 字段 | 上下文 |
|---|---|---|
| `node-drawer.js:513` | `n.limits.*` → `value="${v}"` | **属性**，可越界 |
| `node-drawer.js:467` | `h.cores`（Komari agent） | 文本 |
| `node-drawer.js:326` | `billing.renewal_reminder_days` | 文本 |
| `lines.js:289 / :420 / :416,418,421 / :210,370 / :478 / :546` | `e.port`、`panel.online_clients`、`config.runtime_rules`、`collector.event_gap`、`probe.assets`、`r.node_count`、`e.source_ip_count`、`j.ok_steps + 1`、`collector.running/waiting` | 文本（线路机 / 面板来源） |
| `lines.js:406,425,428,435,439,445 / :467,481,482 / :532` | `r.id`、`j.id`、审计 `x.id` 进 `id=` / `aria-controls=` / `data-*=` | 属性 |
| `behavior.js:122 / :340–341` | `data-ago="${t}"`、`data-ack="${r.id}"` | 属性 |
| `onboarding.js:78,132,127,142,317`、`onboarding-drawer.js:275,284` | 批次选项、`batch_no`、计数 | 文本 |
| `ui/viz.js:21` | `kpiTile` 的 `action.href`（只 `esc`，`javascript:` 能过） | href；现有 3 个调用方都是固定 `#/nodes?…` → 加 `^#/` 断言 |
| `ui/dom.js:124` | `flag(region.code)` 进 `img src` 路径 | 同源图片，无脚本执行 → 限定 `^[A-Z]{2}$` |
| `ui/forms.js:19,32` | `esc(width)` 进 `style=` | CSS 注入 → 只收数字 |

**其它**：`nodes.js exportCsv :563` 没防公式注入（单元格以 `= + - @` 开头会在 Excel 里执行）→ 前置 `'`；`app.js:319–320` 拼 `#/lines?relay=${id}` / `entry=${id}` 没 `encodeURIComponent`；`lines.js:601…`、`behavior.js:800`、`onboarding-dialog.js:341` 把原始 ID 放进 `querySelector`（异常 ID 会抛错中断更新）→ `CSS.escape`。

**CSP**（生产头：`default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; …`）：原型唯一的问题是 `index.html` 的内联主题脚本，PHASE 1 构建时已搬成 `js/theme-init.js`；没有内联事件属性、`eval`、`new Function`、字符串定时器、`javascript:`、外部源；uPlot 没有 eval；约 73 处 `style="…"` 属性靠 `'unsafe-inline'`（style-src）放行——生产头里有，将来若收紧 style-src，这些会失效（进度线宽度、骨架尺寸、图表坐标轴位置）。冒烟：**CSP 违规 0**。

---

## 9. PHASE 1 冒烟结果

`tools/smoke.mjs`（无头 Chrome，经 SSH 隧道访问 a395 测试服，测试服发生产同款 CSP）：3 种视口（1440×900 浅色、1440×900 深色、390×844）× 8 个视图（6 个页面 + 节点抽屉 + 一个原生占位页），截图 24 张在 `shots/phase1/`（含真实数据，只留本机）。

| 项 | 结果 |
|---|---|
| 实时数据 | 每个视图 `nodes=41`、`provenance.source=live`、轮询器在跑；首页 KPI“探针在线 41 / 41”、月费 $1,281.35；抽屉“本月 / 近 24 小时”有值（按需流量） |
| 控制台错误 / 异常 | 21 个视图 0；**上架流水线 3 个视图各 2 条**（`renderAll` 读 `batch.id`：实时快照没有演示批次，§5 #3，PHASE 2 修） |
| 失败请求 | 0（`snapshot.json` 不再请求） |
| CSP 违规 | **0** |
| 写请求 | 0 |
| 首次访问 | 18 个 API 请求、峰值 3 个在途；走完全部页面 + 一个抽屉 + 一次轮询后共 61 个 |
| 已知的 PHASE 2 项 | 鲜度在两次轮询之间显示“延迟”（`freshTick` 写死 interval 2）；原生页显示“原型未包含此页”；写操作仍是原型行为（本预览不会发请求，但会出“原型演示”toast） |

---

## 10. PHASE 2 改动计划（集中、每处几行）

新模块一个，其余每个文件是 1–5 行的接入点。草稿：`deploy/v2/phase2/live-mode.js`（`isLive`、`isReadOnly`、`ACTIONS` 深链表、`READ_LINKS`、`NATIVE`、`oldUrl`、`readOnlyNotice`、`guard`、`liveBadge`、`showAuth`、`installLiveMode`）。原型模式下全部是空操作，原型的截图、审计、交互脚本不受影响。

| # | 文件 · 函数 | 改动 |
|---|---|---|
| 1 | **新建** `js/live-mode.js` | 拷草稿；`components.js` 不需要 re-export 它（页面直接 import） |
| 2 | `ui/layers.js` · `confirmAction` (`:166`) | 参数加 `action`（key）、`ctx`；开头 `if (isReadOnly()) { readOnlyNotice(action ?? 'generic', ctx); return Promise.resolve(false); }` —— 一处覆盖全部 L1 / L2 |
| 3 | `ui/layers.js` · `toast` (`:193`) | 安全网：实时模式下 message 含“原型”→ `console.warn` 并改出 `readOnlyNotice('generic')`（任何漏拦的假成功提示都不会出现） |
| 4 | `app.js` · `shellHtml` | `.topbar-right` 最前加 `${liveBadge()}`；NAV 里 `NATIVE` 有的项在实时模式下 `href` 换成原版地址（`<a>` 本来就是链接） |
| 5 | `app.js` · `[data-top]` 委托 (`:305`) | logout：实时模式 `location.assign(OLD.logout)` |
| 6 | `app.js` · 命令面板 `act:verify` / `act:restart` (`:333–334`) | `run: () => guard('palette.verify', { node: cur }) \|\| …` |
| 7 | `app.js` · `renderDemo` (`:256`) | `if (isLive()) { box.hidden = true; return; }` |
| 8 | `app.js` · `freshTick` (`:375`) | `interval: live.intervalS ?? 2`、`failed: (live.status?.().failing ?? 0) >= 3`、tip 加请求数 |
| 9 | `app.js` · `boot` (`:386`) | catch：`e.auth` → `showAuth(e)`；其余文案“读取实时数据失败”；`live = createLive(store).start()` 之后 `installLiveMode(app)` |
| 10 | `provision.js` · `openProvisionWizard` (`:41`) | 第一行 `if (guard(kind === 'relay' ? 'provision.relay' : 'provision.node', { relayId })) return;` —— 覆盖运营总览、节点页、线路页 4 个入口 |
| 11 | `onboarding-dialog.js` · `openNewBatch` (`:173`) | 第一行 `if (guard('onboarding.batch.new')) return;` |
| 12 | `node-drawer.js` · `onAction` (`:486`) | 取到 `act` 后一行：`if (DRAWER_WRITES.has(act) && guard('drawer.' + act, { node: n })) return;`（L1 / L2 已被 #2 拦，这一行管 L0 与对话框类） |
| 13 | `pages/lines.js` · `[data-act]` 委托 (`:764`) | 一行 `if (LINES_WRITES[act] && guard(LINES_WRITES[act], { entry, store })) return;`；`retry-panel` 与 ↻ 改 `await app.live.refresh()`；`[data-demo]` 条在实时模式下不渲染；`mount` 里 `app.live.need?.('jobs')` |
| 14 | `pages/behavior.js` · 委托 (`:642`)、表单 submit (`:663`)、预览对话框“应用”(`:866`) | 同上一行 `guard`；`moreMenu` 与证据对话框的“查看访问记录”→ `READ_LINKS.activity`；`:421` 文案；预览里“模拟阻止”列实时模式隐藏 |
| 15 | `pages/onboarding.js` · `mount` | `const live = isLive() && !store.onboarding().batch;` 为真时只渲染页头（去掉暂停 / 取消）+ §7.5 提示条 + “全部批次”（真实任务）；不启动 `setInterval(tick)`；`app.live.need?.('jobs')`，`store.on('change')` 里 `diff.fleet.onboarding` 时重画列表 |
| 16 | `pages/dashboard.js` `manualRefresh` (`:272`)、`nodes.js` / `behavior.js` refresh、抽屉 ↻ | 假等待 → `await app.live.refresh?.()` |
| 17 | XSS（§8） | 2 处必须修 + 依赖类型的约 30 处包 `esc` / `num`；`kpiTile` href 断言；`flag()` 代码校验；`forms.js` 宽度只收数字；CSV 公式注入；`encodeURIComponent` 与 `CSS.escape` |
| 18 | `css/shell.css` | `.live-badge` 的窄屏规则（一条） |
| 19 | `deploy/v2/live/boot.js` | 页面自己 `need('jobs')` 后删掉 `JOB_ROUTES` |

验收（PHASE 2 完成后）：`build.sh` → `staging/run.sh` → `tools/smoke.mjs` 全绿（含上架流水线）；再补一个交互冒烟：逐个点 `ACTIONS` 里的入口，断言出现“只读预览：请在原版后台操作”、没有确认框、没有非 GET 请求、store 未变；`AUTH_TOKEN=x staging/run.sh` 跑一次 401 看登录对话框；停掉测试服看“已断开”与失败 toast。

发布仍用 `deploy/prod/deploy.sh deploy/v2/dist`（不在本阶段执行）。

---

## 11. PHASE 2 完成情况（构建 `5a1ebfe40a47`）

§10 的计划已全部落到 `prototype/original-refined/`（原型模式行为不变：无徽标、`?demo=1` 条照常、写操作照旧弹确认框，已用无头 Chrome 核对）。与计划的差异：

- `js/live-mode.js` 直接从 `ui/layers.js`、`ui/controls.js`、`ui/dom.js` 导入（`layers.js` 反向引用它做 `confirmAction` / `toast` 拦截，循环只在调用时发生）；`deploy/v2/phase2/live-mode.js` 是当时的草稿，以 app 里的为准。
- 拦截点：`confirmAction`（新参数 `action` / `ctx`，`local: true` 用于“放弃已填写的内容？”）；`toast` 的“原型”安全网；`openProvisionWizard`、`openNewBatch` 入口；线路页 `WRITES` 表、抽屉 `DRAWER_WRITES` 表、行为页标记已查看 / 保存 / 丢弃 / 应用；命令面板两项；上架流水线批次与行操作（实时模式下不可达，仍有拦截）。
- 各页 ↻、线路页“面板重试”在实时模式下调用 `app.live.refresh()`；行为页“查看访问记录”打开原版 `/admin/activity`。
- 上架流水线实时模式：提示条“批量上架随新后台上线”+“打开原版线路页”，下面是旧流程真实任务（22 条），任务详情到了自动重画；不启动模拟计时器。线路页“接入任务”同样随任务详情更新。`boot.js` 的按路由取任务详情已删除，改由两页 `app.live.need('jobs')`。
- XSS：§8 两处必须修 + 全部“依赖后端类型”的插值已包 `esc()`；选择器改 `CSS.escape`；`kpiTile` href 限 `#/`；`flag()` 限两位国家代码；表单宽度只收数字单位；CSV 公式前置 `'`；命令面板 hash 参数转义。
- 401：启动时出登录对话框（`请先登录管理员账户` · 去登录 → `/admin`），页头鲜度显示“已断开”；`AUTH_TOKEN=… staging/run.sh` 实测通过（截图 `shots/phase2/auth-401-light-1440.png`）。

冒烟（`tools/smoke.mjs`，a395 测试服，生产同款 CSP，3 种视口 × 8 个视图 + 每种视口点 12 个写操作）：控制台错误 0、异常 0、失败请求 0、CSP 违规 0、非 GET 请求 0、写操作全部出“只读预览：请在原版后台操作”且无对话框（36/36）、上架流水线不再报错、鲜度在两次轮询之间保持“更新于”；首访 18 个请求、峰值 3。截图 `shots/phase2/`（含真实数据，只留本机）。

## 12. 生产验收后的修正（构建 `fb6791713f51`）

1. `egress_verified`：实际出口为空（超时、curl-28）时是 `null`（未知），不是 `false`（不符）；`live/synth.js` 与 `prototype/data/tools/build.py` 同改，测试 `adapter.test.mjs` 新增一项。抽屉出口 pill 在出口检查失败 / 超时时显示“超时”/“失败”（warn），不再是红色“不符”。
2. 抽屉“近 24 小时”在逐节点历史到达前显示“—”+ 骨架，不再是“0 B”；`enrichNode` 先发该节点的 `history?asset&hours=24`（核心优先级，排在 `/node`、`/behavior`、`/activity` 之前），到了立即补丁；流量页签在历史到达后重画一次。“本月不完整”的日期改用真实 `coverage_start`（原来写死 09-15）。测试 `budget.test.mjs` 新增一项（顺序 + 不重复请求）。
3. 运营总览“业务速率排行”始终 5 行（有速率的不足 5 台时用 0 速率节点按团队、编号补齐），列表最小高 5 行；冒烟里把有速率的节点从 5 台压到 2 台，面板高度不变。
4. 检查失败 / 超时时不显示原始毫秒：`node-model.js latencyF / probeFail`（按 `status.failed_checks` 的 tcp / egress / ingress，`reason: timeout` → “超时”，其余 → “失败”；出口不符仍显示真实耗时）。用在节点卡片、抽屉（概览、连接质量）、心跳条说明、节点页表格、线路页入口表、行为页节点概览。
5. 冒烟：行为变化列表为空时跳过“标记已查看”。

## 13. 写操作接回旧后台（构建开关 `WRITES=1`）

v2 取代旧界面时要能写。今晚线路机的 Xray 从 3x-ui 迁到新系统，所以只接**只动 team-ops 数据库**的操作（类 a）；经桥接 / 3x-ui / Xray / SSH / Komari 的操作（类 b）一律不接，点了提示“此操作随线路机 Xray 迁到新系统，改在新系统中进行”（没有“打开原版”链接：迁移后旧后台也做不了）。

- **开关**：`WRITES=1 deploy/v2/build.sh` 生成的 `app/js/live-flag.js` 里 `globalThis.TEAMOPS_WRITES = true`；默认构建是 `false`，即当前的只读预览（类 a 仍是“只读预览：请在原版后台操作 + 打开原版”）。开关打开后顶栏徽标变成“实时数据”。
- **请求**（与旧运营页 `app.js:16-28` 相同）：`POST /api/team-ops/<path>`，`credentials: same-origin`，头 `Content-Type: application/json`、`X-Ops-Request: 1`（插件代理 `script.js:100` 的防 CSRF 检查，另要求 `Sec-Fetch-Site: same-origin`，浏览器自动带）；成功 `{data}`，失败 `{error}` 原文进 bad toast（不自动消失）；401 → 登录对话框；成功后轮询器立即重读。`/assets/edit` 成功后插件代理自己把资产同步到 Komari（`script.js:116`），前端不用管。
- **代码**：`js/live-writes.js`（`postOps` / `runWrite`）、`js/live-actions.js`（表单与请求体，照旧运营页字段）、`js/live-mode.js` 的 `ACTIONS[key].cls / wired`、`guard()`、`isWired()`；`confirmAction` 与各页入口不变（已接上的 key 在开关打开时放行）。`live/boot.js` 提供 `TEAMOPS.asset(id)`（旧资产记录，含 `revision`）与 `TEAMOPS.nodeRecord(id)`（GET `/node`）。
- **测试服验证**：`WRITES=1 build.sh && STAGE_WRITES=1 staging/run.sh`（测试服只对类 a 路由放行 POST，同样检查 `X-Ops-Request`，写在每次启动新拷贝的库副本上），`tools/writes-smoke.mjs` 17/17：登记续费（到期日与 revision 在实时数据里变化）、编辑资产、过期 revision → 400 原文、前端校验不发请求、监控阈值写入后 `limits` 随之变化、类 b 只提示不发请求；故障已知悉与行为已查看两个接口另经 `postOps` 直调验证（测试服库里没有未处理的行）。

| # | 操作（v2 入口） | 旧后台接口与请求体 | 类 | v2 |
|---|---|---|---|---|
| 1 | 编辑资产（抽屉 · 费用） | `/assets/edit` `{uuid, expected_revision, name, price:"12.50", due_date, note, renewal:false, renewal_note:""}` → 更新后的资产；revision 不符 400 | a | **接上**：表单 名称 · 月付金额 · 到期日 · 备注，按钮“保存并同步”，toast `已保存 · 名称` |
| 2 | 登记续费（抽屉 · 费用） | `/assets/edit` 同上，`renewal:true, renewal_note`；新到期日须更晚 | a | **接上**：默认下个月同日，toast `已登记续费 · 名称` |
| 3 | 监控与维护（抽屉 · 管理） | `/node/profile` `{uuid, expected_revision, profile:{thresholds:{cpu_percent, memory_percent, disk_percent, latency_ms}, maintenance_until}}`（0 = 结束维护，≤ 7 天） | a | **接上**：四个阈值 + 本地时区维护截止，toast `监控设置已保存` |
| 4 | 确认故障（抽屉 · 管理） | `/incidents/ack` `{id, note:""}`，对该节点每条未确认的未恢复故障 | a | **接上**：toast `已标记知悉；恢复仍由真实测量确认` |
| 5 | 标记已知悉（抽屉 · 连接质量故障行） | 同上，单条 | a | **接上** |
| 6 | 标记已查看（节点行为 · 行为变化） | `/behavior/ack` `{id}` | a | **接上**：只把这一行置为已查看（旧页同样不弹提示） |
| 7 | 保存草稿（访问规则） | `/policies/draft` `{scope, expected_revision, policy:{mode, domains, note}}` | a | 不接：草稿只有“应用”后才有意义，应用是类 b；整块规则随新系统 |
| 8 | 丢弃全部草稿 | `/policies/discard` `{draft_hash}` | a | 不接（同上） |
| 9 | 应用访问规则 | `/policies/apply` → 桥接改 3x-ui 模板 + `xray api adrules` | b | 迁到新系统 |
| 10–15 | 入口 停用 / 启用 / 重置用量 / 编辑备注 / 更换连接凭据 / 重新验证链路（线路页 6 个） | `/panel/action` `{relay, action: inbound_enable / client_reset / inbound_reset / inbound_remark / client_rotate / probe_now, args}` → 桥接 SSH → 3x-ui API | b | 迁到新系统 |
| 16–21 | 同上 6 个（抽屉 · 管理） | 同上 | b | 迁到新系统 |
| 22 | 重启 Xray（线路页） | `/panel/action` `xray_restart` | b | 迁到新系统 |
| 23 | 查看 Xray 日志（线路页） | `/panel/action` `xray_logs` | b | 迁到新系统 |
| 24 | 面板“重试” | 不是写：重读一次数据 | — | `live.refresh()` |
| 25–28 | 新增家宽节点（运营总览 / 节点页 / 线路页页头 / 线路机卡片） | `/provision/node` → SSH + 3x-ui + Komari RPC 的上架任务 | b | 迁到新系统 |
| 29 | 新增线路机 | `/provision/relay` | b | 迁到新系统 |
| 30 | 登记已存在的入口 | `/fleet/add`（插件编排 prepare → Komari → bind → commit） | b | 迁到新系统 |
| 31–32 | 命令面板 重新验证链路 / 重启 Xray | `/panel/action` | b | 迁到新系统 |
| 33 | 新建批次（上架流水线） | 旧后台没有批次接口 | b | 迁到新系统（阶段 4） |
| 34–39 | 批次 暂停 / 取消未开始的行、行 重试 / 再等 / 跳过 / 提交密码 / 改 IP / 放弃 | 旧后台没有（`/jobs` 只读） | b | 实时模式下不可达；随新系统 |
| — | API 密钥 新建 / 撤销 | `/api-keys/create` `{name, scope, expires_days}`、`/api-keys/revoke` `{id}` | a | v2 没有这一页（侧栏链到原版 `/admin/api-keys`）；旧界面下线前要补 |
| — | 退出登录 | `GET /api/logout`（Komari 原生） | 会话 | 照原版跳转 |

计数（按上表 39 行）：类 a 8 个（接上 6：编辑资产、登记续费、监控与维护、确认故障、标记已知悉、标记已查看；不接 2：规则草稿保存 / 丢弃），类 b 30 个（其中上架流水线 7 行在实时模式下本来就不可达），1 个不是写（面板重试）；另有 API 密钥 2 个类 a 不在 v2 页面上。**旧界面下线前还缺**：API 密钥页（类 a，接口简单）；类 b 全部由新系统提供。

## 14. 中英数混排与剩余精修（构建 `38356ae8d122`）

**对齐**（owner：数字浮起来、空格忽宽忽窄、“线路机2 :20002”）。做法取原版首页的规则：**Inter 只给读数**（KPI 主值、进度线读数、表格 `.v`、抽屉大读数、弹层值、卡片流量与持续运行/到期），**混着汉字的名称、元信息、说明行一律正文字体**（SF + 苹方，数字 tabular）。SF 的数字与苹方同一基线、同一字重梯度；Inter 数字的大写高 0.727 em（SF 0.705）、空格也更宽，放进汉字句子里会显得偏高、偏宽。改动：

- `css/components.css`：元信息容器（`.panel-count/.panel-sub/.kpi-sub/.relay-meta/.ob-meta/.beat-win/.table-foot …`）改回 `--font-sans`，这些容器以及 `.kv-sub/.meter-sub/.note-body/.dialog-text/.toast-msg/.empty/.tl-meta/.th-2l-sub/.beat-cap/.dt .sub` 里的 `.num` 跟随正文字体（consistency #12 当时的方向反过来了）；`.meter-sub`、`.beat-cap b`、`.tl-meta`、`.th-2l-sub` 改正文字体。
- 名称、ID 不会被自动加空：`body { text-autospace: no-autospace }`（fidelity #4）保留；逐页扫描确认没有“团队 1-01”。
- “线路机2 :20002” → “线路机2:20002”（与原版一致）：`fmt.entryLabel`、`node-model` 的“主节点:20005”、线路 / 上架 / 向导里拼端口的 8 处、`live/synth.js` 与 `prototype/data/tools/build.py` 的 `entry_label`、原型快照数据；对照测试把旧抓取按新规则归一。
- 数字与汉字单位之间一个空格（brief §4 单位规则：`2 核`、`9 天`、`13 个入口`），百分号不空格——现状已一致，未改。
- 线路页名称行“接收 2%”：预留宽度从数字前面挪到后面（标签与数字之间只剩正常间距）；描述行 IP 后的复制图标改为悬停浮出，不再占一个图标宽的空隙。
- 放大 3 倍的前后对照：`shots/align/before-*`、`after-*`（原型数据）与 `live-*`（测试服实时数据），卡片标题、整张卡片、KPI、表格行、线路机头、抽屉头与正文，浅色 / 深色。

**剩余精修**（`prototype/original-refined/critique/*/REPORT.md` 的高 / 中 45 项：40 项此前已修，1 项今天随字体修掉，5 项部分完成，今天全部补齐）：consistency #7 “部分”改用 tagChip（运营总览两处）；#8 术语检查补上“拦截”；#10 抽屉“最近连接”加精确到秒的次行；craft #15 仍用 `--fg-3` 的可读说明文字（表单提示、已筛选、30 天图例、菜单与命令面板分组名、日志时间）改 `--fg-2`；craft #16 费用页签明细改 3 列与连接质量对齐；节点页空状态按钮改“新增家宽节点”。低优先级项不在本轮范围。

## 15. 接 fleetd（新后台）与新页面（构建 `d2f0b69f2c22`，`WRITES=1`）

**数据层优先用 fleetd，自动回落**（`live/boot.js`、`live/adapter.js`，契约 `fleet/API.md`）：

- 加载时 `GET /v2/api/v1/system`（5 s 超时）：返回 `{data:{service:'fleetd'}}`，或 fleetd 的 401（登录文案相同）→ 用 fleetd；404、Komari 前端的 HTML、网络错误、超时 → 照旧用 `/api/team-ops/` 轮询。fleetd 上线之前生产上这一次探测会 404（浏览器控制台会记一行 404，冒烟把这一条单独放行）；之后每 5 分钟（页面可见时）再探一次，fleetd 一出现就切过去，不用刷新。
- fleetd 模式：旧接口从 `/v2/api/legacy/`（与 `/api/team-ops/` 逐字节相同）读，fleetd 自有资源从 `/v2/api/v1/`；`EventSource('/v2/api/stream')`：`hello` 后停止定时轮询；`change` 只重取变了的那一组（overview/health → 快表；history；incidents/behavior/activity；renewals/jobs；probes；audit 只清缓存），250 ms 内合并成一次；`reset` 全量；`auth` → 登录对话框；`alerts / notify / silences` 交给页面（`TEAMOPS.fleet.on`）。45 s 没心跳或出错 → 回到轮询直到再次 `hello`；页面隐藏时关闭流。fleetd 不推的 Komari 客户端列表与逐节点流量仍每 5 分钟取一次。
- 延迟条：首屏取 `/v2/api/v1/probes/latest`（每节点每类最近 20 个样本），打开页面就是满的 20 格；失败 / 超时 / 出口不一致记为丢失、没有毫秒，`retry` 不算样本；接口不存在时照旧逐次积累。SSE `probes` 变化时重取。
- fleetd 的旧接口镜像连续 3 次读不到（不是登录问题）→ 自动回落到 `/api/team-ops/` + 轮询，页面收到 `fleet-down`（新页面变回“新后台未上线”）。
- 写后的一致性：fleetd 的 `/overview` 镜像比写操作最多晚一个轮询周期（≤ 10 s），旧的 `revision` 会让下一次编辑被拒（“资产信息已变化”）。`/assets/edit`、`/node/profile` 返回的新资产先记在本地（`source.noteAsset`），直到读回的 revision 追上为止，到期天数按账期时区重算。
- 类 a 的写仍走 `/api/team-ops/*`（fleetd 的旧接口只读）；fleetd 自己的写（确认事项、静默、维护窗口）走 `/v2/api/v1/*`，带 `X-Fleet-Request: 1`，同样只在 `WRITES=1` 构建里打开。

**新页面**（只用已有组件；侧栏顶层与原版一致，新页放进分组）：

| 页面 | 侧栏 | 数据 | 写 |
|---|---|---|---|
| API 密钥 `#/api-keys` | 运维（原位置，原来链到原版） | 旧 `/api-keys`、`/api-keys/usage`、`/schema`（fleetd 在时经其镜像） | 新建（名称 · 权限 · 有效期，密钥只显示一次 + 复制 + 调用示例）、撤销（勾选“我确认”）→ `/api/team-ops/api-keys/create|revoke`，照旧 apikeys.js |
| 事项与告警 `#/alerts` | 运维 | `/v2/api/v1/alerts`、`silences`、`maintenance` | 确认（`/alerts/{id}/ack`）、新建静默 / 维护窗口（范围 线路机 / 节点 / 团队 / 供应商 / 原因，开始 + 时长 ≤ 7 天）、结束（DELETE） |
| 通知 `#/notify` | 通知分组第一项“告警通知” | `/v2/api/v1/channels`（机密已打码）、`notify-log` | 只读（渠道在 fleetd 配置里） |
| 套餐与配额 `#/plans` | 运维 | `/v2/api/v1/plans`、`assets`、`quota` | 只读；续费在节点抽屉“费用” |

事项页：影子模式提示条“尚未配置通知渠道：告警只记录不外发”、4 张 KPI、状态分段（进行中 / 未确认 / 已确认 / 已结束 / 全部）、分组（按线路机 / 按原因）、展开行看证据与时间线、对象链到节点抽屉或线路页。顶栏在 fleetd 在时显示“事项 N”（进行中且未确认；严重为红，0 时不显示），点了去事项页。fleetd 不在时三个新页整页显示“新后台未上线”。

**测试服**：fleet 的测试实例（`fleet/staging`，a395 的 :18396 + 旧后端只读副本 :18397）已经在跑；`staging/fleet_front.py`（a395 :18398）把 `/v2/api/*` 转给它、其余转给 v2 测试服（:18395），模拟生产 Caddy 的同源布局。两个入口各跑一遍：`tools/smoke.mjs --base http://127.0.0.1:18395`（旧接口）与 `--base http://127.0.0.1:18398`（fleetd），`tools/writes-smoke.mjs` 同理（fleetd 读的是它自己的旧后端副本、不收 POST，所以类 a 的数据库写在 :18395 验，fleetd 自己的写在 :18398 验）。

## 16. 3x-ui 下线后的线路机、抽屉请求、顶栏（构建 `c7e392cafdef`，`WRITES=1`）

- **线路机状态**：3x-ui / x-ui 已下线，线路机 Xray 由新系统 fleet-xray 管理。`live/synth.js` 的线路机分项里“面板”“配置”不再参与判定（null = 不适用，不是故障）；Xray 运行状态优先取 fleetd `/v2/api/v1/relays/{host}/xray`（`process.xray.running`、版本、上报时间），没有 fleetd 时只在面板真的应答时才采用它的 `xray_running`，**null（未知）不再当成“已停止”**；采集分项在有 fleetd 数据时取 relayd 是否在上报（≥ 180 s 没上报 = 中断）。线路机主机 id：线路机1 = line1、线路机2 = central、线路机3 = line3（`[edge] relay_hosts`），只对 `/v2/api/v1/hosts` 里有的主机请求 xray（没有上报的线路机不产生 404）。
- **入口与节点速率**：有 fleet-xray 数据时，入口的连接数（该端口已建立连接）、速率（B/s × 8）、累计（cum_down = 发往客户端）、有活动（5 分钟窗口有流量或有连接）取新系统；节点的业务速率与累计同样来自它所在入口（`rate.source = fleet_xray`）。
- **仪表盘与线路页一致**：线路机资产在旧 `/health` 里若只因“线路管理通道”（经 3x-ui 桥接的采集）失败，而新系统报告该线路机 relayd 在上报、Xray 在运行，则这项检查记为不适用、状态为正常（`status.source = fleet`），运营总览与线路页读同一份结果。
- **原始错误不上屏**：`<urlopen error [Errno 111] …>` 之类的传输错误在数据层换成中文短语（连接被拒绝 / 超时 / 域名解析失败 / TLS 握手失败 / 连接失败）；面板错误不再传给界面。线路页页头右侧显示“Xray 更新于 …”（新系统上报）或“由新系统管理”，分项后缀“由新系统管理”；重启 Xray、入口停用等仍是类 b 提示。
- **抽屉请求**：“最近访问”只取 24 小时（`/activity?…&seconds=86400`，旧后端 0.7 s；14 天要 16–20 s）。请求分两条车道：页面 / 轮询 / 后台（≤ 3）与抽屉 / 图表（≤ 2），互不挤占；客户端超时：抽屉 15 s、其余 30 s，超时即让出位置并提示“读取超时”。
- **SSE**：fleetd 第一次见到每个资源都会推一次 change；5 秒内刚读过的组不再重取。`probes/latest`（约 224 KB）最多 30 秒取一次。
- **顶栏**：“事项 N”在 ≤ 760 宽只留图标 + 数字；≤ 480 宽收起只有一项的“强调色”“语言”菜单。390 / 375 宽顶栏与页面都不横向溢出（冒烟新增顶栏溢出检查）。
- 复核（fleetd 模式）：出口验证超时显示“超时”、抽屉“近 24 小时”先“—”+ 骨架再出值、抽屉只发 `seconds=86400` 的访问请求。

## 17. 累计用量回到连续账本、ETag、抽屉复用（构建 `35e0b674dc78`，`WRITES=1`）

- **累计用量**：fleet-xray 的累计计数随进程重启清零（例：20001 从 308 GiB 变成 117 MiB），不再用于入口与节点的“累计用量”；累计、今日、本月一律取旧系统的连续账本（x-ui.db 计数由 fleet-xui-traffic 继续维护，旧采集账本照常）。只从新系统取：速率、连接数、Xray 是否运行。
- **有活动与连接数**：同一个 5 分钟窗口——窗口内有流量或当前有连接即“有活动”；入口表与线路机描述行的提示写“近 5 分钟有流量或当前有连接”。
- **ETag**：fleetd 对 `/v2/api/legacy/*` 带 ETag 且 `no-store`（浏览器不会自己复验），数据层自己带 `If-None-Match`；`overview` 与 `health` 都 304 时不重建、不求差、不移动时钟锚点，只确认数据仍是新的（鲜度照常）。服务端不带 ETag 时行为不变。
- **抽屉**：`/node?asset=`（371 KB，服务端 2–3 秒）30 秒内复用，同一节点连开两次只读一次（本会话内的补充数据本来就只取一次）。

## 18. 写操作全部接新后台、登录页、旧系统下线（构建 `0594c7cf341d`，`WRITES=1`）

契约：`fleet/API.md` §5d（上架）、§8（账户）、§9（原生写）、§12（下线）。**能力只看 `/v2/api/v1/system` 的 `data.features`**（`TEAMOPS.fleet.has(名)`；没有这份列表的旧 fleetd 只认 `auth` ≥ 0.4 与 `edge.onboard`），界面从不发请求去“试”一个接口，所以没有 404 噪音。每个写操作在 fleetd 列出对应能力之前都给提示“新后台尚未开放此操作，开放后在这里直接进行”，列出后同一个按钮直接生效，不用重新构建。

| 能力 | 界面 | 请求 |
|---|---|---|
| `onboard` | 上架流水线整页（批次卡 · KPI · 线路机合批 · 矩阵 · 步骤抽屉 · 全部批次）；新建批次（CSV 粘贴 / 上传 → 计划 → 核对哈希 → 开始）；待确认批次“确认计划并开始”；已中断 / 待介入“继续执行”；“取消批次”（输入批次号；在跑的批次也能取消：回 202 `cancelling` → 取消中，进行中的步骤跑完后逆序补偿 → 已取消）；行内：补密码、DD 人工闸门“重装完成，继续”（可附重装后的 root 密码）、重试（风险停止要输入行名确认已处置）、放弃此行（输入行名，逆序补偿）。其它页的“新增家宽节点”跳到这里打开新建批次 | `GET/POST /onboard/batches`、`GET /batches/{id}`、`/plan`、`/events?after=N`、`POST /start {plan_hash}`、`/resume`、`/cancel {row?}`、`/rows/{name}/actions {action, password?, force?}` + `resume`。SSE `onboard`（path = 批次）→ 只重读那个批次；兜底：有批次在跑时 10 秒、否则 60 秒，页面在后台时不读 |
| `entries` | 线路页、节点抽屉：停用（确认）· 启用 · 重置用量（输入对象名）· 改备注（≤ 64 字）· 更换连接凭据（**输入“线路机名:端口”**，说明客户端订阅会变；生效后读新分享链接只显示一次） | `POST /relays/{线路机}/entries/{端口}/actions {action: enable|disable|reset_traffic|rename|rotate_uuid, remark?, confirm?}`；新链接 `GET …/share` |
| `restart_xray` | 线路页“重启 Xray”（原确认框）；命令面板跳到那张线路机卡片 | `POST /relays/{线路机}/actions {action:'restart_xray', confirm:线路机名}` |
| `ops` | 以上异步操作的进度：同一条提示“已提交，等待线路机确认…” → “已生效（线路机3 第 43 代配置）” / 被拒绝（relayd 回执里的原因）/ 部分生效 / 线路机 60 秒内没有取走（可重试） | `202 {op_id,…}` → SSE `ops`（path = 这个 op）立即读，否则 1 s、2 s… ≤ 5 s 读 `GET /ops/{op_id}`，最多 2 分钟 |
| `policies` | 节点行为 › 访问规则：范围列表与编辑框显示线上生效的规则与服务端草稿；保存草稿、预览影响（每个节点应用后的模式 + 近 24 小时模拟阻止次数）、应用（勾选核对，每台线路机一个 op，提示“k/N 台线路机已生效”）、丢弃全部草稿、发布记录里“回滚到这个版本”（输入版本号）。没开放时读仍然是真的（旧 `/policies`、`/policies/preview` 只读） | `GET /policies`、`PUT /policies/draft {scope, policy, base_rev}`、`POST /policies/preview|apply {draft_hash}|discard|rollback {version_id}`；SSE `policies` |
| `assets.write` `renewals` `profile` `incidents.ack` `behavior.ack` `api-keys` | 原来的类 a 表单不变 | 有能力 → `PATCH /assets/{uuid}`、`POST /assets/{uuid}/renewals`、`PUT /nodes/{uuid}/profile`、`POST /incidents/{id}/ack`、`POST /behavior/ack`、`POST /api-keys`、`/api-keys/{id}/revoke`（`X-Fleet-Request: 1`）；没有 → 旧 `/api/team-ops/*`（旧系统下线后不再走，改为提示） |
| `auth` | **v2 登录页**（原版登录页样子：卡片 = 对话框外观、品牌在卡片里、提交按钮满宽带 LogIn 图标，两步验证码后台要求时才出现；旧系统下线前多一个“使用原版后台账户登录”）；**账户与安全** `#/account`（账户分组第一项）：登录方式、修改密码、两步验证（生成密钥 → 验证器里手动添加 → 6 位码开启 / 关闭）、登录会话（可退出单个会话）；顶栏“退出登录” | `POST /auth/login {username,password,totp?}`（401 `totp_required` → 显示验证码框；429 原文）、`/auth/logout`（旧系统在时再 `GET /api/logout` 退出原版会话）、`GET /auth/me`、`/auth/sessions`、`POST /auth/password`、`/auth/totp/setup|enable|disable`、`DELETE /auth/sessions/{id}` |

- **未登录**：fleetd 对 `/system` 回 401 时直接显示登录页（不再请求 `/overview`，控制台只有这一个 401）；会话中途失效（SSE `auth` 或任一 401）同样换成登录页，并说明“登录状态已失效”。登录成功整页重载。旧后台（没有 fleetd）时仍是原来的“去登录”对话框。
- **旧系统下线**（`legacy_source = native`）：数据层继续读 `/v2/api/legacy/*`（fleetd 用自己的库给同样的形状）与 `/v2/api/v1/*`，**fleetd 连续失败也不再退回 `/api/team-ops/`**（保持重试并提示）；侧栏拿掉原版 Komari 的原生页（空了的分组一起拿掉），直接访问这些地址显示“原型未包含此页”式空状态而不是“在原版后台打开”；只读提示不再带“打开原版”；“查看访问记录”改开节点抽屉的访问页签；类 a 写只走 fleetd 原生接口。
- **没有后端的动作**不出现或给原因：上架矩阵的跳过 / 再等 10 分钟 / 打开面板 VNC / 改 IP（改 IP = 放弃此行 + 新 CSV）不出现；重新验证链路（“新后台每分钟自动验证链路”）、查看 Xray 日志（“新后台暂不提供”）、新增线路机（“由 fleet-cutover 接入”）、重试面板连接（“3x-ui 面板已退役”）给原因提示。
- **只读构建**（不带 `WRITES=1`）：以上写全部只给“这个构建没有开启写操作”提示；读（包括访问规则、上架批次）照常。

**测试**：`tests/` 44 个（新增 `writes.test.mjs` 8 个：类 a 路由、类 b 接线条件、op 进度、入口请求体、上架批次模型、事件日志、CSV 检查、设备名；`fleet.test.mjs` 下线后不回退、SSE 页面资源；`boot.test.mjs` features 判定），`prototype/shared` 71 个。测试服：`staging/fleet_front.py` 现在像 Caddy 一样带 `X-Forwarded-For`（否则 fleetd 把代理来的请求当本机诊断），304 / 204 不再加分块编码（浏览器会拒收）；新开 :18399 = 不注入会话的同源入口，专门验登录。`tools/flow-smoke.mjs`（`UITEST_PW` 从 a395 的 `fleet-stage/test-user.txt` 读，不打印）：登录页三种视口、错密码、登录、账户页、上架全流程（CSV → 计划 → 哈希 → 开始 → 矩阵 → 抽屉日志 → P1 连不上文档地址 → 待介入 → 取消批次；第二个批次在跑时取消 → 取消中 → 已取消）、退出。27/27。

## 19. 旧系统下线后的生产修正（构建 `55ca52365e6c`，`WRITES=1`）

生产现状：fleetd 0.9.x、`legacy_source = native`、Komari 与 team-ops 已停，`/api/*` 回 410。

1. **首屏不再被可选数据挡住**：首次加载只等 `/overview` + `/health`（fleetd 的 `/v2/api/legacy/*`）；history、incidents、behavior、activity、clients、renewals、jobs、hosts、延迟条、线路机配额最多再等 3 秒，没到的之后到达时自动补上（`notify('core')`，轮询器照常打补丁）。套餐目录 5 秒读不到就退回原始套餐名。**启动看门狗**：15 秒还没有首屏数据 → 内容区显示“数据加载超时”、正在等哪些接口、重新加载按钮（数据后到仍会正常显示），不再无限骨架。生产只读核对（`tools/prod-mirror.mjs` + `tools/prod-check.mjs`，只转发 GET）：就绪 5.5 秒（经本机转发），0 失败请求。
2. **原版入口全部拿掉**：登录页不再有“使用原版后台账户登录”；侧栏去掉 Komari 原生页（主机配置、主机统计、延迟监测、远程执行、会话管理、工作台、主题、插件、系统、原版账户 / 关于）；只剩一项的分组摊平：“告警通知”“账户与安全”成为顶层项；退出只调 `POST /v2/api/v1/auth/logout`（不再请求 `/api/logout`）；只读提示、访问记录、上架说明都不再链到原版；旧书签打开原生页 → “这个页面属于已下线的原版后台 · 回到首页”。
3. **应用在站点根**：`dist/index.html` 在根（`https://38.58.57.22/#/home`），其余文件在 `/app/`、`/shared/`、`/original/`、`/data/`、`/live/`（相对路径不变，`../../shared` 从 `/app/js/` 正好落到 `/shared`）。旧地址 `/admin/v2/app/#/lines` → 301 `/` 时浏览器自动带上 `#/lines`。
4. **名称 + 计数对齐**（分段、页签、筛选 chip）：名称与计数放进同一个行内文本块，同一条基线；计数与名称同字体、同字号、等宽数字，常规字重、次要色，固定 6 px 间距。
5. **线路机卡片**：流量上限 = 套餐目录的出站配额（十进制 GB：Pulse Mini 2,000 GB、Pulse Nano 1,000 GB）+ 本账期已出站（`/v2/api/v1/quota`），显示“已用 / 上限”和额度条（≥ 90% 橙、≥ 100% 红）；延迟条：fleetd 的 `probes/latest` 里一出现线路机自身的探测（键为 r1 / line1 / central / line3，或 `relays` 下）就照节点一样画，没有时仍写“不适用”。
6. **节点抽屉 · 来源画像 / 来源 IP**：读 fleetd `GET /v2/api/v1/nodes/{uuid}/sources?window=24h|14d`（运营商、地区、接入方式分布、逐个来源 IP 与情报）；fleetd 没数据时退回这个节点自己的访问记录（24 小时）与入口当前的来源 IP——有连接时绝不显示“没有来源连接”；详情到达前显示骨架。
7. **事项原因中文**：规则 / 原因代码（`ipintel_shared_identity`、`upstream_down` …）一律译成中文（与 fleetd `CAUSE_LABELS` 同一份；接口给 `rule_label` / `cause_label` 时以接口为准），摘要里夹的代码也替换。
8. **目录的内部待办不上屏**：套餐目录的 `questions` / `pending` 与规格文字里的核实备注（“（…截图）”“Qn”之类）一律不显示——首页套餐浮层、节点抽屉套餐、套餐与配额页都去掉了；目录没有定论的流量上限显示中性的“未设置”。构建时 `tools/dist.mjs catalog` 生成去掉这些字段的 `dist/data/plan-catalog.json`，`check` 发现 dist 里出现核实类字样、`questions` / `pending` 或 Qn 引用就失败。
9. **深链**：fleetd 通知里的 `#/lines/r1` → 线路页该线路机，`#/nodes/t1-01` → 节点抽屉。
10. **线路机的延迟与出口验证**：与节点同一种画法——relayd / fleetd 对主节点端口的 TCP 心跳与经主节点入口的出口验证（`probes/latest` 里按线路机资产 uuid / r1 / 主机 id / `relays` 取）有样本就画 20 格条、延迟 · 丢包、出口验证；节点表的 TCP 列与线路页主节点入口的“TCP / 出口”同样取这份数据。“不适用”的说法全部去掉，没有样本时是缺测“—”。

## 20. 热修（构建 `a7b5d5b6cbee`，`WRITES=1 PALETTE_DEFAULT=indigo`：外观与 55ca52365e6c 相同）

- **对话框滚不动**（owner：连接突增证据对话框 13 行只显示一半）：`.dialog-body` 是 grid，`overflow:hidden` 的证据表被压扁、末尾几行被裁掉，而体本身没有溢出——所以滚轮什么也不做。修：体 `min-height:0` + `grid-auto-rows:max-content` + `overscroll-behavior:contain`，对话框最高 `min(85vh, 100dvh - 32px)`；抽屉体同样 `min-height:0`；弹层超高时自己滚。滚动锁只锁背后的页面（`html` overflow hidden），没有任何全局 wheel / touch 拦截。自动检查 `tools/scroll-check.mjs`：对话框（证据 × 2、40 行长对话框）、抽屉（访问记录、概览）、命令面板，1440×900 / 1440×520 / 390×844 / 390×560，真实滚轮滚到底且最后一行可见、PageDown / ↓ 能滚、背后页面不动——修前 27/31（长对话框 4 个视口全挂），修后 41/41。
- **旧系统下线后的写**：类 a 写（资产编辑 / 续费、监控设置、故障 / 行为确认、API 密钥）与访问规则草稿 / 丢弃 / 发布改走 fleetd `POST /v2/api/v1/legacy-write/<旧路径>`（`features` 有 `legacy.write` 时；请求体与返回同旧接口）。55ca 在下线后的生产上这些会提示“新后台尚未开放”，这版修好；生产同形的测试服（fleetd 0.9.4 native，:18386）写入验收 21/21。
- **来源 IP 的画像**：节点行为的来源列表、证据对话框、抽屉“最近连接”——行上有 `source_intel` 就用；否则批量查 `GET /v2/api/v1/ipintel/batch?ips=`（fleetd 0.9.7；老版本逐个查 `/ipintel/{ip}`，同时 ≤ 3，缓存）；线路机地址直接写“线路机N（内部）”+“线路机自身”标记；没有画像时是“—”，不再写“没有画像”。
- **抽屉来源卡**：8 秒没有来源数据 → 用这个节点最近的访问记录顶上，写明“来源画像读取超时”并给“重试”，控制台记下哪个请求慢；fleetd 0.9.7 的 sources 已改成 3–18 ms（原因是 IP 情报入库长时间持锁）。
- 线路机来源画像不请求（fleetd 对线路机没有来源统计）。

## 21. 热修 2（构建 `e3504d4570be`，`WRITES=1 PALETTE_DEFAULT=indigo`）

- **证据对话框的逐条记录向新后台要**：`/v2/api/legacy/activity?asset=&host=|source_ip=&limit=200`；行为事件取事件前 10 分钟到后 5 分钟（`before = ts + 300`，`seconds` 覆盖到 `ts − 600`），其余 24 小时；“加载更早的记录”（`before` = 最早一条）；行上的 `source_intel` 直接用；去掉“逐条记录只保留最近几条”的说明。
- **对话框开着时背后的页面不动**：节点行为列表在有浮层时不重画（数据或情报到了先记下，浮层关掉再画一次）——原来列表换行位置看起来像背后被滚走。`scroll-check` 增加“背后所有滚动容器与窗口位置不变”和“在遮罩上滚轮不动背后”，并滚到 200 行证据的最后一行：55/55。
- **打开更快（先用本机缓存）**：上次的核心数据（overview、health、history、事件、行为、访问、客户端、续费、任务、主机、配额、延迟条、审计 / 延迟任务）存在本机（localStorage，≤ 24 小时，退出或 401 即清除）；再次打开时先用它立即画出页面，同时在后台读新数据、到了就地更新（鲜度写缓存的年龄）。上次的新后台 `/system` 也缓存，不等探测。套餐目录也先用缓存。所有静态模块 `modulepreload`，当前页的页面模块在首屏脚本里提前下载。
  实测（本机出静态文件、接口经隧道到生产同形测试服）：再次打开的首屏数据 首页 150 ms、运营总览 101 ms、节点 114 ms、线路 154 ms、节点行为 535 ms、事项与告警 1,420 ms（事项列表每次向新后台读，下一步做缓存）；应用内（模块加载完到有数据）52–87 ms。首次打开 1.7–4 s（按接口）。经 SSH 隧道 + 单线程测试服出静态文件时模块下载本身就要 ~1 s，生产的 Caddy（HTTP/2 + ETag）不受这个影响。

## 22. 热修 3（构建 `b85ba8c95967`，`WRITES=1 PALETTE_DEFAULT=indigo`）

- 表格上的纵向滚轮：对话框 / 抽屉里的表按容器宽度排（可截断列吃剩余宽度，`.table-wrap` 放得下就加 `.is-clip`，不再是滚动容器；`syncWraps` 也管浮层里的表）；仍需横向滚动的表，纵向为主的滚轮交给最近的能纵向滚的祖先（对话框体 / 抽屉体 / 页面；背后页面被锁时不交）。
- 加上 §21 之后的：事项与告警、通知、套餐与配额、API 密钥先用本机上次的结果画出来（不出骨架）再换新；退出或会话失效清掉所有 `teamops:*` 缓存；新访问记录走 fleetd `activity/delta` 增量（SSE `activity`），抽屉“最近连接”卡就地更新。
- 检查：`scroll-check` 在证据对话框、宽表对话框、抽屉“最近连接”、节点 / 线路页的表上滚轮（带少量横向分量）+ 在单元格上发 DOM wheel 事件确认转交，95/95。

## 23. 增量（构建 `f4ef8aa093ef`，`WRITES=1 PALETTE_DEFAULT=indigo`）

- 节点行为：已有表格时换筛选 / 切节点 / 数据未到不回到骨架，旧行变淡留在原处，新数据到了就地换上。其余见 §21–§22（先用本机缓存打开、activity 增量、事项 / 通知 / 套餐 / 密钥先画缓存）。
- 打开时间（本机出静态文件，接口经隧道到生产同形测试服；再次打开 / 首次打开，“首屏数据”= 页面有真实行、无骨架，“新数据”= 本次读到的数据已套用）：

| 页面 | 再次打开 首屏数据 | 新数据 | 首次打开 |
|---|---|---|---|
| 首页 | 128 ms | 420 ms | 545 ms |
| 运营总览 | 108 ms | 602 ms | 485 ms |
| 节点 | 86 ms | 396 ms | 1,171 ms |
| 线路 | 122 ms | 524 ms | 595 ms |
| 节点行为 | 96 ms | 410 ms | 442 ms |
| 事项与告警 | 106 ms | 488 ms | 821 ms |

## 24. 只用靛蓝（构建 `a0add577037e`）
- owner：粉紫先不做。首屏脚本固定 `data-palette="indigo"`，强调色菜单只有“靛蓝”，构建不再有 `PALETTE_DEFAULT`；tokens.css 里的粉紫 / plum2 token 块保留但不会被套用。其余同 `f4ef8aa093ef`。

## 25. overview / health 增量（构建 `1269fed80179`）
- fleetd ≥ 0.12 且 `features` 有 `delta.overview` / `delta.health` 时，快速组（SSE change / 轮询）读 `GET /v2/api/v1/delta/{overview|health}?since=<手上的版本>`，按 §12.5 的补丁规则就地套用（不改原对象，没变的子树共用；空补丁 = 没变，不重建）；第一次 / 版本太旧 → 整份；503 或出错 → 读整份 /overview、/health（ETag 照旧）。事项与告警照旧整份（很小）；访问记录见 §22。
- 检查：单元测试按 fleetd delta.rs 的往返用例核对 applyDelta（结果一致、原对象不变、`$removed`、子树共用）。

## 26. 出口只看连通（构建 `3075c7d7fd53`，配合 fleetd 0.13.3）
- 拿到出口 IP 就是通过（`egress_verified` = 有实际出口），IP 只作信息；不再有“不符 / 认证出口验证 / 与台账不符”。抽屉概览：“连通”（悬停出口 IP）/ 超时 / 失败 / 未知；线路页入口表：“出口 x.x.x.x”；出口画像卡：IP 换过只作信息，台账记录注明“仅供参考”。检查项名“出口”，旧原因码 egress_mismatch 显示“出口变更”；新告警 `egress_route_leak` =“出口泄漏到线路机”。
- 检查：单元测试（有出口 IP、换了 IP、没拿到 IP 三种）50/50，shared 71/71。
