# 原版精修原型 · 外壳、组件库与首页

`prototype/original-refined/` 是“在原版基础上美化”的原型。规范见 `../original/SPEC.md`，token 只有一份：`../original/tokens.css`，这里直接引用，不复制。

这一步完成的范围：

- 外壳：顶栏、侧栏、hash 路由、浅色 / 深色 / 跟随系统主题、命令面板、快捷键、鲜度计时器。
- 全站组件库：`css/components.css`、`css/cards.css`、`css/table.css`，以及 `js/components.js`。
- 首页 `#/home`：KPI、三种视图、分组、排序、地区与套餐筛选、三种节点卡片、今日流量弹层、套餐弹层。
- 节点抽屉：7 个页签。其中“主机”页签收纳了原首页详情页 `/instance/<id>` 的内容，包括 6 张 uPlot 小图。

运营总览（`#/dashboard`，§9）已接入；其余页面由各自的构建者补上，没登记的路由显示“原型未包含此页”。

> **给页面构建者的规则：不写自己的组件样式。** 页面只组合本 README 里的组件和 `pages.css` 里的排布类，所有颜色、字号、圆角、间距都来自 token。缺组件时，先在 `components.css` 和 `js/ui/*` 里补通用版本，再在本 README 里登记。

---

## 1. 运行与检查

| 事项 | 做法 |
|---|---|
| 打开 | `http://127.0.0.1:5199/original-refined/`。5199 是 `prototype/` 的静态服务器，不在时用 `nohup python3 -m http.server 5199 --bind 127.0.0.1 --directory ~/Desktop/team-ops-v2/prototype >/dev/null 2>&1 &` 启动，不要关掉它 |
| 深浅色 | 用顶栏的外观按钮或 `⌘⇧L` 切换，结果存进 localStorage `refined:theme`，值为 `light`、`dark` 或 `system`。首屏前由 `index.html` 里的内联脚本设好 `<html data-theme>`，不会闪 |
| 造状态 | URL 里加 `?demo=1`，页面底部会出现演示条：可以回放 5 个真实时刻（故障、降级、16 台未知），也可以随机触发一次状态变化。调试时可以直接用 `__app.store` 和 `__app.live` |
| 截图 | 运行 `~/.local/bin/node tools/shoot.mjs tools/jobs-home.json`（无头 Chrome，只访问本地；这份 job 清单生成了 `shots/` 里的全部 44 张验收截图，包括与原版并排的 `compare-*.png`）。job 支持 `dark`、`view`、`full`、`before`（页面内执行的 JS）、`delaySnapshot`（拖住 snapshot.json，用来截首屏骨架）。截图写到 `shots/` |
| 验收 | `~/.local/bin/node tools/audit.mjs '#/home' 20` 检查 SPEC §8.2 的 V3 字号、字重、圆角、控件高，V4 术语，V5 套餐原文，V6 实时更新 20 秒的累计布局位移，V8 窄屏溢出，V13 控制台报错。V2 用 `grep -nE '#[0-9a-fA-F]{3,8}\b\|!important' css/*.css` |
| 交互 | `~/.local/bin/node tools/interact.mjs`：用真实按键事件跑快捷键、抽屉、命令面板、筛选写 URL、后退关抽屉 |

## 2. 目录

```
index.html            外壳挂载点；首屏内联脚本设置主题
css/app.css           唯一入口：tokens → shell → components → cards → table → pages
css/shell.css         顶栏、侧栏、内容区、页头
css/components.css    通用组件（按钮、分段、页签、chip、KPI、进度线、心跳条、面板、表单、浮层、抽屉、对话框、toast、命令面板、空态、鲜度、图表）
css/cards.css         节点卡片 NodeCard（三种视图 × 三种节点）
css/table.css         DataTable、表内进度线、步骤单元格 StepCell
css/pages.css         页面级排布（stack、row、cols-2、home-bar、抽屉内排布）
js/app.js             外壳、路由、主题、快捷键、命令面板、鲜度、演示条
js/components.js      组件库唯一入口（re-export js/ui/*）
js/ui/dom.js          esc、num（Fmt → HTML）、on（事件委托）、storage、flag…
js/ui/controls.js     按钮、chip 家族、分段、页签、折叠、复制
js/ui/overlay.js      tooltip、弹层、菜单、口径说明 ⓘ
js/ui/layers.js       抽屉、对话框、确认分级、toast、Esc 栈
js/ui/viz.js          KPI 卡、进度线、额度条、心跳条、30 天条、图表
js/ui/layout.js       页头、鲜度、面板、明细、提示条、空 / 错误 / 骨架
js/ui/table.js        dataTable、tableSkeleton、tableMeter、stepCell
js/ui/palette.js      命令面板外观（逻辑用 shared/keys.js）
js/icons.js           lucide 路径（描边 1.5），icon(name, size)
js/plans.js           套餐规范名（SPEC §7）
js/node-model.js      节点字段的显示模型（卡片、抽屉、表格共用）
js/node-card.js       节点卡片渲染与实时补丁
js/node-drawer.js     节点抽屉（任何页面都用 app.openNode 打开）
js/node-ipintel.js    抽屉里的 IP 情报：出口画像（连接质量）、来源画像与来源 IP（访问记录）
js/pages/index.js     页面注册表
js/pages/home.js      首页
js/pages/nodes.js     节点（11 列表格 + 工具条，见“节点 #/nodes”一节）
assets/flags/         US.svg、MX.svg（原版首页的国旗）
tools/                shoot.mjs、audit.mjs、interact.mjs
shots/                验收截图
```

## 3. 写一个新页面

1. 新建 `js/pages/<name>.js`，然后在 `js/pages/index.js` 里登记一行 `<name>: () => import('./<name>.js')`。
2. 模块默认导出：

```js
import { fmt } from '../../../shared/index.js';
import { pageHead, freshness, segmented, onSegment, panel, dataTable, watchStuck, hydrate } from '../components.js';

export default {
  title: '节点',
  density: true,                    // 运营页有 宽松 / 紧凑；外壳会在 <main data-density> 上应用 token 覆盖
  mount({ el, app, params }) {      // params：hash 里的 URLSearchParams
    el.innerHTML = `<div class="page">${pageHead({ title: '节点', tools: freshness() + segmented({ name: 'density', items: [{ value: 'comfortable', label: '宽松' }, { value: 'compact', label: '紧凑' }], value: app.density() }) })} …</div>`;
    const offStuck = watchStuck(el);
    const offDensity = onSegment(el, 'density', (v) => app.setDensity(v));
    const offLive = app.store.on('change', (diff) => { /* 只补丁变化的格子 */ });
    return {
      update(params) { /* URL 参数变了（筛选、排序）：只重画受影响的部分 */ },
      nodeOrder: () => ids,          // 抽屉里 [ ] 切换节点时用的顺序
      onDrawer(id) { /* 抽屉正在显示的节点（高亮当前行 / 卡） */ },
      unmount() { offStuck(); offDensity(); offLive(); },
    };
  },
};
```

3. 页面状态写进 URL：`app.setParams({ status: 'attention', sort: '-rate' })`（replaceState，会触发 `update(params)`）。视图偏好写 localStorage（`storage.set`）。规则见 SPEC §5.6。
4. 打开节点详情一律用 `app.openNode(id, tab)`，不要自己做详情页。tab 的取值有 `overview`、`quality`、`traffic`、`host`、`billing`、`access`、`manage`。

### `app` 对象（传给每个页面）

| 成员 | 说明 |
|---|---|
| `store` / `live` | `shared/data.js` 的 store（套餐已换成规范名）与 `shared/live.js` 的实时控制器 |
| `keys` | `shared/keys.js` 的注册表。页面用 `app.keys.bindAll([...])` 绑定自己的快捷键（给出 `desc` 和 `group`，`?` 帮助里会列出来），并在 `unmount` 时解绑 |
| `params()` / `route()` | 当前 hash 的参数与路径 |
| `setParams(obj, { replace })` | 合并参数写回 URL，值为 `null` 表示删除 |
| `navigate('#/nodes?plan=…')` | 换页 |
| `openNode(id, tab)` | 打开节点抽屉（pushState，浏览器后退就关抽屉） |
| `drawerOpen()` / `drawerNodeId()` | 抽屉状态 |
| `density()` / `setDensity(v)` | 运营页密度，存 localStorage `refined:density`，只对声明了 `density: true` 的页面生效 |
| `toast(opts)` | 同 §4.5 |

---

## 4. 组件 API（全部从 `js/components.js` 导入）

渲染函数返回 **HTML 字符串**。交互由外壳启动时装好的**事件委托**处理：`installControls`、`installOverlays`、`installBeats`，页面不用逐个绑定。渲染完成后对新插入的区域调用一次 `hydrate(root)`，用来定位分段滑块和页签指示条。

### 4.1 基础（`ui/dom.js`）

| 函数 | 说明 |
|---|---|
| `num(f, { plain, cls, title, tone })` | **所有数值都要经过它**。输入 `fmt.*` 返回的 Fmt，数字（含 `$`、`<`、`↓` 前缀）包在 `.num` 里，单位包在 `.unit` 里（12 px `--fg-2`）。缺测、未设置、待核实这类占位值会自动退一级颜色，语气色也会自动带上，`exact` 放进 title。`plain: true` 时单位不缩小 |
| `txt(f)` | 纯文本 Fmt（相对时间、日期等），带语气色与 title |
| `esc(s)` / `attrs(obj)` | 转义 / 拼属性 |
| `on(root, type, selector, (e, el) => …)` | 事件委托，返回解绑函数 |
| `storage.get(key, def)` / `.set` / `.remove` | 包了 try/catch 的 localStorage |
| `flag('US')` | 国旗 21×14 |
| `debounce`、`rafBatch`、`$`、`$$`、`el(html)`、`currentScheme()`、`reducedMotion()` | 小工具 |

`fmt` 来自 `shared/format.js`，负责单位、精度和状态词，详见 `../shared/README.md`。页面里不直接调用 `toFixed`、`toLocaleString`、`Intl.*`。

### 4.2 按钮与 chip（`ui/controls.js`，SPEC §3.7–3.8）

