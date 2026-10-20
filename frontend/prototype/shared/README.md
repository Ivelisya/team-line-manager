# prototype/shared — headless logic for every direction

Vanilla ES modules, no framework, no styling. Every direction (A/B/C or your own) imports these and supplies only markup and CSS. Views should never call `toFixed`, `toLocaleString` or `Intl.*` directly (brief §4); everything goes through `format.js`.

```js
import { fmt, loadSnapshot, createLive, createTable, nodeTableConfig, createKeys, createPalette, paletteItems, defaultBindings, charts } from '../shared/index.js';

const store = await loadSnapshot();            // data/snapshot.json (76 KB gz), clock pinned to as_of
const live  = createLive(store).start();       // simulated 2 s updates, emits minimal diffs
const table = createTable(nodeTableConfig(store, { resortThrottleMs: 5000 }));
table.fromParams(location.search);             // URL → state
store.on('change', (diff) => { table.invalidate(); /* patch changed cells */ });
```

| Module | Size (raw / gzip) | What it does |
|---|---|---|
| `format.js` | 42 KB / 14.6 KB | Brief §4: numbers, units, dates, countdowns, relative time, freshness, placeholders, status words, glossary |
| `data.js` | 39 KB / 12.5 KB | Loads the snapshot; selectors, derived fields (需处理, due tiers), replays, onboarding, lazy files, 200-row stress, table and palette configs for nodes |
| `live.js` | 26 KB / 8.3 KB | Simulated live updates at the real cadences; mutates the store and emits minimal diffs |
| `table.js` | 25 KB / 8.0 KB | Headless table: sort, facets, token search, segments, grouping, selection, cursor, URL state, localStorage preferences, saved views |
| `keys.js` | 17 KB / 6.4 KB | Shortcut registry (⌘K, `/`, j/k, Esc, `g n` …), scopes, help list, headless command palette with fuzzy matching |
| `charts.js` | 33 KB / 11.3 KB | uPlot helpers (time series, mirror ↓/↑), DOM tabular axes, tooltip, plus SVG sparkline, tracker, latency strip and stacked bar |
| `vendor/uplot/` | 51 KB / 22 KB | uPlot 1.6.32. It loads lazily on the first chart mount, so SVG-only pages never fetch it |
| `index.js` | | Barrel re-export |

All six modules together are about 61 KB gzipped, plus 22 KB for uPlot when a chart mounts. The brief's first-screen JS budget is 180 KB gzipped.

Tests: `cd prototype/shared && ~/.local/bin/node --test` runs 71 tests across `*.test.mjs` against the real snapshot. The browser harness is `http://127.0.0.1:5199/shared/smoke.html`, with `?stress=200`, `?rate=20` (more status events) and `?status=attention&sort=-rate` as URL state. It is deliberately plain and is not a design direction.

---

## format.js

Every formatter returns a **`Fmt`**, which stringifies to the display text and also carries the parts a view needs to typeset it:

| field | meaning | example for `bytes(1318123456)` |
|---|---|---|
| `text` | full display string | `1.23 GiB` |
| `prefix` / `num` / `sep` / `unit` / `suffix` | parts, so the unit can be 12 px `--text-2` (§4.1) | `''` / `1.23` / `' '` / `GiB` / `''` |
| `kind` | `value · zero · missing · unset · unlimited · pending · na · stale · unverified · error · word` | `value` |
| `tone` | `null · ok · warn · bad · info · muted · secondary · unknown` → map with `toneVar(tone, 'text'｜'dot'｜'bg')` | `null` |
| `exact` | tooltip text with the precise value | `1,318,123,456 B` |
| `raw` | the input | |

`fmt.html(f, {numClass:'num', unitClass:'unit'})` returns escaped HTML with the number and unit in separate spans. `fmt.esc(s)` escapes a string.