| 函数 | 外观 |
|---|---|
| `button({ label, icon, iconEnd, caret, variant, size, attrs })` | `variant`：`primary`、`secondary`（默认）、`quiet`、`text`、`link`、`danger`、`danger-quiet`；`size`：`lg`（36，默认）、`md`（32）、`sm`（28）。每个区域最多一个 primary |
| `iconButton({ icon, label, size, attrs })` | 方形 36/32/28；`label` 必填，同时用作 aria-label 和 tooltip |
| `statusPill(fmt.health(code, { stale }), { count, text })` | 胶囊 + 6 px 圆点 + 10% 底。未知和数据过期用空心圆点，维护中的圆点会呼吸 |
| `tonePill(text, tone, { dot, title })` | 任意语气的 pill（`已转发`、`已验证`、步骤状态） |
| `neutralChip(text)` | `无主机`、`家宽代理`、`未绑定`、`主节点` |
| `tagChip(text)` | 发丝线描边、灰字：供应商、标签、线路机名 |
| `priceChip(fmt.monthlyFee(b, { perMonth: true }))` | `$28.88/月`，黑字 600 |
| `utilChip(fmt.utilPair(d, u))` | `↓<1% ↑2%`，卡片 footer 用 |
| `mark('IPv4' / 'LA' / '原名', { warn, title })` | 18 高的小标记 |
| `kbd('⌘K')` | 键帽 |
| `fchip({ label, value, group, count, pressed, flagHtml, tip })` | 筛选 chip（32 高）。选中时是软靛蓝底；点击由页面委托处理 `[data-fgroup]` |
| `moreChip(n, items)` | `+N` 溢出 chip，点开是完整列表（已委托） |
| `removableChip(text, attrs)` | 已生效筛选的可删 chip |
| `dot({ state: 'on'\|'off'\|'unknown', tone, tip })` | 6 px 圆点 |
| `infoButton(text)` | ⓘ 口径说明，点开是 280 宽的弹层（已委托）。用它替代 `title` |
| `copyable(text, { display })` | 可复制的值：悬停出 Copy 图标，复制后变 Check 1.5 秒，并给屏幕阅读器播报（已委托） |

### 4.3 分段、页签、折叠

```js
segmented({ name: 'view', role: 'tablist'|'radiogroup', size: 'md'|'sm', value, items: [{ value, label, count, disabled, tip }] })
onSegment(root, 'view', (value) => …)      // 返回解绑函数
setSegmented(root, value)                   // 程序化切换，不派发事件
setSegmentCounts(root, { value: n })        // 实时更新计数
tabs({ name, items: [{ value, label, count, unread }], value })   // 下划线页签；监听 'tab-change' 事件
disclosure({ title, meta, body, open })     // 折叠，替代 <details>
```

- 分段是胶囊轨道加白色滑块，滑块会在项之间滑动；← → 可以移动并选中，Home / End 跳到两端。放不下时轨道内部横向滚动。
- “切换同一份数据的视图或筛选”用分段（首页视图、分组、密度、时间范围）；“同一个面板里切换内容”用下划线页签（抽屉页签、节点行为四个视图）。

### 4.4 浮层（`ui/overlay.js`，SPEC §3.18）

| 用法 | 说明 |
|---|---|
| 任意元素加 `data-tip="…"` | tooltip，悬停 400 ms 后出现；加 `data-tip-fast` 改为 80 ms，给心跳格这类数据小格用。键盘聚焦时也会出现。内容用 `\n` 换行 |
| `openPopover(anchor, { html, width, placement: 'bottom-start'\|'bottom-end'\|'bottom', label })` | 返回 `{ el, update(html), close(), reposition(), pinned }`。同一时间只开一个；Esc、点外部或页面滚动都会关闭 |
| `hoverPopover(anchor, { render(api), width, delay })` | 悬停预览 + 点击固定（今日流量弹层） |
| `openMenu(anchor, { items, onSelect(value, item), minWidth })` | 菜单项的写法：`{ group: '排序' }`、`{ sep: true }`、`{ value, label, icon, checked, end, danger, disabled }`。↑↓ 移动，Enter 选择 |
| `showTip(el)` / `hideTip()` / `refreshTip(el)` | 数据变化时原地更新正在显示的 tooltip，不关闭、不跳位 |
| 弹层内部排版类 | `.pop-title`、`.pop-row`、`.pop-key`、`.pop-val`、`.pop-sep`、`.pop-foot`、`.pop-q`（待核实）、`.pop-skel` |

### 4.5 抽屉、对话框、toast（`ui/layers.js`，SPEC §3.16–3.19）

```js
openDrawer({ title, pill, meta, tabsHtml, tab, render(tab, bodyEl, api), onTab, onNav(dir), onRefresh(btn), onClose })
openDialog({ title, sub, size: 'sm'|'md'|'lg', body, foot, init(el, api), onClose })   // 底栏固定；[data-dlg="cancel"] 自动关闭
await confirmAction({ title, sub, body, impact, confirm: '停用', danger: true, typeToConfirm: '团队1-21' })  // L1 / L2 确认；L0 直接执行 + toast 撤销
toast({ message, tone: 'ok'|'bad'|'info', action: { label: '撤销', run }, timeout })   // 失败不自动消失；最多叠 3 条
registerLayer({ close }) / closeTopLayer()   // Esc 栈：菜单 → 弹层 → 对话框 → 命令面板 → 抽屉
```

节点详情不要直接用 `openDrawer`，用 `app.openNode(id, tab)`（见 `js/node-drawer.js`）。

### 4.6 读数与数据图形（`ui/viz.js`）

| 函数 | 说明 |
|---|---|
| `kpiTile({ key, label, info, value, sub, action: { icon, label, href }, filtered, clickable, pressed, loading })` | KPI 卡，外形照首页 `.overview-card`：高 106，无阴影。主值永远是 `--fg`，状态放在副行。需要可点击的 KPI 时传 `clickable: true`，它会渲染成 button |
| `kpiValue(fmt.rate(x))` / `kpiValue('41', '/ 41')` | 主值 HTML（27 px Inter，单位 12 px 灰） |
| `countDots([['故障', 1, 'bad'], ['降级', 0, 'warn']])` | 副行里带彩色圆点的计数 |
| `patchKpi(root, key, { value, sub })` | 实时更新，只写变化的部分 |
| `meter({ icon, label, value, pct, tone, sub, tip, slot })` / `patchMeter(el, …)` | 资源进度线：12 px 标签，13/600 数值，2 px 细线，11 px 副行。`tone` 用 `fmt.meterTone(v, limit)`，只影响填充色，数字本身不变色 |
| `quota({ pct, tone })` / `quotaTone(pct)` / `patchQuota` | 6 px 额度胶囊，≥90% 为 warn，≥100% 为 bad |
| `latencyStrip(node)` / `patchBeat(el, node)` / `beatModel(node)` | 20 格 6×12 的心跳条，说明行写成 `延迟 12 ms · 丢包 0%`，右侧注明时间窗。没有样本时显示链路证据，都没有时显示空格。整组只占一个 Tab 位，← → 逐格出 tooltip |
| `dayTracker(t30)` / `dayLegend()` | 30 天条（8×16） |
| `await mountChart(el, { data, series: [{ label, color, fill }], kind, height, syncKey, markers, yMin, yMax })` | uPlot 时间序列（`shared/charts.js`）：2 px 细线，不画点，只有横向发丝线网格，坐标轴用 DOM 文字，按北京时间。↓ 与接收系列加 `fill: true` 画淡填充；图例由面板头负责。返回的控制器带 `setData`、`toTable()`、`destroy()` |
| `chartLegend(series)` / `chartTable(ctl.toTable())` | 面板头图例 / 表格视图 |
| 小多图卡 | `.minis > .mini`：`.mini-head`（`.mini-title` + `.mini-value`）+ `.mini-sub` + 图表容器。顶部有一条 2 px 的强调色渐隐线；`.minis.is-2` 表示两列 |

颜色约定：↓ 下行和网卡接收用 `var(--chart-1)` 并画填充；↑ 上行和网卡发送用 `var(--chart-2)`，只画线。只有一个序列时用 `--chart-1`。

### 4.7 排布与状态（`ui/layout.js`）

| 函数 | 说明 |
|---|---|
| `pageHead({ title, note, tools })` + `watchStuck(root)` | 全站同一个页头：静止时无底色，吸顶后才出现半透明画布色、模糊和底线。`tools` 按顺序放：鲜度 → 视图或密度分段 → 主操作 → 次操作 → ↻，控件一律 36 高 |
| `freshness()` | 页头里的鲜度文字。外壳每秒统一刷新（`更新于 15:22:18` / `延迟 · 25 秒前` / `已断开 · 3 分钟前`），悬停列出各数据源的最后更新时间 |
| `panel({ title, count, sub, tools, body, flush })` | 白色面板。以表格开头时传 `flush: true`，表格贴边。面板里不要再套面板，再分区用 `.segment` |
| `kv([{ label, value }], { cols, big })` | 明细：标签在上、值在下，与卡片的 2×2 字段同一写法。`.kv-item.is-wide` 占满整行 |
| `dlRows([{ label, value }])` | 左标签右值的行（主机页签的三组信息） |
| `reading({ label, value, sub })` | 独立读数卡 |
| `note({ tone, title, body, action })` | 提示条：部分失败、风险说明 |
| `empty({ icon, text, action })` / `pageError({ title, reason, impact })` / `skel(w, h)` | 空状态、页面失败、骨架块 |

### 4.8 表格（`ui/table.js` + `css/table.css`，SPEC §3.10）

```js
dataTable({
  columns: [{ key, label, align: 'right', sortable: true, width, cell: (r) => html, sub: (r) => html }],
  rows, getId: (r) => r.id, sortOf: table.sortOf, current: drawerId, two: true, rowLink: true,
  groups: table.groups(),      // 可选：分组行（名称 + 台数 + 小计）
  empty: empty({ text: '没有符合筛选条件的节点', action: { label: '清除筛选', attrs: { 'data-act': 'clear' } } }),
})
tableSkeleton(cols, rows)      // 行高与真实行相同
tableMeter([{ label: 'CPU', value: fmt.pctResource(x), pct: x, tone }])   // 表内两条进度线，宽 120
stepCell('running', { text: '3 分 12 秒', tip })                          // 流水线步骤单元格
```

排序、筛选、分组、光标和 URL 状态用 `shared/table.js` 的 `createTable(nodeTableConfig(store))`，这个助手只负责画。行点击时委托处理 `[data-row]`，排序时委托处理 `[data-sort]`。