| Area | Functions | Examples (real snapshot values) |
|---|---|---|
| Bytes §4.2 | `bytes(n)`, `bytesPair(d,u)`, `exactBytes(n)` | `0 B` `812 B` `1.23 KiB` `12.3 MiB` `0.98 GiB` (1000 MiB) `38.8 GiB` (today) `1.05 TiB` (month) |
| Rates §4.3 | `rate(bps, {unit:'bit'｜'byte', dir:'down'｜'up'})`, `ratePair(d,u)`, `exactRate(bps)`, `setRateUnit('bit'｜'byte')`, `getRateUnit()` | `9.12 Mbps`, `<1 Kbps`, `0 bps`, `↓ 12.4 Mbps ↑ 1.10 Mbps`. Byte mode gives `1.48 MiB/s` and `<1 KiB/s`. Tooltip: `12,400,000 bit/s（1.48 MiB/s）` |
| Bandwidth | `bandwidth(bps)`, `pctUtil(v)`, `utilPair(d,u)` | `50 Mbps` `1 Gbps` · `<1%` · `↓42% ↑3%` |
| Percent §4.4 | `pctResource(v,{precise})`, `pctLoad`, `loadLabel(p,{cores,windowMin})`, `loadTriple`, `pctAvail`, `pctLoss`, `coverage`, `pctDelta`, `meterTone(v, limit)` | `45%`/`45.3%` · `负载 132%（5 分钟 · 4 核）` · `99.95%`, `100%`, `99.999 → 99.99%` (never rounds up to 100) · `0%` `0.7%` `<0.1%` · `覆盖 96%` · `+3.2%` `−1.0%` · meter tone warn at 0.9 × limit, bad at the limit |
| Latency §4.5 | `latency(ms,{failed,timeout})`, `latencyTone(ms,'tcp'｜'egress', node.limits)` | `42 ms` `1.24 s` `<1 ms` `失败` `超时` `—`. TCP: ≤100 ok, ≤300 warn, above that bad. Egress: above `limits.latency_ms` (default 3000) is warn |
| Money §4.6 | `money(cents,{currency,perMonth})`, `monthlyFee(billing,{perMonth})`, `moneyTotals(items)`, `priceToCents('28.88')`, `monthlyCents`, `cycleKey`, `cycleWord`, `CYCLES` | `$1,281.35` · `$5.00/月` · `EUR 4.00` · `免费` · `待定` (warn) · a quarterly price shows as monthly with tooltip `季付 $15.00` |
| Dates §4.7 | `date(t)`, `stamp(t,{seconds})`, `clockTime(t)`, `absolute(t)`, `bjParts(t)`, `ymdIn(t,tz)`, `dayDiff` | `2026-09-23` · `09-23 15:22` (same year) / `2025-12-31 23:59` · `2026-09-23 15:22:18 北京时间`. Everything is in Beijing time |
| Due §4.7 | `due(billing, t)`, `dueDate(billing)`, `countdown(days,{cycle})`, `dueDays(date,tz,t)`, `dueTier`, `isDue7`, `tzBadge(tz)` | `已过期 3 天` (bad) · `今天到期` (bad) · `1–7 天` (warn) · `26 天` · `长期` · `—`. Days are counted on the **billing zone's** calendar: 线路机1 is `8 天`, badge `LA`. `f.tier` is one of `expired｜today｜soon｜normal｜long｜unset` |
| Relative §4.8 | `ago(t)`, `relative`, `duration(s,{units:1｜2})`, `uptime(s)`, `refreshEvery(t)` | `刚刚` `38 秒前` `2 分钟前` … `09-15 15:22` · `10 分钟后` · `3 分 12 秒` `2 小时 5 分` `12 分钟` · `持续运行 12 天` · timer cadence 1 s under 60 s, else 30 s |
| Freshness §4.8 | `freshness(age, 'komari'｜'xray'｜'monitor'｜'panel'｜seconds)`, `freshnessPill(age, src)`, `INTERVALS`, `stale(fmt, age)` | `{state:'fresh'｜'delayed'｜'stale', word, tone, hollow}` · `实时 · 2 秒前` / `延迟 · 25 秒前` / `已断开` · stale values show hollow, with tooltip `3 分钟前的数据` |
| Placeholders §4.9 | `missing()`, `unset()`, `unlimited()`, `pending()`, `na('无主机')`, `unverified(f)`, `loadFailed()` | `—` `未设置` `不限` `待定` `无主机` `—`+`待核验` chip `读取失败` |
| Status §4.10 | `health(code,{stale})` → `{code,word,tone,dot:'solid'｜'ring'｜'none',rank,modifier}`, `healthCode`, `statusLine(status,t)`, `lifecycle`, `step`, `relayPart(part,ok)`, `entryWord(kind,yes)`, `accessAction`, `incident(inc)`, `apiKey(kind,c)`, `reasonWord`, `checkTitle`, `useDict(snapshot.dict)`; tables `HEALTH LIFECYCLE STEP RELAY_PARTS ENTRY ACCESS INCIDENT API_KEY REASONS CHECKS CAPABILITY` | `正常/降级/故障/维护中/未知` (rank: 故障 0 … 正常 4, 不适用 5) · status line `出口不符 · 12 分钟` / `重试中` / `38 秒前` · steps `排队 进行中 成功 失败 已回滚 跳过 已取消 已停止（风险） 等待输入 等待闸门` |
| Names §4.11 | `shortName(team,n)`, `pad2`, `entryLabel(relay,port,{master})`, `ip(addr,{max})`, `shortId(id)` | `团队1-07` · `线路机2 :20015` / `线路机2 主节点` · IPv6 truncated middle `2001:0db8…0370:7334` (exact in tooltip) · `5117f80b…` |
| Glossary §4.12 | `GLOSSARY`, `lintTerms(text)` | a dev check that flags banned wording such as `已拦截`, `需要关注` or `∞` |
| Clock | `setClock(fn)`, `now()` | `data.js` binds this to the store clock, so every "N 秒前" is relative to the snapshot's `as_of`, never the browser clock |