- 表头是浅灰带，12/500 灰字。数字列右对齐。行尾是 ›。当前行底色为 `--selected`，首列左侧有 2 px 强调色。
- 单元格排版类：`.v`（数值 13/600 Inter）、`.sub`（次行 12 灰）、`.strong`、`.clip`（单行省略）、`.nowrap`。
- 密度 token（`--row-1`、`--row-2`、`--cell-py`、`--cell-edge`）由 `<main data-density>` 切换，表格不需要另写紧凑样式。
- 运营总览补充的几个选项（其它页面也能用）：
  - `head: false`：不画表头带，用于原版总览里“实时传输排行”“需要关注”那种列表式小表。`tableSkeleton(cols, rows, { head: false })` 与之对应。
  - `sections: [{ title, count, rows }]`：同一张表里分小节（例如“需处理”下方的“未知 16”），小节标题是 `tr.section-tr`（12/500 灰字），列宽上下对齐。没有 `title` 的小节不画标题行。
  - 列的 `cls: 'hide-narrow'`：内容区 ≤480 px（手机）时隐藏这一列，表头和单元格一起隐藏。只用在信息别处已有的列上，例如短名里已含团队时的“团队”列。
  - `patchTable(root, { columns, rows, getId })`：实时补丁。行的集合与顺序没变时，只改写内容变了的单元格并返回 `true`；变了就返回 `false`，由页面决定何时整表重画（按速率排序时 5 秒节流、鼠标或焦点在表里时暂停，§5.3）。它和 `dataTable` 共用同一个单元格渲染函数，写进 DOM 的 HTML 与首绘一致。
  - `expand: { isOpen(row), render(row) }`（线路页补充）：可展开行。行上写 `aria-expanded` 和 `.is-open`，行尾 › 展开时转 90°，首列左侧 2 px 强调色一直延到展开内容；展开内容是紧跟的 `tr.expand-tr > td > .expand-body`（`--sunken` 底、内边距 16）。点击和键盘由页面委托 `[data-row]` 处理。窄内容区里表格横向滚动时，`.expand-body` 吊在可见区域里。展开内容常用 `.dl-grid`（两列 `dlRows`）+ `.expand-actions`（一排 sm 按钮，右对齐；第一个 `.faint` 子元素靠左放说明）。
  - 单元格里的 pill / chip 按行盒顶对齐（线路页补充）：基线对齐会把 20 px 的行盒撑到 23，整行多 3 px。
  - `.cell-link`（表内名称链接，点名称开抽屉）、`.row-actions` + `.row-act`（行内图标按钮：平时 `--fg-3`，行悬停 `--fg-2`）。

### 4.9 命令面板与快捷键

外壳已经装好命令面板（⌘K 或顶栏搜索），分组依次为：最近、跳转、节点、线路机、套餐、操作。全局快捷键包括 `⌘K`、`/`（聚焦 `[data-page-search]`）、`?`、`g h/o/n/r/b/p`、`[ ]`、`Esc`、`⌘⇧L`、`⌘⇧D`。页面自己的快捷键用 `app.keys.bindAll`，首页的例子是 `1/2/3`、`j/k`、`o`。

### 4.10 图标

`icon('chevron-down', 14)` 返回内联 SVG，描边 1.5。图标名用 lucide 的 kebab-case，已收录 89 个，见 `js/icons.js`。大部分从原版产物抽取（lucide-react 0.511 和首页用的 lucide 0.473），产物里没有的 18 个按 lucide 官方路径补写。需要新图标时加到 `ICONS` 里。尺寸：卡内标签 13，按钮和表格 14，侧栏、顶栏、卡头 16。

---

## 5. 节点相关的共享模块

| 模块 | 用途 |
|---|---|
| `plans.js` | `applyPlans(store)` 在启动时执行一次，按 `by_node_id` → `by_asset` → 推导别名表的顺序解析套餐，并**就地替换**成规范名（`n.plan`、`n.plan_id`、`n.billing.plan`、`relays[].plan`、流水线演示行、搜索 haystack）。原始套餐字符串从内存里消失，不可能被渲染出来。其它 API：`planOf(n)`、`allPlans(store)`、`planById(id)`、`quotaLimit(n)`（`不限` / `待核实` / `未设置`）、`planLine(p)`、`membersOf(store, id)`。规格摘要里的供应商原文括注会去掉，方向词“出站”统一换成“发送” |
| `node-model.js` | 卡片、抽屉、表格共用的字段取值：`probeState`、`probeTip`、`offlineText`、`isStale`、`healthOf`、`entryText`（线路机自身显示 `主节点 :20005`）、`planHtml`、`bandwidthF`、`utilOf`、`tagsOf`、`feeF`、`dueHtml`（分级着色 + `LA`）、`uptimeF`、`resourceMeters`（阈值取节点 `limits`）、`nic`、`entryTraffic`、`quotaModel` |
| `node-card.js` | `nodeCard(store, n, { view: 'large'\|'small'\|'compact', current })`、`patchCard(el, store, n)`（只写变化的 `data-s` 槽位，健康变化时 pill 外圈闪一次）、`skeletonCard(view)`（与真实卡等高）、`fitTags(root)`（放不下的标签换成 `+N`，价格 chip 不会被挤掉） |
| `node-drawer.js` | `openNodeDrawer(app, id, tab)` 由外壳按 URL 的 `node` / `tab` 调用。页面只需要调 `app.openNode`。另外导出 `todayNic(store, n)`，它用 host-24h 序列计算今日网卡流量与采样峰值 |

卡片的交互挂在 `data-act` 上，由页面委托处理：`open`、`today`、`access`、`manage`、`plan`。

---

## 6. CSS 约定（SPEC §2、§4.8）

- 只引用 token，不写裸 hex、不写 `!important`，不用 `:is()` 或 `body .x` 抬优先级。选择器最多两级单类。`tokens.css` 的基础样式在 `@layer base` 里，未分层的组件规则天然更高。
- 可用字号只有 11、12、13、15、16、24、27，字重只有 400、500、600，圆角只有 0、4、8、10、14 和胶囊，控件高只有 20、28、32、36。`tools/audit.mjs` 会在运行时逐元素检查。
- 带单位的数值、计数、金额、百分比、时长一律经过 `num()`，得到 `.num` 加 `.unit`。IP、端口、日期用正文字体加 tabular-nums。
- 响应式按**内容区宽度**判断：`.content` 是名为 `content` 的 container，用 `@container content (max-width: …)`。例如侧栏展开时 900 宽视口的内容区只有 612，KPI 会变成两列。只有顶栏、侧栏这类外壳结构才用视口媒体查询。
- 动效只用 `--dur-*` 和 `--ease-*`。在 `prefers-reduced-motion` 下全部为 0。

---

## 7. 首页实现要点（SPEC §6.1）

- 区块顺序与原版相同：页头 → 4 张 KPI → 分组分段 + 排序 → 筛选条（地区 + 套餐）→ 卡片网格。
- KPI 按当前筛选后的节点计算，有筛选时标签后显示 `· 已筛选`。网卡两项的口径与原版相同，只是改了名字，并在 ⓘ 里说明线路机与落地机会重复计量。筛选结果里没有主机时显示 `—`，不显示 0。
- 视图偏好存 localStorage `refined:home:view`。分组、地区、套餐、排序、分组方式存 URL，例如 `#/home?team=团队1&plan=attvps.resi-vps.basic&sort=-netrate&group=plan`。
- 实时更新：订阅 `store.on('change')`，只补丁受影响的卡片槽位和 KPI。审计时实测 30 秒累计布局位移为 0。按网卡速率排序时，重排最多 5 秒一次，用 FLIP 位移，鼠标停在网格上时暂停重排。
- 离线节点按原版规则始终排在最后。离线卡的圆点变红，名称后显示 `离线 N 分钟`，数值按“数据过期”显示旧值并变灰，心跳格变成空心。

## 8. 相对 SPEC 的取舍（都在注释里写了原因）

| 地方 | SPEC | 实际做法 | 原因 |
|---|---|---|---|
| 大卡片 R8（家宽代理） | 左侧写 `持续运行 ··· 无主机` | 左侧放 `上游 TCP 47 ms` | 原代理卡有“上游 TCP”字段，照 SPEC 写会丢掉它（信息密度不能少）。R4 表头已有 `无主机` chip |
| 小卡片 / 紧凑（家宽代理） | 三格：今日下行、本月下行、到期 · 月费 | 三格：本月业务（↓↑）、上游 TCP · 出口、到期 · 月费 | 今日下行已经在入口流量表里，重复了；换成原代理卡的上游 TCP 与出口验证 |
| 筛选条 | 一行，放不下时横向滚动 | 桌面上组内换行（第二行与第一个 chip 对齐）；内容区 ≤900 时套餐组另起一行；≤760 视口才改成一行横向滚动 | 1440 下 6 个套餐放不下一行，横向滚动会藏掉套餐 |
| 连接路径（抽屉） | 字段名 `出站` / `入站 tag` | `转发到` / `入口标签` | V4 要求页面里不出现 `出站`、`入站`（方向词只用于流量） |
| 套餐规格摘要 | 原样显示 `specs.summary` | 摘要里的“出站”换成“发送”，去掉 `（原文「…」）` | 同上，并满足 V5“原始套餐字符串不显示” |

## 9. 运营总览 `#/dashboard`（SPEC §6.2）

`js/pages/dashboard.js`。版面与原版 `/admin/dashboard` 一一对应（`shots/compare-dashboard-light.png`、`compare-dashboard-dark.png` 是同一时刻的并排对照）：页头 → 4 张 KPI → 面板“团队” → 两栏“业务速率排行 │ 需处理” → “24 小时业务流量”图。

| 区块 | 原版 | 精修 |
|---|---|---|
| 页头 | `更新于` · 宽松/紧凑 · 新增节点 · ↻ | 同顺序；鲜度带状态点；刷新时 ↻ 旋转到完成，成功不弹 toast；密度切换与 ⌘⇧D 同步（外壳新增 `page.api.onDensity(v)` 回调） |
| KPI | 状态正常 `39`（整串红）/ 实时传输 `1.3 MiB/s` / 今日流量 / 每月费用 `41 项服务 · USD` | 正常 `39 / 41`（主值不染色，副行 `● 故障 1 · ● 降级 1 · 未知 0`，全部正常时写 `全部正常`）/ 业务速率 `10.6 Mbps`（采样不全时标 `部分` 并写 `覆盖 39/41`）/ 今日业务流量（副行 `↓ · ↑`）/ 每月费用（右上图标去节点页按月费排序）。四个 ⓘ 写口径 |
| 团队表 | 团队 · 节点 · 状态（5 种徽章逐个列出）· 实时速率 ↑↓ 一行 · 今日流量 · 每月费用 · › | 团队（次行 `36 台 · 有活动 3`）· 状态（只列非零 pill，列最小宽 260）· 业务速率（↓ / ↑ 两行）· 今日业务流量（次行 ↓ · ↑）· **本月业务流量**（新增）· 每月费用 · ›；去掉与“N 台”重复的“节点”列。团队行去节点页 `?team=`，线路机行去线路页 |
| 实时传输排行 → 业务速率排行 | 前 5，名称 + IP · 团队 · ↑↓ | 无表头列表外形保留；小号分段 `速率 │ 今日`（URL `rank=today`）；IP 可复制；按速率排序时行序 5 秒节流 + FLIP，鼠标或焦点在面板里时暂停；行点击开抽屉，`[ ]` 在这 5 行里切换 |
| 需要关注 → 需处理 | 完整名 + IP · 检查项 · 徽章 · 原因 · 持续；空 `当前没有待处理的问题`；小节“未知” | 短名 + IP · 检查项（手机上隐藏）· 状态 pill · 原因 · 持续；“未知”是同一张表里的小节行，列对齐；空状态 `当前没有需处理的节点`（绿色 CircleCheck）；有内容时标题带计数、右侧 `在节点页查看`；口径只用 `store.attention()`（T-04） |
| 24 小时流量 → 24 小时业务流量 | 手写 SVG，576 个采样圆点全显示，上传/下载两种蓝分不开，首尾时间 + 峰值 | uPlot：2 px 线、不画点，↓ 下行靛蓝 + 淡填充、↑ 上行橙色只画线；十字线 + tooltip；峰值标记；小号分段 `1 小时 │ 6 小时 │ 24 小时`（URL `range=`，标题跟着变）；表格视图；底部 `起止 · ↓ 峰值 · ↑ 峰值 · 缺测 N 分钟 · 1 分钟桶`；每分钟随实时数据追加 |
| 新增节点 | 向导 `openProvisionWizard('node')` | 全站同一个向导：`js/provision.js`（线路页构建者实现）。`js/enroll-wizard.js` 只是薄封装 `openEnrollWizard(app, { relay })`，节点页也用它 |