## data.js

`loadSnapshot(opts?) → Promise<Store>` (fetches `../data/snapshot.json` relative to the module) · `createStore(raw, {base, fetch, bindClock=true, stress, clone})`.

**Entities**

- `nodes()` gives all 41 assets in team, then number, order. Each node gains normalised fields: `teamOrder`, `relayId`, `hasHost`, `isProxy`, `isRelay`, `caps` (adds `'no_host'`), `ipNum` and `haystack`.
- `node(id｜name)`, `relays()`, `relay(id｜name)`.
- `entries()`, `entry('线路机2:20015')`, `entriesFor(relay)`, `entryOf(node)`.
- `teams()`, `providers()`, `site()`, `system()`, `rankings(kind)`, `access(id)`, `behavior()`, `fleetTraffic()`, `upcomingDue(n)`.
- A relay row and its node row share the same live `host`, `bandwidth` and `billing` objects.

**Derived** (all recomputed at the simulated `now()`)

- `attentionOf(n)`: 需处理 is an open failure, or degraded for at least 5 minutes, or the server flag.
- `dueOf(n)` returns `{days, tier, due7}`.
- `staleOf(n)` is true when the monitor has not evaluated the node for more than 5 × 10 s.
- `derived(n)` returns all of the above plus `health`.
- `kpis()` has the same shape as `snapshot.kpis` and is derived from the nodes, so it stays consistent under live updates. At the snapshot it matches exactly: 41/41, $1,281.35, 38.8 GiB.
- `groups()` gives the team summary. It matches `snapshot.groups`, including `active`, which counts nodes at 1 KiB/s or more.
- `attention()` returns `[{node, word, tone, since, duration_s, check_title, reason_word, incident}]`, failures first. `unknown()` lists the 未知 nodes.

**Incidents**

- `incidents({open, node, limit})` and `addIncident(inc)`.
- The full list of 78 is in `extra()`.

**Replays** (these fill the attention list with real past moments)

- `replays()` returns the 5 recorded moments.
- `applyReplay(i)` re-enacts one with its duration preserved. `clearReplay()` undoes it, and `replay` holds the current index. Both emit `change`.
- Index 0 is 团队1-31 故障 (入口认证失败) at 09-23 09:11. Index 4 is 16 hosts 未知 at 09-15 11:21.

**Onboarding**

- `onboarding()` returns `{history[22], batch}`.
- `batch.rows[]` adds `cellMap`, `currentStep`, `stateWord`, `tone`, `attention`, `progress{done, applicable}`, `phaseCells[7]` (the pipeline-spec phases) and `columnCells[10]` (the brief §3.6 columns). `batch.phaseSummary` and `batch.columnSummary` give `done/applicable` per column.
- `aggregateSteps(statuses)` merges several step statuses into one cell, where the worst or most active status wins.

**Lazy files**

- `load('detail'｜'extra'｜'traffic_5m'｜'traffic_1m'｜'host_24h'｜'latency_24h')` is cached and retries after a failure. `prefetch(name)` loads in the background.
- `detail(id)` and `extra()`.
- `series(id, kind)` returns `{t0, step, unit, ...columns}`. `t0` and `step` are filled from the file header when the node's entry lacks them (the 1-minute file).

**Clock**

`now()`, `setNow(t)`, `setClock(fn)`. `live.js` installs a continuous clock.

**Events**

`on('change', fn)` returns an unsubscribe function. `emit`, `version`.

**Stress**

`store.stressed(200)` returns a **new** store with 159 synthetic rows added to the 41 real ones, marked `stress:true`. The rows are seeded, so the output is the same on every run. They get new names that continue each team's numbering, IPs in 192.0.2.0/24 (then 198.18/15), and new relay ports. Rates, traffic, host metrics and latency get jitter. About 10% of due dates fall within 7 days, and about 14% of rows are degraded, failed, unknown or in maintenance. `stressNodes(base, n, {seed})` is the pure function behind it.

**Configs for views**

- `nodeTableConfig(store, extra)` supplies everything `createTable` needs:
  - `nodeColumns`: 27 sort accessors. Status ascending means the worst rows come first, and rate/today/cost/… default to descending.
  - `nodeFacets`: team, relay, status, region, provider, cap, plan, lifecycle.
  - `nodeTokens`: search tokens `team:`, `relay:`, `status:` (codes, words, `attention`/`需处理`, `stale`), `provider:`, `region:`, `cap:host|proxy|relay|no_host`, `ip:` (prefix or `*` wildcard), `plan:`, `due:<7d` (7 days or fewer, including expired), `due:>30`, `due:expired` and `lifecycle:`.
  - `nodeSegments`: 全部 · 需处理 · 未知 · 7 天内到期 · 尚未接入.
  - `NODE_PRESETS`: the built-in saved views.
- `entryTableConfig(store)` is the same for the cross-relay entry table.
- `paletteItems(store, open(kind,id))` returns palette items for nodes, relays and entries. They match on name, number (`21`/`07`), IP, IPv6, service ID, provider, team, relay and client label.

**Utilities**

`get(obj,'a.b')`, `set(obj,'a.b',v) → changed`, `rng(seed)` (mulberry32, with `.normal()` and `.pick()`).

## live.js

`createLive(store, {intervalMs=2000, seed=42, eventRate=1, pauseWhenHidden=true, maxCatchUpS=3600})`

The simulation runs at the real cadences:

- **Host metrics (Komari):** every tick.
- **Business rates (relay collector, 10 s):** updated per relay; mean-reverting log-normal jitter, with the last full hour as the baseline and occasional bursts.
- **Monitor evaluation:** every 10 s. Also flips `status.attention` once a node has been degraded for 5 minutes.
- **TCP probe:** every 30 s. It shifts `latency.strip_tcp` and recomputes the median and loss.
- **Egress probe:** every 300 s.
- **Panel:** every 15 s.