实时更新只补丁变化的格子：KPI 用 `patchKpi`，三张表用 `patchTable`，“需处理”成员变化时整块重画并让新行淡入。实测 30 秒累计布局位移 0。键盘：`j / k`（或 ↑↓）在页面所有行之间移动，`Enter` / `o` 打开，团队行换页、节点行开抽屉。首屏骨架（`app.js`）按本页版面画：KPI + 3 行团队表 + 两栏 5 行 + 190 高图表。

验收：`~/.local/bin/node tools/shoot.mjs tools/jobs-dashboard.json` 生成 `shots/dashboard-*.png`（1440 / 375 × 浅色 / 深色，状态回放、紧凑、tooltip、表格视图、骨架、向导三步、抽屉）和两张并排对照；`~/.local/bin/node tools/interact-dashboard.mjs` 跑 31 项交互检查；`~/.local/bin/node tools/audit.mjs '#/dashboard' 30` 跑 V3–V6、V8、V13。

### 9.1 这一页对共享层的补充

| 位置 | 补充 | 原因 |
|---|---|---|
| `ui/table.js` | `head: false`、`sections`、列 `cls` 也加到 `<th>`、`patchTable` | 见 §4.8 |
| `css/table.css` | `tr.section-tr` 小节行；`.hide-narrow`（内容区 ≤480 隐藏） | 需处理 + 未知同表；手机上排行不横向滚动 |
| `css/components.css` | `.empty.is-ok`（空状态图标用 `--ok`）；内容区 ≤600 时 `.panel-head` 折行、工具换到第二行（空工具不占行，图例单独一行）；`.field` 顶端对齐（同一行两个字段有无说明时控件不再错位）；`.radios` / `.radio`（§3.9 单选，向导用）；`.select-wrap > .select` 撑满；图表十字线改实线（uPlot 自带虚线规则后加载，同形选择器加一级覆盖）；图例色标去掉 1 px 圆角（不在圆角档位里） | 组件级缺口 |
| `css/pages.css` | `.page` 显式 `grid-template-columns: minmax(0, 1fr)`，`.cols-2` 加 `min-width: 0` | 页头工具条、两栏面板的最小内容宽曾把 375 宽页面撑到 410（横向溢出） |
| `js/app.js` | 运营总览的首屏骨架；`setDensity` 后回调 `page.api.onDensity(v)` | ⌘⇧D 切密度时页面里的分段要跟着动 |
| `tools/shoot.mjs` | 每个 job 前清掉 `refined:density` | 紧凑截图不再污染后面的截图 |

## 节点行为 `#/behavior`（SPEC §6.5）

`js/pages/behavior.js`。版面与原版 `/admin/behavior` 一一对应（`../original/replica-ops/renders/ops-behavior*.png`、`ops-rules.png`）：页头 → 页面分段 `节点行为 │ 访问规则` → 筛选条 → （选了节点时）上下文条 → 4 张 KPI → 面板 + 下划线页签 `域名与 IP │ 行为变化 ● │ 来源地址 │ 节点概览` + 表 + 表尾分页。

| 区块 | 原版 | 精修 |
|---|---|---|
| 页头 | `更新于` · 宽松/紧凑 · 导出 · 更多 ▾（原始记录）· ↻ | 同顺序；鲜度带状态点；`更多 ▾` 里是 `查看访问记录`（T-10，访问记录页不在本轮原型，点了给说明）和 `复制当前筛选的链接`；导出当前筛选的 JSON（访问规则分段导出规则 JSON） |
| 筛选 | 原生下拉 ×3（团队 · 节点 · 最近 24 小时）+ 灰底搜索框 + 主按钮 `查询` + 折叠 `▶ 更多筛选`（目标类型 · 处理结果）；下拉即时、搜索要回车 | 首页筛选条同一外形（白条、`--r-float`），控件 36 高：团队 · 节点（随团队联动、按团队分组）· 时间范围分段 `1 小时 │ 24 小时 │ 7 天 │ 14 天` · 搜索（防抖 400 ms，Enter 立即，Esc 先清空）· 目标类型 · 处理结果 · `清除`。**全部即时生效，去掉 `查询`**（SPEC §6.5 / dna-ops I4）。目标类型与处理结果只作用于“域名与 IP”，在别的页签里禁用并写明原因；内容区 ≤900 时收进 `筛选 ▾` 菜单 |
| 选择上下文 | 节点短名 · IP · 健康徽章 · `› 节点详情` `◇ 访问规则` | 同内容：短名 600 · IP（可复制）· 团队与入口 · 健康 pill（实时，状态变化时外圈闪一次）· `24 小时 N 条连接 · N 个目标 · N 个来源` · `节点详情`（抽屉“访问记录”页签）· `访问规则`（切到规则分段并选中这个节点）· ✕ 取消节点筛选 |
| KPI | 有活动的节点 · 连接记录 · 访问目标（副行 `各节点分别去重后相加`）· 被拦截的连接（口径只在悬停里） | 外形照首页；`已阻止的连接`（T-05）。副行：`共 41 个节点` · `最近 24 小时` · `按节点去重后求和`（K4）· 已阻止发生在哪个节点（`线路机1 主节点 27`）。四个 ⓘ 写口径。右上图标：`有活动的节点` → 节点概览页签；`已阻止的连接` → 只看已阻止（再点取消）。选了团队或节点时按筛选计算并标 `· 已筛选`；连接记录随实时数据累加 |
| 域名与 IP | 目标 · 节点（团队 + 短名；主节点 + `线路机1 · :20005`）· 来源 · 连接数 · 处理结果 · 最近连接 · ›（证据弹窗）· 第 N 页 | 目标 600（可复制，次行 `域名` / `仅 IP`）· 节点（短名 + ScanSearch：点名称 = 只看这个节点；次行团队；主节点是中性 chip + `线路机1 :20005`）· 来源 `2 个来源` · 连接数（悬停 `累计记录`）· `● 已转发 29,081`（有阻止时再加 `● 已阻止 n`）· 最近连接（悬停首次时间）· ›。数值列可排序（URL `sort=`），表尾 `每页 100 条 · 第 1 页 · 共 40 条` + 上一页 / 下一页 |
| 行为变化 | 卡片列表（短名 ◎ · 类型灰字 · 时间 ✓；新来源次行对象 ›；说明只在 `title` 里） | 改成与其它页签一致的表（SPEC §6.5）：时间（次行绝对时间）· 节点 · 类型（标签 chip）· 对象（新来源 = IP + 运营商画像；连接突增 = `729 次 / 5 分钟` + `平时中位数 118 次`）· 说明（可见，单行省略，悬停全文）· 处理（`标记已查看` → L0 直接生效 + toast `撤销`）。工具条小号分段按类型筛选（带计数）+ `14 天未查看 N 条（不含新域名）`。页签圆点只统计未查看的非“新域名”事件（§9 Q5）。行点击：新来源 → 这个来源在这个节点上的证据；连接突增 → 这个节点最近的连接 |
| 来源地址 | 来源 IP › · 节点 ◎ · 连接 · 目标 | 来源 IP（可复制 + 标记 `新来源` / `境外` / `机房` / `线路机自身`；次行 **运营商 · 城市 · 接入方式**，悬停完整画像）· 节点（1 个时是短名，多个时 `32 个节点`，悬停列出）· 连接（次行会话数）· 目标 · 最近连接（次行首次时间）。选了节点时换成这个节点上的来源（detail.json 的 `ipintel.sources`，数值是这个节点上的连接），“节点”列换成客户端标识，按节点拆分的目标数没有采集，不放这一列 |
| 节点概览 | 节点 ◎（次行 `ip · relay`）· 连接 · 域名 · 来源 · 出口延迟 · 最近活动 · › | 同列；`出口验证`（T-11，按节点阈值着色）；连接次行 `已阻止 n`（红）、域名次行 `仅 IP n`；行 › = 节点抽屉“访问记录”页签，当前行高亮；连接数、最近活动、出口验证实时补丁，行序不跳 |
| 证据对话框 | 标题 = 目标 / 来源，副标题 = 节点；`N 条连接 · 最近 24 小时`；表 5 列；更早记录 · 完整时间线 · 关闭 | lg 对话框；统计行 + 快照里的逐条记录数 + 来源画像；表：时间（次行精确到秒）· 节点（次行团队 · 线路机）· 来源（IP + 标记；次行 `客户端标识 · 运营商 · 城市 · 接入方式`）· 目标（`TCP :443 · 域名`）· 结果 pill；底栏 `查看访问记录` `关闭` |
| 访问规则 | 两栏：选择管理范围（团队默认规则 / 单独配置节点）│ 规则编辑（访问方式 · 域名列表 · 规则备注 · 保存草稿 · 预览影响 · 丢弃全部草稿）；规则发布记录 | 同版面（1 : 3）；范围项右侧标记当前模式或 `草稿`，当前项软靛蓝；编辑区标题带 `已同步` / `待应用` pill；域名列表按服务端规则校验（IP、网址、端口、白名单为空 → 字段下方报错）；草稿只存在内存；`预览影响` → lg 对话框（橙色说明 + 受影响节点表 + 勾选确认后红色 `应用预览中的配置`，原型不真的应用）；`丢弃全部草稿` → L1 红色确认；发布记录空态 `暂无发布`。URL `mode=rules&scope=node:t1-01` |

**URL**（§5.6）：`mode`、`team`、`asset`、`range`、`q`、`type`、`result`、`view`、`page`、`sort`、`ek`、`scope`。外壳把 `node` / `tab` 留给节点抽屉，所以节点筛选叫 `asset`、页签叫 `view`（SPEC §5.6 原写 `node=` / `tab=`，会和抽屉互相覆盖）；抽屉“访问记录”页签的 `查看访问记录` 链接已改成 `#/behavior?asset=<id>`。

**数据**：KPI 来自 snapshot `behavior_kpis` 与各节点 `behavior`；页签数据来自 `extra.json` 的 `behavior`（首屏先出骨架，加载失败时面板里是红色提示条 + `重试`，节点概览不受影响）。快照只有 24 小时汇总：切到 1 小时 / 7 天 / 14 天时出现提示条说明，仍显示 24 小时数据。快照只带全机队前 40 组“节点 × 目标”，选了不在其中的节点时空态会说明，并给出这个节点的 24 小时连接与目标数。

**键盘**：`j / k` 在行间移动，`Enter` / `o` 打开（证据或抽屉），`/` 聚焦搜索，搜索框里 `Esc` 先清空；页签、分段 ← → 切换后焦点留在原控件上；表头排序、标记已查看后焦点不丢。`⌘⇧D` 切密度，页头分段同步。

**验收**：`SHOOT_PORT=9391 ~/.local/bin/node tools/shoot.mjs tools/jobs-behavior.json` 与 `~/.local/bin/node tools/interact-behavior.mjs`（48 项交互检查，同时生成对话框、草稿、紧凑、375、深色等状态截图）生成 `shots/behavior-*.png`（29 张，其中 7 张 `behavior-compare-*.png` 是与原版复刻截图的并排对照）；`~/.local/bin/node tools/audit.mjs '#/behavior' 20`（也跑了 `?view=events|sources|nodes`、`?mode=rules`、`?asset=r1&view=sources`）：V3 字号 / 字重 / 圆角 / 控件高无违规，V4 术语、V5 套餐原文为空，1440 / 1440 深色 / 390 无横向溢出、无控制台报错，实时更新累计布局位移 0。

### 这一页对共享层的补充

| 位置 | 补充 | 原因 |
|---|---|---|
| `js/ui/forms.js`（新） | `select({ name, value, options（可分组）, label, width, disabled, tip })` + `onSelect(root, name, fn)`；`searchBox({ name, value, placeholder, pageSearch })` + `onSearch(root, name, fn, { delay })`（防抖、Enter 立即、Esc 先清空、清除按钮）+ `setSearch` | §3.9 的下拉与搜索框之前只有 CSS，没有统一的 HTML 和行为 |
| `js/ui/intel.js`（新） | 来源画像：`intelOf(store, ip)`、`sourceLine(x)`、`sourceLineHtml(x, { ip, prefix })`（次行 `运营商 · 城市 · 接入方式`，低置信度的接入方式退一级颜色，悬停完整画像）、`sourceTip`、`sourceBadges(x, { isNew, relayName })`（`新来源` / `境外` / `机房` / `线路机自身` 用 `mark` 外形）、`sourceCell(ip, x)` | 行为页、证据对话框、抽屉都要写来源；运营商不按品牌着色，不引入新颜色 |
| `css/components.css` §21 | `.fform`（筛选表单条）、`.fform-more` / `.fform-menu`（≤900 收进菜单）、`.fform-slot`、`.fform-end`；`.select { width: var(--w) }` | 原“筛选面板 + 更多筛选”，访问记录页落地时也用它 |
| §22 | `.ctxbar`（选择上下文条） | SPEC §6.5 的上下文卡 |
| §23 | `.panel > .tabs`（页签左右内边距 = 面板内边距）、`.panel-bar`（页签下的工具条）；`[hidden]` 对 `.panel-bar` / `.table-foot` / `.fform-end` 生效 | 面板里的下划线页签 |
| §24 | `.ilink`（名称 + ScanSearch 的行内动作） | 原 ◎“查看节点行为”，不增加行高 |
| §25 | `.picklist` / `.pick`（当前项软靛蓝） | 访问规则“选择管理范围” |
| §26 | `.src`、`.src-line`、`.src-conf-low`；表格行悬停时显示可复制值的 Copy 图标 | 来源画像 |
| `css/pages.css` | `.rules-grid`、`.rules-list`、`.rules-domains`、`.evidence-table` | 页面排布 |
| `tools/shoot.mjs`、`tools/audit.mjs` | 调试端口可用 `SHOOT_PORT` 覆盖 | 几个构建者同时截图时不抢同一个端口 |
| `js/node-drawer.js` | 访问记录页签的链接 `#/behavior?node=` → `#/behavior?asset=` | 见上面的 URL 说明 |

## 线路 `#/lines`（SPEC §6.4）

`js/pages/lines.js`。版面与原版 `/admin/lines` 一一对应（`../original/replica-ops/renders/ops-lines*.png`；`shots/compare-lines-*.png` 是并排对照）：页头 → 控制条 → 每台线路机一块白面板 → 接入任务 → 管理操作记录。

| 区块 | 原版 | 精修 |
|---|---|---|
| 页头 | `更新于` · 宽松/紧凑 · 新增线路机（主）· 新增家宽节点 · ↻ | 同顺序；鲜度带状态点；↻ 旋转到完成；⌘⇧D 切密度时分段同步 |
| 控制条（新增） | 没有：只能在单台线路机的入口表里搜 | 线路机分段 `全部 41 │ 线路机1 13 │ 线路机2 16 │ 线路机3 12`（URL `relay=`）+ 跨线路机搜索（端口、节点、IP、客户端标识、套餐名；URL `q=`）。搜索时每台线路机只留匹配行，没有匹配的面板折成一行 `线路机3 无匹配入口` |
| 线路机头 | `线路机1` 带宽 1 Gbps · 上行 2.04% · 下行 2.17% · `在线` + 四种异常 chip ……… `更新于 2026/9/23 20:22:32`；描述 `IP · 13 个连接入口 · 13 个家宽节点` | 名称行不变：`线路机1` · 带宽 · **接收 2% · 发送 2%**（D-04，整数）· 健康 pill（`relay.status`，悬停写原因）……… `面板 更新于 15:22:20`（超 30 秒变“延迟”，不可达变红）。描述行：IP（可复制）· 13 个入口 · 12 个节点 · **套餐规范名**（悬停规格摘要）· 到期 8 天 `LA` ……… 状态分项 `探针 在线 · Xray 运行中 · 面板 可达 · 采集 正常 · 配置 已核对`（T-12，异常项换成对应颜色的 pill） |
| 读数行 | CPU / 内存 / 磁盘三条（值是字节，填充近黑）+ TCP 连接 462 与 ↑↓ 速率 | 六格，首页进度线同一外形：CPU（`2 核`）· 内存（`571 MiB / 3.78 GiB`）· 磁盘 · **负载**（`1.63 / 1.61 / 1.73`）· **流量上限**（`1.09 TiB / 1.95 TiB`，≥90% warn）· TCP 连接 1,258（副行网卡接收 / 发送速率）。阈值取节点 `limits`；内容区 ≤1000 三列、≤520 两列 |
| 连接入口 → 入口 | 嵌套面板（圆角 10 的第二层边框）；徽章行；带小标签的原生下拉 ×3 + 灰底搜索框；分页 10/20/50 | 去掉一层边框（A-16），改成面板里的通栏分段：`入口` + `共 13 · 有活动 11 · 停用 0 · 未绑定 0 │ 连接 325 · 业务 ↓ 9.03 Mbps ↑ 471 Kbps`（非零的停用、未绑定用 warn 色）……… `全部团队 ▾` `全部活动 ▾` `全部状态 ▾`（菜单带计数，生效时按钮变软靛蓝；URL `r1.team=` `r1.act=` `r1.state=`）· 新增家宽节点（预选这台线路机）· Xray 日志。入口不到 20 条，取消分页 |
| 入口表 | 端口（等宽）· 绑定节点 · [备注] · 状态 · 活动 · 链路 · 累计用量（先 ↑ 后 ↓）· 出口（等宽 IP + 徽章）· 操作 ⏻ ↺ › | 端口（正文字体 tabular，次行 `vless · reality`）· 绑定节点（短名，点名称开抽屉；次行 `团队1 · ATTVPS 基础版`；主节点 / 未绑定是中性 chip；节点健康不是正常时名称前加彩色圆点）· [备注，只在有入口备注与节点名不同时出现] · 状态 `启用 / 停用` · 活动 `有活动 / 空闲`（次行 `12 个连接` / `上次 3 小时前`）· 链路 `TCP 44 ms` / `出口 492 ms`（超阈值着色）· **业务速率**（新增，↓ / ↑ 两行）· 累计用量（**先 ↓ 后 ↑**，悬停合计与限额）· 出口（IP 可复制，次行 `● 已验证` / `● 不符 · 实际 x`）· 操作 Power / Play、RotateCcw、›。可排序：端口、活动、业务速率、累计用量（URL `sort=-rate`）；按实时值排序时 5 秒节流 + FLIP，指针或焦点在表里时暂停 |
| 展开行 | 两列八项 + 按钮行 停用 · 重置用量 · 编辑备注 · 更换连接凭据 · 重新验证链路 | 同位置、同按钮：入口标签 · 客户端标识 · 连接凭据（前 8 位，可复制）· Reality 目标 / SNI · 转发到（原“出站”）· 备注（行内编辑，Enter 保存、Esc 取消）│ 来源 IP（extra.json 懒加载，每个带运营商画像）· 来源画像（24 小时）· 最近活动 · 连接数 · 出口核对 · 累计合计 · Xray 计数；左下 `打开 团队1-18 详情` |
| 更新详情 | `▶ 软件与数据更新详情`（浏览器默认三角）+ 卡片右下红字 `重启 Xray` | 同一行：折叠（元信息 `3x-ui 3.4.2 · Xray 26.6.27 · 访问记录开启`）……… danger-quiet `重启 Xray`。展开两列：面板版本 · Xray 版本 · 主机探针 · 访问记录 · 运行规则顺序 · 模板版本（悬停四个哈希）│ 运行规则 · 采集耗时 · 日志积压 · 面板自报 · 在线客户端 · 链路探针 |
| 面板不可达 | 表上方橙色 note；按钮靠字符串替换禁用 | 红色提示条 `面板不可达 · <原因>。下表为最近一次采集的入口数据，活动状态不可用。` + `重试`；活动列显示 `—`；行内和展开行的操作全部禁用并写原因；健康 pill 与分项同步变化（`?demo=1` 页内演示条可以造：线路机2 面板不可达、线路机3 Xray 已停止、线路机1 采集中断、线路机1 :20012 停用） |
| 接入任务 | 标题 · 状态 chip · 开始 · 耗时 · ok/总步 · ⌄；展开步骤 + `日志 N` | 折叠行 `新增家宽节点 · 团队1-36` + 步骤状态 pill + `线路机3 · 198.51.100.46 · 开始 15:02:14 · 耗时 1 分 33 秒 · 8/8 步`；展开：结果提示条（成功 ok / 已回滚 warn / 失败 bad）+ 步骤时间线（`timeline`）。步骤名按术语表改：`建入站` → `建入口`、`出站与路由` → `转发与路由`。头部链接 `在上架流水线查看全部`。本页向导发起的演示任务排在最前，带 `演示` 标记，实时推进 |
| 管理操作记录 | 标题 + `查看全部`；`备份完整` · 校验 · 采集队列；表 5 列，动作 chip 没有映射就显示原 key；150 条里 69 条是系统自动记录 | 头部同内容（校验写成 `09-22 15:50`）+ `动作 ▾` `操作者 ▾`（多选，带计数）+ 开关 `显示自动记录`（默认关，表尾写 `已隐藏 69 条自动记录`）。动作中文名补全（`供应商核对`、`清理测试密钥`，`更换 UUID` → `更换连接凭据`）；操作者 `系统` / `上架流程` / `密钥 agent`；详情折叠摘要写字段中文名（最多 4 个）→ 等宽 JSON（`.codeblock`，最高 260 滚动）。**JSON 里的套餐原文经 `scrubPlans` 换成规范名**。默认 10 条，`查看全部` 到 150 条；窄屏整表横向滚动 |