Counters only grow: today, month, cumulative, the hourly bucket, NIC totals, relay quota used and `events_today`. The fleet's 1-minute series and the KPI spark append one point per minute and keep their length. Hour and day rollovers are handled.

Status changes are rare. With the default `eventRate: 1`, about one node per 5 minutes moves to 降级, 故障 or 未知, using the real reasons and failed checks. Each change opens and later resolves an incident, bumps today's tracker incident count, and affects latency, loss and rate. Nodes recover by themselves after 20 s to 10 min.

**Controller**

- `start()`, `stop()`, `running`.
- `tick(dtSeconds)` is the manual and test mode.
- `on(fn)` subscribes to diffs; the store also emits `'change'`.
- `setEventRate(v)`. `triggerEvent()` is for a demo button.
- `age()` returns real seconds since the last tick, which feeds the FreshnessPill.
- `sources()` returns the last update time per source (komari, xray, monitor, panel).
- `clock`.

**Diff shape**

Only values that changed are included. The diff paths are dotted paths into the entity.

```js
{ seq, now, dt, catchUp,
  nodes:   { 't2-01': { 'rate.down_bps': 17012345, 'host.cpu_pct': 3.1, 'latency.strip_tcp': {…}, 'status': {…} } },
  relays:  { r1: { 'host.cpu_pct': 44.8, 'traffic_quota.used_bytes': … } },
  entries: { '线路机1:20010': { rate_down_bps: … } },
  fleet:   { kpis: true, 'kpis.events_today': 140848, 'traffic.fleet_24h': 1, 'kpis.business_rate.spark': 'append' },
  events:  [{ type:'status', id, from, to, reason, check }, { type:'attention', id, attention }, { type:'incident', incident }] }
```

Helpers: `touched(diff, id, 'rate.', 'host.cpu_pct')` checks whether any of those paths changed for a node, and `createPatcher().changed(key, text)` lets you write to the DOM only when the formatted text actually differs. At 200 rows the headless tick costs about 3 ms. With 41 rows, a typical tick changes 6 to 40 visible cells.

## table.js

`createTable(cfg)`. Use `nodeTableConfig(store, extra)` or pass your own config: `rows`, `getId`, `columns{key:{get,type,defaultDir}}`, `facets{key:{get,word,order,label}}`, `tokens{key:{test,facet,values}}`, `haystack`, `segments[]`, `presets[]`, `defaults{sort,density,view,groupBy,hidden,order}`, `storageKey`, `segmentParam='status'`, `resortThrottleMs`, `clock`, `pageSize`, `storage`.

| Group | API |
|---|---|
| Rows | `rows()` (memoised filter and sort), `count()`, `total()`, `indexOf(id)`, `row(id)`, `state()` |
| Sort | `setSort(key,{multi,dir})` (repeat the call to toggle; `multi` is shift-click), `clearSort()`, `sortOf(key)` → `{dir,index,ariaSort}`. Nulls always sort last |
| Filter | `toggleFilter(k,v)`, `setFilter(k,[…])`, `clearFilters(k?)`, `filterValues(k)`, `facetCounts(k)` → `[{value,word,count,selected}]`. Counts apply every other filter, not the facet's own |
| Search | `setQuery(q)` (debounce in the view), `query()`, `parsedQuery()`, `suggest(fragment)` for autocomplete (`rel` → `relay:`; `relay:线` → values). `parseQuery(q, keys)` understands `key:v1,v2`, `-key:v` and `"quoted text"`; IPv6 stays free text |
| Segments | `setSegment(key)`, `segment()`, `segmentCounts()` → `[{key,label,count,active}]` |
| Chips | `chips()` returns the removable active filters (segment, facet and token), each with `remove()`. `reset()` clears them |
| Group | `setGroupBy(facetKey｜null)`, `groups()` → `[{key,word,rows,count,collapsed}]`, `toggleGroup(key)` |
| Prefs (localStorage) | `setDensity`/`density`, `setView`/`view`, `visibleColumns(allKeys)`, `toggleColumn(k,visible?)`, `moveColumn(k,i,all)`, `resetColumns()`. Stored under `teamops:table:<storageKey>`, wrapped in try/catch |
| Saved views | `views()` (presets first), `saveView(name)`, `applyView(name｜view)`, `deleteView(name)`. Stored under `teamops:views:<storageKey>` |
| URL | `toParams(params?)` / `fromParams(params｜string)` for `status` (segment and status facet in one param, e.g. `status=attention` or `status=due7,degraded`), `team`, `relay`, `region`, `provider`, `cap`, `plan`, `q`, `sort=-rate,name` and `view`. Defaults are omitted |
| Selection | `toggleSelect(id,{range})`, `selectAll()` (the visible rows), `clearSelection()`, `isSelected`, `selected()`, `selectedCount()`, `selectionState()` → `none｜some｜all` |
| Cursor | `cursor()`, `setCursor(id)`, `move(±1)`, `moveTo('first'｜'last')`, `page(±1)`, `toggleCursor({range})`. If the cursor row gets filtered out, the cursor lands on its neighbour |
| Keys | `keyBindings({open, close, scope})` returns bindings for `keys.bindAll`: j/↓, k/↑, x, ⇧x, ↵/o, Esc, ⌘A, Home, End |
| Live | `invalidate()` after each diff: membership updates at once, and order is held for `resortThrottleMs` (use 5000 for rate sorts, per the brief). `resort()` forces a re-sort |
| Events | `on(fn)` receives `{reason:'sort'｜'filter'｜'query'｜'segment'｜'group'｜'prefs'｜'selection'｜'cursor'｜'data', reasons}`. `batch(fn)` groups changes into one event |
| Misc | `safeStorage(backing?)` returns `{get,set,remove}` and never throws. `virtualWindow({count,rowHeight,scrollTop,viewport,overscan})` returns `{start,end,padTop,padBottom}` for more than 200 rows |

## keys.js

**`createKeys({sequenceTimeout=1000, mac})`**

- `bind(combo｜[combos], run, {desc, group, scope='global', when, allowInInput, preventDefault=true, hidden})` returns an unbind function. `bindAll(list)` binds several at once.
- `pushScope(name,{exclusive})` returns a pop function. `scopes()` lists them.
- `handle(event)` returns true if a binding ran. It works on plain objects, which is how the tests drive it.
- `start(target=document)`, `stop()`.
- `pending()` returns the in-progress sequence (e.g. `'G'`) for a hint; `onPending(fn)` subscribes to it.
- `list()` returns `[{group, items:[{labels, keys, desc}]}]` for the `?` help overlay.
- Combos look like `mod+k` (⌘ on Mac, Ctrl elsewhere), `/`, `?`, `shift+x`, `g n`, `Escape` and `ArrowDown`.

Rules:

- Keys are ignored during IME composition (Chinese input, `isComposing` or keyCode 229).
- In text fields, only bindings with `allowInInput` fire, plus Escape.
- An abandoned `g` sequence falls back to the next key on its own, so `g` then `j` still moves down.
- An exclusive scope such as the palette blocks lower scopes. Global modifier combos like ⌘K and `allowInInput` bindings still pass through.

**Other helpers**

- `defaultBindings(actions)` returns the brief §2.5 set for whichever handlers you supply: ⌘K `palette`, `/` `focusSearch`, `?` `help`, `g o/n/r/t/b/p/a/s` navigation, `[`/`]` previous/next in the drawer, ⌘⇧L `toggleTheme`, ⌘⇧D `cycleDensity`.
- `parseCombo`, `comboKeys` (`['⌘','K']`) and `comboLabel` (`⌘K`, `G 然后 N`).