**操作与确认分级**（§3.17）：启用入口、编辑备注、重新验证链路是 L0（直接执行，toast 带撤销）；停用入口、重启 Xray 是 L1（红色按钮，正文第一句写后果，提示条写影响的入口数与连接数）；重置用量、更换连接凭据是 L2（输入节点名后按钮才可用；更换后弹出新凭据，只显示这一次）。原型只改内存里的快照，刷新即还原，toast 都写“原型演示”。

**新增线路机 / 新增家宽节点**：`js/provision.js` 的 `openProvisionWizard(app, 'node' | 'relay', { relayId, onJob })`，全站同一个向导（运营总览、节点页经 `js/enroll-wizard.js` 调它）。lg 对话框、`wizardSteps` 步骤条、底栏固定；字段是原版 §4.1 / §4.2 全集，套餐下拉只列规范名（按供应商过滤，选中带出带宽和月费）；补了 IP 格式、端口范围、编号与出口 IP 重名、日期格式校验；名称预览缩写按供应商（修 OhnoICU 被预览成 NM）；确认页用提示条写风险，不再要勾选“我已确认”；执行页按真实步骤顺序模拟，可以“在后台继续”，进度同步到接入任务面板。

**键盘**：`j / k`（焦点在表里时也可用 ↓ ↑）在所有入口行之间移动；`Enter` / `Space` 展开或收起，`→ / ←` 展开 / 收起；`o` 打开焦点行节点的抽屉；`/` 聚焦搜索，搜索框里 `Esc` 先清空；命令面板选中入口 → `#/lines?entry=线路机2:20015`：清掉挡住它的筛选、展开这一行、滚到视口中间并高亮一次，然后从 URL 去掉 `entry`（展开行不持久化）。

**验收**：`SHOOT_PORT=9451 ~/.local/bin/node tools/shoot.mjs tools/jobs-lines.json` 生成 `shots/lines-*.png`（1440 / 900 / 375 × 浅色 / 深色，展开行、搜索、筛选菜单、按速率排序、紧凑、面板不可达与 Xray 停止、L1 / L2 确认、Xray 日志、向导四态、接入任务与审计、抽屉、首屏骨架）和 5 张 `compare-lines-*.png` 并排对照；`SHOOT_PORT=9455 ~/.local/bin/node tools/interact-lines.mjs` 跑 41 项交互检查；`SHOOT_PORT=9453 ~/.local/bin/node tools/audit.mjs '#/lines' 30`：V3 无违规，V4 术语、V5 套餐原文为空（把 41 个入口、全部任务和 150 条审计详情都展开后再扫一遍也为空），1440 / 1440 深色 / 390 无横向溢出、无控制台报错，30 秒实时更新累计布局位移 0。

### 这一页对共享层的补充

| 位置 | 补充 | 原因 |
|---|---|---|
| `js/ui/table.js` | `dataTable({ expand })` 可展开行（见 §4.8） | §3.10 的展开行之前只有一条 CSS |
| `css/table.css` | 展开行（› 旋转、首列强调色延到展开内容、窄屏吊住展开内容）、`.dl-grid`、`.expand-actions`、`.cell-link`、`.row-actions` / `.row-act`；单元格 pill / chip 顶对齐 | 同上；pill 基线对齐让双行行高从 56 多出 3 px |
| `js/ui/wizard.js`（新） | `wizardSteps(labels, current)` | §3.17 向导步骤条 |
| `css/components.css` “线路页补的通用组件” | `.wiz*` 步骤条；`.meter.is-reading`（与进度线同排的纯读数，轨道留空保持基线）；`.meter-sub` 里的 12 px 图标；`.codeblock`（JSON、公钥）；`.switch-label`（开关 + 文字） | 组件级缺口 |
| `js/provision.js`（新） | 新增线路机 / 新增家宽节点向导；导出 `NODE_STEPS`、`RELAY_STEPS`、`STEP_NAMES` | 全站同一个向导 |
| `js/plans.js` | `scrubPlans(text)`：自由文本里的套餐原文（快照 `billing.plan` 原文、目录 `supplier_plan_name` / `product_line`）换成规范名 | 审计详情、资产备注里原样带着原文，§7 要求任何地方都不显示 |
| `js/app.js` | 命令面板选中入口 → `#/lines?entry=<id>`（原来是 `q=<端口>`，三台线路机都有 20001–20017，会搜出多行）；线路页首屏骨架（控制条 + 线路机面板，无 KPI 行） | §5.4、§3.20 |
| `css/pages.css` “线路” | `.lines-bar`、`.relay-*`、`.entry-*`、`.job-*`、`.audit-*` | 页面排布 |

### 相对 SPEC 的取舍

| 地方 | SPEC | 实际做法 | 原因 |
|---|---|---|---|
| 带宽与利用率 | 移到读数行，作为一个读数 `带宽 1 Gbps（接收 2% · 发送 2%）` | 留在名称行（原版位置），只改方向词和精度 | 名称行 `线路机1 带宽 1 Gbps 上行 2.04% 下行 2.17% ● 在线` 是这一页最有辨识度的一行；读数行换成负载与流量上限，信息只多不少 |
| 状态分项 | 单独一个分段 | 放在描述行右侧 | 省一行高度，头部仍是原版的两行 |
| 读数行 | CPU · 内存 · 磁盘 · 带宽 · TCP 连接 | CPU · 内存 · 磁盘 · 负载 · 流量上限 · TCP 连接 | 带宽在名称行；负载（线路机2 实测 195%）与流量额度（线路机1 已用 56%）是线路机最该看的两个数，原版只在首页卡片上有 |
| 流量上限 | 额度条 6 px 胶囊 | 与同排资源读数一样用 2 px 进度线（阈值 90% / 100% 照额度条） | 一排六格里混一条 6 px 胶囊，副行会错开 4 px |
| 入口表“出口”次行 | pill `已验证` / `不符` | `● 已验证` / `● 不符 · 实际 x`（圆点 + 文字，KPI 副行同一写法） | 次行放 20 px pill 会把双行行高撑高 2 px；颜色和文字都还在 |
| 入口分段标题 | 未写字号 | `入口` 15/600（面板标题级） | 原版“连接入口”嵌套面板标题就是 15/600 |
| 展开行字段名 | `入站 tag`、`出站` | `入口标签`、`转发到` | 与抽屉一致，V4 不出现 `入站` / `出站` |
| Xray 日志 | 真实日志 | 由这台线路机最近的访问记录拼成 Xray access log 格式，对话框里写明“原型演示” | 快照里没有 Xray 日志 |
| 日志 N（接入任务步骤） | 每步可折叠 `日志 N` | 不显示 | 快照的旧任务只带每步耗时与错误，没有日志行 |
| 筛选状态 | SPEC §5.6 只列了 `relay`、`q` | 每台线路机的三个筛选也写 URL：`r2.team=团队1&r2.act=idle&r2.state=disabled` | 业务状态不存 sessionStorage；刷新、分享链接能还原同一屏 |

## 上架流水线 `#/onboarding`（SPEC §6.6）

新增页面，没有原版可对照，所以只用已有组件拼：页头、批次卡（面板）、KPI、面板里的表、StepCell、提示条、抽屉、对话框。版面：页头（上架流水线 · 鲜度 · 宽松/紧凑 · `新建批次` · ↻）→ 批次卡 → 6 张 KPI → 线路机合批 → 上架矩阵 → 全部批次。

| 文件 | 内容 |
|---|---|
| `js/pages/onboarding.js` | 页面：批次卡、KPI、合批、矩阵（阶段 7 列 / 步骤 16 列）、常驻说明行、批次列表、快捷键、URL 状态、每秒计时补丁 |
| `js/onboarding-model.js` | 会话内模型与模拟引擎：演示批次（`store.onboarding().batch`）变成可变模型，进行中的格子按快照时钟计时，合批窗口倒计时、到点合并，操作（提交密码、从这一步重试、再等 10 分钟、跳过、放弃此行、改 IP、暂停、取消未开始的行、新建批次）推动后续步骤；日志、时间线、输入输出由步骤状态与说明推出来（演示） |
| `js/onboarding-drawer.js` | 步骤抽屉（日志 · 时间线 · 输入输出，底部固定按钮）与旧任务抽屉（步骤 · 日志 · 输入输出），同一个 `openDrawer` 外壳 |
| `js/onboarding-dialog.js` | 新建批次向导：`1 录入 ─ 2 预览 ─ 3 选项 ─ 4 开始` |
| `tools/shoot-onboarding.mjs` + `tools/jobs-onboarding.json` | 30 张截图 `shots/onboarding-*.png`（1440 / 375 × 浅色 / 深色；矩阵、步骤 16、筛选、紧凑、6 种抽屉、向导 4 步 + CSV、新批次开始后、首屏骨架）；首屏骨架加在 `js/app.js`。CDP 端口随机挑空闲的，不会连到别人的无头 Chrome |
| `tools/interact-onboarding.mjs` | 55 项检查：筛选与 URL、KPI 点击、j/k/Enter、`[ ]`、Esc 与后退、时间线跳步、日志追加、提交密码、再等、改 IP（重复 IP 被拦）、放弃（L2）、合批到点、向导全流程；每种状态下的 V3 字号/字重/圆角/控件高、V4 术语、V5 套餐原文、V8 横向溢出、V13 控制台；V6 30 秒布局位移（实测 0.0004） |

**页面要点**