**`createPalette({items, groups=['跳转','节点','线路机','操作'], limitPerGroup=8, recentKey, storage})`**

- Items have the shape `{id, group, title, subtitle, keywords[], status, run(), danger, confirm}`.
- Controller: `open()`, `close()`, `isOpen`, `setQuery(q)`, `results()` (flat), `grouped()`, `active()`, `setActive(i)`, `move(±1)` (wraps), `run(i?)`, `armed()`, `keyBindings()` (scope `palette`), `on(fn)`, `invalidate()`.
- A danger item needs a second `run()`. The first call returns `{needsConfirm, message}`.
- An empty query shows 最近 (the last 8 run, kept in localStorage) followed by 跳转.

**`fuzzy(q, text)`** returns `{score, ranges}` for highlighting. Scores: exact 1000, prefix 800, word-boundary substring 600, substring 400, subsequence up to 300. Subsequence matching applies only to queries of 3 or more characters that are not purely numeric, so `21` never matches `:2001`.

## charts.js

**`mountTimeSeries(el, spec)` (async)** returns `{uplot, setData(d), setLoading(bool), resize(), refreshColors(), toTable(), destroy()}`.

spec fields:

- `data`: `[xs, …ys]`, with x in Unix seconds and `null` for a gap.
- `series`: `[{label, color:'var(--chart-1)', fmt?}]`.
- `kind`: `rate｜bytes｜pct｜ms｜count`.
- `mirror`: draws the second series below zero.
- `height`: 160 for panels, 240 for detail. It includes the x-axis band.
- `yMin`, `yMax`, `yTicks`.
- `thresholds`: `[{value,label}]`, drawn as a dashed line with a label.
- `markers`: `[{x,y,label,series}]`, for example a peak.
- `syncKey`: synchronised cursor.
- `gapFactor`: defaults to 1.6. Gaps longer than 1.6 × the median interval are broken, not bridged.
- `tooltip`, `onCursor({idx,x,values})`, `legend` (automatic at 2 or more series), `xTickSpace`, `css:false` (when the page already bundles uPlot's CSS).

It implements brief §5.6:

- 2 px lines with no smoothing and no point markers. The hover point is 8 px with a 2 px ring in `--surface`.
- Horizontal grid only, about 5 round ticks.
- One unit per axis, shown once as a caption (`.ts-unit`); the ticks show numbers only, with fixed decimals.
- Axis labels are **DOM**, not canvas, so they get real `tabular-nums` and the page's numeric font.
- X ticks are aligned to Beijing time, and midnight shows `09-23`.
- The tooltip puts the time first, then one row per series: bold value, then name, with a short line as the colour key.
- The zero baseline uses `--chart-zero`. `toTable()` provides the table view.
- Colours are resolved from CSS variables and re-resolved on `data-theme` or `prefers-color-scheme` changes.
- `setLoading` sets `data-loading` so CSS can dim the last frame.

`mountMirror(el, {xs, down, up, height, syncKey, markers, kind})` draws down (↓) above zero in `--chart-1` and up (↑) below it in `--chart-2`.

DOM classes you can style: `ts-root ts-legend ts-legend-item ts-key ts-axes ts-unit ts-tick ts-tick-x ts-tick-y ts-tip ts-tip-time ts-tip-row ts-threshold ts-marker`, plus uPlot's own `.u-cursor-x`. Only positioning is set inline. Axis text colour defaults to `var(--chart-axis)`.

**Data shaping**: `columnar(series, ['down','up'], {toRate})` → `[xs, …]` (bytes per bucket → bits per second), `xsOf`, `bytesToBps`, `insertGaps(data, 1.6)`, `peakOf(xs, ys)`, `niceTicks`, `yTicks(min,max,kind)` → `{ticks, labels, unit}` (follows `setRateUnit`), `timeTicks(min,max,n)`.

**SVG renderers** return strings, and their colours are `style="fill:var(--…)"`, never hex:

- `sparkline(values, {width=72, height=20, color='var(--text-3)', strokeWidth=1.5, dot=3, min, max, label})` breaks on nulls. A flat zero series sits on the baseline. `updateSparkline(svg, values)` patches one in place, and `sparkPath` is the pure path function behind them.
- `mirrorSparkline(down, up, {width=64, height=24})` is for the KPI business rate. Down and up share one scale.
- `tracker(tracker30, {cell=3, gap=1, height=14, radius=1, focusable})` draws 30 cells 119 px wide. Colours come from `TRACKER_COLORS`: `--tracker-ok` (fallback `--ok-dot`), `--tracker-minor`, `--warn-dot`, `--bad-dot` and `--border-subtle` (not monitored). Cells carry `data-i`, and `trackerTip(t30, i)` gives text like `9/12 · 99.79% · 1 次故障`.
- `latencyStrip(strip, {kind, limits, mode:'cells'｜'bars'})` draws 20 samples. The colour comes from `latencyTone`: an OK sample uses `--latency-ok` (fallback `--ok-dot`), so a direction can keep healthy cells quiet. A lost sample uses `--bad-dot`. `latencyTip(strip,i)` gives `15:21:30 · 42 ms`, and `latencyCaption(strip)` gives `近 9 分钟 · 中位 12 ms · 丢包 0%`.
- `stackBar(segments, {width, height})` with `statusSegments(counts)` draws the team status distribution in the order 正常 · 降级 · 故障 · 未知 · 维护中.

**Colour helpers**: `statusColor(code)` → `var(--ok-dot)`…, `seqColor(v, [lo,hi], {invert})` → `var(--chart-seq-1…5)` (status wall colouring), `microBar(v, {max, limit})` → `{pct, tone}`, `cssRef`, `resolveColor(el, spec)`, `watchTheme(cb)`, `loadUplot()`, `ensureUplotCss()`.

**CSS variables the renderers read** (define them in your direction):

- `--chart-1…2`, `--chart-grid`, `--chart-axis`, `--chart-zero`, `--chart-seq-1…5`
- `--ok-dot`, `--warn-dot`, `--bad-dot`, `--info-dot`, `--unknown-ring`
- `--ok-text`, `--warn-text`, `--bad-text`, `--info-text`
- `--border-subtle`, `--surface`, `--text-1…3`
- optional: `--tracker-ok`, `--tracker-minor`, `--latency-ok`

## Vendored uPlot

- Version 1.6.32, taken from the npm registry tarball `https://registry.npmjs.org/uplot/-/uplot-1.6.32.tgz`.
- The sha512 checksum was verified on download: `KIMVnG68zvu5XXUbC4LQEPnhwOxBuLyW1AHtpm6IKTXImkbLgkMy+jabjLgSLMasNuGGzQm/ep3tOkyTxpiQIw==`.
- Licence: MIT, © 2022 Leon Sorokin (`vendor/uplot/LICENSE`).
- `uPlot.min.mjs` is `dist/uPlot.iife.min.js` with one line appended, `export default uPlot;`. That is 51 KB raw and 22 KB gzipped, against 42 KB gzipped for the unminified ESM build.
- `uPlot.min.css` is unchanged. It only sets structural positioning.

## Data caveats (from data/README.md) worth handling in views

- At the snapshot, all 41 nodes are 正常, and 需处理 and 7 天内到期 are both 0. Use `applyReplay(i)` or `live.triggerEvent()` to show populated states, or `store.setNow(as_of + 2*86400)` to see 线路机1 in the 7-day tier.
- Monitoring began on 09-15, so the first 21 tracker cells are `none` (未监控) and this month's traffic is partial (`traffic.month_partial`).
- The 20-sample strips come from Komari probe tasks. The latency column comes from the team-ops monitor, so the two numbers differ a little.