- **批次卡**：标题 16/600 + 批次 pill（进行中呼吸点）+ `演示` 标记；两行 12 px 元信息（类型 · 金丝雀 · 操作者 · 开始 · 已用 · 预计剩余；配方一行省略号，悬停出全文）；一条“已上线”分段进度（每行一段，颜色 = 行状态）+ 重装并发 · 合批窗口 · 分配方式。右上 sm `暂停批次 / 继续批次`（L0 + 撤销）、danger-quiet `取消未开始的行`（L1）。
- **KPI 6 张**：总数 · 已上线 `3 / 8` · 进行中（副行按所在步骤计数）· 需处理（`● 等待输入 1 · ● 风险 1`）· 失败（`团队1-42 · 重装验收超时`）· 预计剩余 `7 分钟`（副行 `已用 11 分 41 秒`，每秒走）。已上线 / 进行中 / 需处理 / 失败可点，点了只看这些行（再点取消），每张都有 ⓘ 口径。
- **矩阵**：行 = 主机，列 = 预检 · 密钥 · 重装 · 系统与 SS · 登记与 agent · 线路机 · 验收交付（表头两行：名称 + `done/applicable`）；工具条 `全部 │ 需处理 │ 进行中 │ 失败 │ 已上线` + 搜索（节点、IP、线路机、套餐、服务号）+ `阶段 7 │ 步骤 16`。单元格是 StepCell（成功 = 耗时，进行中 = 实时计时，等待输入 / 超时 / 已停止 = 状态词，跳过 / 排队只有图标），悬停列出这一阶段每一步的状态、耗时、预计和说明。需处理的行首列左侧 2 px 状态色，下面紧跟**常驻说明行**：失败行 `再等 10 分钟 · 打开面板 VNC · 从这一步重试 · 放弃此行`，等待输入行 只写密码框 + `提交` + `打开供应商面板`，风险行 指纹证据（等宽）+ `重新盘点 · 复制工单模板 · 改 IP · 放弃此行`。点格子开那一步的抽屉，点行其它地方开当前步。表格是固定布局（计时文字变长不挤动列宽）；放不下时横向滚动并固定首列，说明行吊在左侧、按钮留在视口里。
- **步骤抽屉**：头 `团队1-42 · 重装验收` + 步骤 pill，副行 `P6 · 耗时 20 分钟 · 预计 5–10 分钟 · IP · 线路机 :端口`；`日志`（等宽 12/20、跟随末尾、搜索高亮与命中数、复制）· `时间线`（16 步，点一步跳过去）· `输入输出`（凭据只写状态、不显示值）；底部固定按钮随状态变（失败：放弃此行 … 再等 · VNC · 从这一步重试；风险：放弃此行 … 改 IP · 重新盘点；等待输入：密码框 + 提交）。`[ ]` 换上一步 / 下一步。
- **全部批次**（批次列表）：新流水线批次 + 22 条旧流程任务（单台也算一个批次），步骤进度是按步数分段的 2 px 条；点新批次切换页面上的批次，点旧任务开任务抽屉。
- **新建批次**：录入（类型单选卡片；粘贴开通邮件或 CSV，逐字识别计数，密码粘进来立刻遮住）→ 预览（可编辑表：IP · 服务号 · 套餐规范名下拉 · 线路机与端口 · 到期与账期时区 · 凭据 `已提供 / 缺少`；重复 IP、已登记服务号、目录外套餐、日期格式报错，出错行不能继续）→ 选项（金丝雀、均衡 / 按录入分配与分配结果、重装并发、合批窗口、只演练、配方、Xray 重载影响）→ 开始（摘要 + `开始上架 N 台`）。开始后新批次成为当前批次：金丝雀先跑，其余 `等待闸门`，缺密码的行停在“推密钥”等待输入。
- **URL**：`batch=`（非最新批次时）`status= q= cols=steps row= step= stab= task= ttab=`；打开抽屉 pushState，浏览器后退关抽屉。快捷键 `n` 新建批次、`j / k` 行、`o` / `Enter` 打开、`c` 切换阶段 / 步骤列、抽屉里 `[ ]`。
- **套餐**：行副行、预览表、输入输出一律 `plans.js` 的规范名（演示行已由 `applyPlans` 换成规范名；新录入的行用 `resolvePlan`）。

### 这一页对共享层的补充

| 位置 | 补充 | 原因 |
|---|---|---|
| `ui/table.js` | `dataTable` 加 `rowAttrs(row)`（追加 tr 属性，class 合并）、`after(row)`（紧跟的常驻说明行 `tr.note-tr > td.note-td`）、`columns[].head` / `thCls`；`stepCell` 加 `fill`、`word`；新增 `stepIcon`、`stepLegend`、`thHead`（两行表头） | 矩阵的需处理行、阶段汇总表头、图例 |
| `css/table.css` | `.th-2l`、`.is-step`（步骤列左右 4）、`.dt.is-pin-first`（首列始终固定，区别于只在窄屏固定的 `is-sticky-first`）、`tr[data-attn]` 首列 2 px 状态色、`td.note-td`、`.step.is-fill`、`.step-legend` | 同上 |
| `ui/flow.js`（新文件，已从 `components.js` 导出） | `choiceCards`（单选卡片）、`stepBar`（按步分段的 2 px 进度条）、`timeline`、`logView` / `logLines` / `followLog`（日志块、搜索高亮、跟随末尾）、`affix`（框内单位）、`secretInput`（只写密码框） | 向导、抽屉、批次列表；别的页面也可以用 |
| `css/components.css` 末尾“上架流水线补的通用组件” | 上面这些组件的样式；`.drawer-foot:empty` 不占位；`.panel-title.is-lg`；`.kpi-grid.is-6`（≤1100 三列、≤760 两列）；`.input-sm` / `.select-sm`（表格里 28 高的可编辑格）；`button.pill`（可点的 pill）；`.kpi.is-clickable` + `.kpi-hit`；`a.ilink` 去下划线；`.log-foot` | 同上 |
| `ui/layers.js` | `openDrawer` 加 `foot`（底部固定按钮条）、`api.setFoot(html)`、`navLabels`（“上一步 / 下一步”） | 步骤抽屉 |
| `ui/viz.js` | `kpiTile` 在 `clickable` 且有 `info` 或 `action` 时改成 article + 铺满的透明按钮 `.kpi-hit` | 原来 `<button class="kpi">` 里再放 ⓘ 按钮，HTML 解析会把外层按钮提前关掉，整张卡散架 |
| `plans.js` | 推导别名表提到模块级；新增 `resolvePlan(provider, text)`（id / 规范名 / 供应商原文 → 目录对象；为空时按供应商最常见的套餐推断，标 `inferred`） | 新建批次把录入的套餐解析成规范名；原文只用于匹配、不显示 |
| `icons.js` | `pause`、`mail`、`clipboard-paste`、`lock`、`skip-forward`、`file-text`、`arrow-right` | 暂停批次、填入示例、跳过、向导“下一步” |
| `css/pages.css` 末尾 | `.ob-*` 排布（批次卡元信息、矩阵列宽与最小宽、说明行、合批倒计时、预览表输入宽度） | 只放排布 |

### 相对 SPEC 的取舍

| 地方 | SPEC | 实际做法 | 原因 |
|---|---|---|---|
| KPI | 4 张：完成 · 需处理 · 失败 · 预计剩余 | 6 张：总数 · 已上线 · 进行中 · 需处理 · 失败 · 预计剩余 | 任务要求的 KPI 条；“完成”改叫“已上线”，与行状态、筛选分段同一个词 |
| 行状态词 | 完成 / 合批中 | 已上线 / 合批中 | 同上（数据里是“已交付”，统一成一个词） |
| 矩阵“操作”列 | 第一个动作 sm 次按钮，其余收进 ⋯ | 只放 ⋯（查看步骤详情、时间线、复制 IP、重试、跳过、放弃此行） | 需处理行的全部按钮已经在紧跟的说明行里，按钮再放一遍会重复，还会把矩阵挤出 1440 的内容区 |
| 行副行顺序 | IP · 套餐 · 线路机 :端口 | IP · 线路机 :端口 · 套餐 | 首列放不下时省略号截掉的是套餐（悬停有全文），入口端口留在视口里 |
| 进行中的阶段格文字 | 各步耗时之和 | 进行中那一步自己的计时（`3 分 13 秒`，与 SPEC 表里的例子一致）；成功格是各步之和 | 阶段和会把已完成的步骤也算进“进行中”，读数不直观 |
| 步骤抽屉页签 | 日志 · 输入输出 · 历史 | 日志 · 时间线 · 输入输出 | 任务要求时间线；“时间线”列出这一行的 16 步（含本步的尝试与说明），替代“历史” |
| 新建批次 | 原型只做第 1、2 步外形 | 4 步都做完，开始后真的在模型里跑 | 任务要求“粘贴 → 预览 → 选项 → 开始” |
| 步骤名 `出站校验`、合批说明里的 `入站` | 原样 | `转发校验`、`入口` | V4：`出站`、`入站` 只允许出现在流量方向上（与节点抽屉的做法一致） |
| “跟随末尾” | — | 复选框（不是开关） | `.check input` 的规则比 `.switch` 更具体，开关放在 `.check` 里会被画成复选框；复选框本来就是 §3.9 的控件 |

## 节点 `#/nodes`（SPEC §6.3）

`js/pages/nodes.js`。版面与原版 `/admin/nodes` 一一对应（`shots/nodes-compare-light.png`、`-dark`、`-drawer`、`-390` 是同一时刻的并排对照）：页头 → 4 张 KPI → 一个面板（工具条 + 表格）。

| 区块 | 原版 | 精修 |
|---|---|---|
| 页头 | `更新于` · 宽松/紧凑 · 新增节点 · 导出 · ↻ | 同顺序；鲜度带状态点；⌘⇧D 切密度时分段跟着动；↻ 旋转到完成，成功不弹 toast；`新增节点` 打开全站同一个向导（`openEnrollWizard(app)`）；`导出` 下载当前视图的 CSV（列同表格，套餐是规范名，带 BOM 给 Excel），toast `已导出 41 行` |
| KPI | 状态正常 `39 / 41`（整串红）· 需处理（橙）· 7 天内到期 · 每月费用 `41 项服务 · USD` | 正常 `39 / 41`（主值不染色，副行 `● 故障 1 · ● 降级 1 · 未知 0` / `全部正常`）· 需处理（副行 `故障 a · 降级 b`；整卡可点 = 切到“需处理”分段，再点取消）· 7 天内到期（副行 `最近：线路机1 · 8 天 LA`；可点）· 每月费用。全机队口径（与原版一致，不随表格筛选变），四个 ⓘ 写口径 |
| 视图分段 + 团队 chip | 分段在面板外（全部 · 需处理 · 待确认 · 7 天内到期 · 尚未接入）；团队是面板里另一种外观的 chip | 两个都放进面板头的工具条第一行，同一种分段：左 `全部 · 需处理 · 未知 · 7 天内到期 · 尚未接入`（T-09 待确认 → 未知），右 `全部 · 团队1 · 团队2 · 线路机`；计数随其它筛选实时变化（dna-ops B2） |
| 搜索 | 灰底搜索框 · 右侧 `41 个节点` | 工具条第二行：搜索框（`/` 聚焦，防抖 150 ms，Esc 先清空）+ `套餐 ▾`（多选，只列规范名 + 台数）+ `线路机 ▾`（多选）+ `分组 ▾`（不分组 / 按套餐 / 按线路机 / 按团队）……… `15 / 41 个节点`。搜索支持 `team:` `relay:` `status:` `plan:` `due:` `provider:` `region:` `cap:` `ip:` token，输入时出补全菜单（↑↓ 选、↵ / Tab 补全）；`plan:` 补全成规范名 `plan:"ATTVPS 基础版"`。有筛选时第三行列出已生效的 chip（可逐个删）+ `清除全部` |
| 表格 | 9 列，不可排序；状态格叠 3 行小字；传输速度 ↑ 在上 | 11 列（SPEC §6.3）：节点 + IP（可复制，复制图标浮在内边距里不占列宽）· 团队 / 入口（`线路机2 :20002`，线路机自身 `主节点 :20005`）· 套餐（规范名；次行 `50 Mbps · ↓<1% ↑2%`，悬停规格与价格）· 状态（pill；次行 `最近检查 刚刚` 或 `探测超时 · 15 分钟`）· TCP / 出口（`TCP 8 ms` / `出口 627 ms`，超阈值着色，`重试中`；线路机 `不验出口`）· 近 30 天（`99.98%` / `覆盖 99%`）· 业务速率（↓ 在上 ↑ 在下）· 本月（次行 `今日 1.69 GiB`；线路机次行是 2 px 额度线 + `额度 56%`）· 主机（CPU / 内存两条进度线，内存写 `165 MiB / 2.57 GiB`；无主机 `无主机` chip；过期 `主机数据过期 · 最后上报 N 分钟前`）· 月费 / 到期（`$28.88` / `20 天`，分级着色 + `LA`）· ›。全部数值列可排序（点表头，Shift 多列，写 URL `sort=-rate,name`）；“团队 / 入口”“TCP / 出口”“月费 / 到期”一列两个排序键 |
| 分组 | 无 | 按套餐 / 线路机 / 团队分组，分组行 = 名称 + `27 台` + 右侧小计 `$779.76/月 · 本月 176 GiB`，可折叠；小计随实时数据更新 |
| 行与键盘 | 行点击开抽屉 | 行点击、Enter / Space、`o` 开抽屉（pushState，后退关）；`j / k`（或 ↑↓）在行间移动；抽屉开着时当前行高亮并滚进视野，`[ ]` 按表格当前顺序切换；点 IP 只复制不开抽屉；选中文字时不当点击 |

**实时**（§5.3）：KPI 用 `patchKpi`，分段计数用 `setSegmentCounts`，单元格用 `patchTable`（tbody 不重建，实测 30 秒累计布局位移 0）；状态变化时行内 pill 外圈闪一次；节点因状态变化离开当前筛选时先降到 0.5 并在 IP 位置写 `已不符合筛选`，5 秒后收起，新进入的行淡入；按业务速率排序时行序最多 5 秒变一次并做 FLIP，鼠标或焦点在表里时暂停；“最近检查 N 秒前”每 30 秒刷新。

**列宽**：11 列在 1440 视口、侧栏展开时的面板内宽（1150）里放下，不横向滚动。做法是固定列宽（`table-layout: fixed`，列宽写在列定义里，合计 1150），实时数值变长不会推动别的列；单元格左右内边距 12 → 10（`.dt.is-tight`，首末列仍是 `--cell-edge`）。唯一放不下的是 OhnoICU 那行的 `100 Mbps · ↓<1% ↑<1%`（省略号 + tooltip 全文）和少数很长的故障原因。内容区更窄（900、390）时表格在面板里横向滚动，首列（节点名）吊在左侧。

**URL**（§5.6）：`status`（分段或状态码）、`team`、`relay`、`plan`（规范套餐 id，与首页、命令面板一致，例如 `#/nodes?plan=attvps.resi-vps.basic`）、`q`、`sort`、`group`；抽屉仍是 `node` / `tab`。首页 KPI 的 `#/nodes?sort=-month`、`?sort=-cost`，运营总览团队行的 `?team=团队1` 都直接落到对应状态。

**抽屉里的 IP 情报**（`js/node-ipintel.js`，只组合现有组件）：
- 连接质量页签，在“连接路径”后面加卡“出口画像”（snapshot `nodes[].ipintel.egress`）：出口 IP（与台账是否一致）· 运营商 + `AS7018 · ATT-INTERNET4` · 接入方式（`固定宽带 住宅`，置信度，悬停判断依据）· IPv6 · 观测次数与首末时间（悬停记录来源）· 位置（两库不一致时标 `两库不一致` 并写出另一库的结果）；出口 IP 换过时是一条橙色提示（时间、前后地址、是否同 ASN / 同地点、上架任务证据）；卡内分段“观测到的出口 IP”逐个列出（`detail.json` 的 `egress_history.ips`：首末时间、次数、上架任务结果、与台账一致 / 不符）。头部 pill `出口 IP 稳定` / `出口 IP 换过 1 次`。
- 访问记录页签，在原版的“最近连接”后面加卡“来源画像”（`ipintel.summary` / `breakdown`，小号分段 `24 小时 │ 14 天`）：来源 IP 数与新增、首位运营商、首位地区与移动网络占比；标记 chip（`24 小时多地接入` 等）；运营商 / 地区 / 接入方式三组分布（前 3 项，用进度线外形）；折叠“IP 情报提醒 N 条”（`ipintel.anomalies`）。再下面是贴边表“来源 IP”（`detail.json` 的 `ipintel.sources`，画像按 IP 到 snapshot 查）：来源 IP + 标记（`ui/intel.js` 的 `sourceCell`，次行 `运营商 · 城市 · 接入方式`，悬停 ASN 与依据）· 最近 / 首次 · 24 小时连接（次行 14 天）· 状态（`新来源 · 未确认` / `活跃` / `已阻止 n`，次行客户端标识）。“最近连接”表的来源列也换成同一种写法。
- 迟到的 detail.json 结果按节点 id 校验，快速切换节点时不会填错。

**验收**：`SHOOT_PORT=9457 ~/.local/bin/node tools/shoot.mjs tools/jobs-nodes.json` 生成 `shots/nodes-*.png`（34 张：1440 / 375 × 浅色 / 深色、整页、紧凑、状态回放、需处理分段、分组、筛选、套餐菜单、搜索补全、空态、首屏骨架、抽屉各页签与 IP 情报、向导，以及 4 张 `nodes-compare-*.png` 并排对照）；`SHOOT_PORT=9457 ~/.local/bin/node tools/interact-nodes.mjs` 跑 47 项交互检查（排序、分段、KPI、ⓘ、多选菜单、分组、token 补全、键盘、抽屉、复制、密度、导出、向导、实时补丁、离开筛选淡出）；`SHOOT_PORT=9457 ~/.local/bin/node tools/audit.mjs '#/nodes' 30`：V3 字号 / 字重 / 圆角 / 控件高无违规，V4 术语、V5 套餐原文为空，1440 / 1440 深色 / 390 无横向溢出、无控制台报错，30 秒布局位移 0。

### 这一页对共享层的补充

| 位置 | 补充 | 原因 |
|---|---|---|
| `ui/overlay.js` | `openMenu(…, { multi: true })`：带 `checked` 的项前面是 16 px 勾选框（`menuitemcheckbox`），点击只切换不关闭，`onSelect(value, item, checked)`；任意菜单项可带 `count`（右侧 12/600 灰字，0 时整项退到 `--fg-3`） | 节点页 `套餐 ▾`、`线路机 ▾` 多选 |
| `ui/table.js` | `dataTable({ cls })` 给 `<table>` 加类；列的 `sorts: [{ key, label }]`（一列两个排序键，中间 `/`）；分组的 `end`（分组行右侧小计，`[data-group-end]` 可实时补丁）；`tableMeter(items, { width })` | 11 列表格、分组小计、主机列放得下 `165 MiB / 2.57 GiB` |
| `css/table.css` | `.dt.is-fixed`（固定列宽，最小宽读容器上的 `--dt-min`）；`.dt.is-tight`（单元格左右 10，› 列 32）；`.dt.is-sticky-first`（内容区 ≤760 时首列吊在左侧）；`td.has-copy`（复制图标浮在右侧内边距）；`tr.is-leaving` / `tr.is-entering`；`td > .pill` 放进 20 的主行、`td .v` 行内盒 16（混排 Inter / PingFang 时行盒不再被撑高）；紧凑密度下次行 12/16（§3.10）；`.tmeter` 每条 16 + 2、宽度可配；次行里的 24 宽额度线 | 行高稳定在 `--row-2` 加 1 px 分隔线；紧凑行 44 |
| `css/components.css` §27 | `.menu-check`；`.menu-item-end .num`；`.toolbar` / `.toolbar-row` / `.toolbar-end` / `.toolbar-count` / `.toolbar-chips` / `.toolbar-label`（面板头里的多行工具条，控件 36 高）；`.btn[aria-pressed="true"]`（有筛选值的下拉按钮是软靛蓝底，和筛选 chip 选中同一种）；`.suggest-hint`（搜索补全菜单底部的键位提示） | 工具条、多选、补全 |
| `css/components.css` §12 | `.drawer-body { grid-auto-rows: max-content }` | 抽屉体高度是定值，内容超出时 `overflow:hidden` 的贴边卡（最近连接、故障记录）会被 grid 按最小贡献压扁；按内容高度排行就不会 |
| `js/node-drawer.js` | 连接质量页签接入“出口画像”，访问记录页签接入“来源画像”“来源 IP”，“最近连接”的来源列改用 `ui/intel.js` | 见上 |
| `js/app.js` | 侧栏分组的点击委托只认 `.nav-group[data-group]`；节点页首屏骨架（两行工具条 + 11 列 8 行） | 表格分组行 `tr[data-group]` 也在外壳里，原来的 `[data-group]` 委托会把它当侧栏分组，点分组行时报错 |
| `tools/shoot.mjs` | `launch()` 的默认端口也读 `CDP_PORT` | 并行截图 |

**相对 SPEC 的取舍**：表头“延迟”写成 `TCP / 出口`（两个排序键，与单元格的前缀一致）；线路机没有出口验证，写 `不验出口`；线路机的“本月”次行用额度线代替 `今日`（原版这里就是额度条）；“来源 IP”表没有单独的 ASN 列（抽屉 760 宽放不下，ASN 在次行悬停里）；分组折叠状态只在内存里。

**已知问题（不在本页范围）**：`shared/format.js` 的原因词 `unstable → 链路不稳定` 会被 `fmt.lintTerms` 的禁用词 `不稳定` 命中（V4）。实时模拟或回放出现这个原因时，任何显示原因的页面都会报。建议在 `REASONS` 里换一个说法（例如 `链路抖动`），由 shared 的维护者决定。
