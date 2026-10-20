// pages/onboarding.js — 上架流水线 #/onboarding（SPEC §6.6，新增页面，只用已有组件拼）。
//   页头（上架流水线 + 鲜度 + 宽松/紧凑 + 新建批次 + ↻）→ 批次卡 → 6 张 KPI → 线路机合批 → 上架矩阵 → 全部批次。
//   矩阵：行 = 主机，列 = 7 个阶段（或 16 步）；单元格 = StepCell；需处理的行下面紧跟常驻说明行（提示条 + 按钮）。
//   点单元格 / 行 → 步骤抽屉（日志 · 时间线 · 输入输出）；点批次列表的旧任务 → 同一外壳的任务抽屉。
// 状态：批次、行筛选、搜索、列模式、抽屉写在 URL（§5.6：batch= status= q= cols= row= step= stab= task=）。

import { fmt } from '../../../shared/index.js';
import {
  icon, esc, num, on, pageHead, watchStuck, freshness, segmented, onSegment, setSegmented, setSegmentCounts, hydrate,
  kpiTile, kpiValue, patchKpi, countDots, button, iconButton, tonePill, tagChip, mark, note, empty, dataTable,
  stepCell, stepLegend, thHead, stepBar, openMenu, toast, confirmAction, openDialog, searchBox, onSearch, setSearch,
  copyText, hideTip, refreshTip, secretInput,
} from '../components.js';
import {
  obState, tick, BUCKETS, BATCH_WORD, GROUP_WORD, ACTIONS, SKIPPABLE, clean, cellDuration, currentCell, rowWord,
  phaseCell, cellText, batchStatus, kpisOf, groupOf, refreshHistory,
} from '../onboarding-model.js';
import { isLive, guard, isWired } from '../live-mode.js';
import { onboardAvailable, attachOnboard } from '../live-onboard.js';
import { openLiveBatch, confirmStart } from '../onboarding-live-dialog.js';
import { openStepDrawer, openTaskDrawer, closeStepDrawer, stepDrawerOpen, tickStepDrawer, refreshStepDrawer, drawerKey } from '../onboarding-drawer.js';
import { openNewBatch } from '../onboarding-dialog.js';
import { HEAD_TOOLS } from '../page-tools.js';

const DENSITY = [{ value: 'comfortable', label: '宽松' }, { value: 'compact', label: '紧凑' }];
const COLS = [{ value: 'phases', label: '阶段', count: 7 }, { value: 'steps', label: '步骤', count: 16 }];
// 步骤状态 → 分段进度条 / 行状态的对照（行状态画进批次卡的进度条）
const ROW_AS_STEP = { active: 'succeeded', dry_done: 'succeeded', in_relay_batch: 'running', running: 'running', queued: 'queued', waiting_gate: 'queued', waiting_input: 'waiting_input', blocked: 'blocked', failed: 'failed', cancelled: 'skipped' };
const PULSE = new Set(['running', 'in_relay_batch', 'waiting_gate']);
// 新后台批次列表的状态（GET /onboard/batches 的 status）→ 页面的批次状态词
const LIST_STATUS = { validated: 'draft', running: 'running', finished: 'succeeded', attention: 'attention', cancelled: 'cancelled', cancelling: 'cancelling' };
// 重装相关的等待输入：DD 人工闸门（在供应商面板重装后继续），不是补密码
const DD_STEPS = new Set(['P4', 'P5', 'P6']);
const isGate = (r, c) => c && DD_STEPS.has(c.step);

function readP(params) {
  const cols = params.get('cols') === 'steps' ? 'steps' : 'phases';
  const status = BUCKETS.some((b) => b.value === params.get('status')) ? params.get('status') : '';
  return {
    batch: params.get('batch') ?? '', status, q: params.get('q') ?? '', cols,
    row: params.get('row') ?? '', step: params.get('step') ?? '', stab: params.get('stab') ?? 'log',
    task: params.get('task') ?? '', ttab: params.get('ttab') ?? 'timeline',
  };
}

export default {
  title: '上架流水线',
  density: true,
  mount({ el, app, params }) {
    const store = app.store;
    const st = obState(store);
    let P = readP(params);
    // 实时数据 + 新后台开了上架流水线：批次来自 fleetd /v2/api/v1/onboard/*（live-onboard.js），不模拟
    const liveOb = isLive() && onboardAvailable();
    let ob = null;
    const listIds = () => ob?.state.list.map((x) => x.id) ?? [];
    const curId = () => (liveOb ? (P.batch || listIds()[0] || null) : null);
    const batchOf = () => (liveOb ? st.batches.find((b) => b.id === curId()) ?? null : st.batches.find((b) => b.id === P.batch) ?? st.batches[0]);
    let order = []; // 矩阵里当前可见的行顺序（j/k）
    // 实时数据但新后台没开上架流水线（或旧后台）：只显示旧流程的真实接入任务（/jobs），不出现演示批次
    const jobsOnly = isLive() && !liveOb && !st.batches.length;
    if (jobsOnly || liveOb) { refreshHistory(st); app.live?.need?.('jobs'); }

    // ─── 首屏骨架 ───
    el.innerHTML = jobsOnly
      ? `<div class="page" data-page="onboarding">
      ${pageHead({ title: '上架流水线', tools: HEAD_TOOLS.onboarding({ density: app.density() }) })}
      <div data-slot="note"></div>
      <div data-slot="list"></div>
    </div>`
      : `<div class="page" data-page="onboarding">
      ${pageHead({ title: '上架流水线', tools: HEAD_TOOLS.onboarding({ density: app.density() }) })}
      <div data-slot="batch"></div>
      <section class="kpi-grid" data-slot="kpis" aria-label="批次概览"></section>
      <div data-slot="relay"></div>
      <section class="panel" data-slot="matrix" aria-label="上架矩阵"></section>
      <div data-slot="list"></div>
    </div>`;
    const liveNote = () => note({ tone: 'info', title: '上架流水线没有启用', body: esc('新后台还没有配置上架流水线（onboard）。启用后这里显示批次、矩阵与合批。') });
    const slot = (k) => el.querySelector(`[data-slot="${k}"]`);

    // ─── 批次卡 ───
    function batchCard(b) {
      const now = st.now();
      const k = kpisOf(b, now);
      const s = batchStatus(b);
      const [word, tone] = BATCH_WORD[s];
      const canary = b.canary?.enabled ? `金丝雀 ${esc(b.canary.node)}${b.canary.passed ? ' 已通过' : s === 'running' || !b.live ? ' 进行中' : ''}` : '不用金丝雀';
      const unstarted = b.rows.filter((r) => !r.cancelled && r.cells.every((c) => ['queued', 'waiting_gate', 'skipped'].includes(c.status))).length;
      const live = s === 'running' || s === 'paused';
      const tools = b.live ? liveTools(b, s) : live || s === 'attention'
        ? button({ label: b.paused ? '继续批次' : '暂停批次', icon: b.paused ? 'play' : 'pause', size: 'md', attrs: { 'data-act': b.paused ? 'resume' : 'pause', disabled: !live } })
          + button({ label: '取消未开始的行', size: 'md', cls: 'is-danger', attrs: { 'data-act': 'cancel-unstarted', disabled: !unstarted, 'data-tip': unstarted ? `${unstarted} 行还没开始` : '没有还没开始的行' } })
        : '';
      const states = b.rows.map((r) => ROW_AS_STEP[rowWord(b, r).state] ?? 'queued');
      const opt = b.options ?? {};
      const started = b.started_at ? `开始 ${esc(fmt.stamp(b.started_at).text)} · 已用 <span class="num" data-live="elapsed">${esc(fmt.duration(k.elapsed, { units: 2 }).text)}</span>` : '还没开始（计划待确认）';
      const optNote = b.live
        ? `台成功 · 并发 <span class="num">${esc(opt.concurrency ?? 5)}</span> · 合批窗口 <span class="num">${esc(opt.relay_window_s ?? 30)}</span> 秒 · 线路机经 Xray API 热加，不重启${b.plan_hash ? ` · 计划 <span class="mono">${esc(b.plan_hash)}</span>` : ''}`
        : `台成功 · 重装并发 <span class="num">${esc(opt.dd_concurrency ?? 5)}</span> · 合批窗口 <span class="num">${esc(opt.relay_window_s ?? 90)}</span> 秒 · 线路机${esc(opt.relay_strategy ?? '均衡')}分配${opt.dry_run ? ' · 只演练' : ''}`;
      return `<section class="panel" aria-label="当前批次">
        <header class="panel-head ob-head is-wrap"><h2 class="panel-title is-lg">${esc(b.title)}</h2>${tonePill(word, tone, { dot: s === 'running' ? 'pulse' : 'solid' })}${b.demo ? tagChip('演示', '演示批次：按真实供应商、套餐、价格和线路机端口构造，IP、服务号、指纹是虚构的') : ''}<div class="panel-tools">${tools}</div></header>
        <div class="panel-body ob-batch">
          ${b.live && b.error ? note({ tone: 'bad', title: '后台执行出错', body: esc(clean(b.error)) }) : ''}
          <div class="ob-meta">${esc(b.type_word)} · ${canary} · 操作者 ${esc(b.operator ?? '—')} · ${started}${live ? ` · 预计剩余 <span class="num" data-live="eta">${esc(etaWord(k.eta))}</span>` : ''}</div>
          <div class="ob-meta ellipsis" data-tip="${esc(`配方 ${b.recipe}`)}">配方 ${esc(b.recipe)}</div>
          <div class="ob-progress">${stepBar(states, { text: `${k.done.length}/${k.total}`, label: '成功台数' })}<span class="ob-progress-note">${optNote}</span></div>
        </div>
      </section>`;
    }
    const etaWord = (s) => (s <= 0 ? '即将完成' : s < 60 ? `${s} 秒` : `${Math.max(1, Math.round(s / 60))} 分钟`);
    /** 实时批次的批次级操作：待确认 → 确认计划并开始；已中断 / 待介入 → 继续执行；没结束的 → 取消批次（逆序补偿）。 */
    function liveTools(b, s) {
      const t = [];
      if (s === 'draft') t.push(button({ label: '确认计划并开始', icon: 'play', variant: 'primary', size: 'md', attrs: { 'data-act': 'ob-start' } }));
      if (s === 'interrupted' || s === 'attention') t.push(button({ label: '继续执行', icon: 'play', size: 'md', attrs: { 'data-act': 'ob-resume', 'data-tip': s === 'interrupted' ? '执行者已不在（重启或中断）：接管后从停下的步骤继续' : '处理完待介入的行后，让排队的行继续' } }));
      if (!['succeeded', 'cancelled', 'cancelling'].includes(s)) t.push(button({ label: '取消批次', size: 'md', cls: 'is-danger', attrs: { 'data-act': 'ob-cancel' } }));
      return t.join('');
    }

    // ─── KPI（4 张，与全站其它页一样：成功 / 进行中 / 待介入 / 预计剩余，fidelity #10） ───
    function kpiModel(b) {
      const k = kpisOf(b, st.now());
      const live = ['running', 'paused'].includes(batchStatus(b));
      const prov = `${k.providers.map(([p, n]) => `${esc(p)} <span class="num">${n}</span>`).join(' · ')}${k.cancelled ? ` · 已取消 <span class="num">${k.cancelled}</span>` : ''}`;
      return {
        done: { value: kpiValue(String(k.done.length), `/ ${k.total}`), sub: prov },
        running: { value: kpiValue(String(k.running.length)), sub: k.runWhere.length ? k.runWhere.map(([w, n]) => `${esc(w)} <span class="num">${n}</span>`).join(' · ') : '没有进行中的行' },
        attention: { value: kpiValue(String(k.attention.length)), sub: k.attention.length ? countDots([['等待输入', k.waiting.length, 'warn'], ['风险', k.risk.length, 'bad'], ['失败', k.failed.length, 'bad']]) : '没有要人处理的行' },
        eta: live ? { value: k.eta > 0 ? kpiValue(String(k.eta < 60 ? k.eta : Math.max(1, Math.round(k.eta / 60))), k.eta < 60 ? '秒' : '分钟') : kpiValue('即将完成'), sub: `已用 <span class="num">${esc(fmt.duration(k.elapsed, { units: 2 }).text)}</span>` } : { value: kpiValue(null), sub: '没有进行中的行' },
      };
    }
    function kpisHtml(b) {
      const m = kpiModel(b);
      const pressed = (v) => P.status === v;
      return [
        kpiTile({ key: 'done', label: '成功', clickable: true, pressed: pressed('done'), info: '16 步（代理产品 7 步）全部成功、入口已启用的行 / 这个批次的全部行。副行是各供应商的台数。点击只看这些行。', ...m.done }),
        kpiTile({ key: 'running', label: '进行中', clickable: true, pressed: pressed('running'), info: '正在执行某一步，或在排队、等金丝雀、等线路机合批窗口的行。点击只看这些行。', ...m.running }),
        kpiTile({ key: 'attention', label: '待介入', clickable: true, pressed: pressed('attention'), info: '停下来等人的行：等待输入（例如密码被拒）、风险停止（例如主机公钥变了）、失败（没有自动重试）。与全站“需处理”（节点健康）不是一回事。点击只看这些行。', ...m.attention }),
        kpiTile({ key: 'eta', label: '预计剩余', info: '还在跑的行各自剩余步骤的估算中值相加（进行中的步骤扣掉已用时间），行之间并行，取最长的一行。待介入的行不计入。', ...m.eta }),
      ].join('');
    }

    // ─── 线路机合批 ───
    function relayPanel(b) {
      const now = st.now();
      const order_ = { collecting: 0, merging: 1, partial: 2, done: 3 };
      const groups = b.relayGroups.slice().sort((x, y) => order_[x.state] - order_[y.state] || x.relay.localeCompare(y.relay, 'zh-CN', { numeric: true }));
      if (!groups.length) {
        return `<section class="panel"><header class="panel-head"><h2 class="panel-title">线路机合批</h2><span class="panel-sub">窗口内到达同一台线路机的入口合并成一次 Xray 更新</span></header><div class="panel-body is-flush">${empty({ icon: 'network', text: '还没有行走到“线路机”阶段' })}</div></section>`;
      }
      const cols = [
        { key: 'relay', label: '线路机', width: 180, cell: (g) => { const r = store.relay(g.relay); return r ? `<a class="ilink strong" href="#/lines?relay=${esc(r.id)}" data-tip="在线路页查看 ${esc(g.relay)}"><span class="ilink-text">${esc(g.relay)}</span>${icon('arrow-right', 13)}</a>` : `<span class="strong">${esc(g.relay)}</span>`; }, sub: (g) => `第 ${esc(g.batch_no)} 批 · ${g.members.length} 台` },
        { key: 'state', label: '状态', width: 120, cell: (g) => { const [w, t] = GROUP_WORD[g.state] ?? [g.state, 'neutral']; return tonePill(w, t, { dot: g.state === 'collecting' || g.state === 'merging' ? 'pulse' : 'solid' }); } },
        { key: 'impact', label: '说明', cls: 'nowrap', cell: (g) => esc(g.impact || '—'), sub: (g) => esc(g.members.join('、')) },
        { key: 'prog', label: '进度', width: 260, cell: (g) => groupProgress(g, now), sub: (g) => (g.left?.length ? `${esc(g.left.join('、'))} 未进入这一批` : '') },
      ];
      return `<section class="panel" aria-label="线路机合批"><header class="panel-head"><h2 class="panel-title">线路机合批</h2><span class="panel-count">${groups.length} 批</span><span class="panel-sub">合批窗口 ${esc(b.options?.relay_window_s ?? 90)} 秒：窗口内到达同一台线路机的入口合并成一次 Xray 更新，线路机只重载一次</span></header>
        <div class="panel-body is-flush">${dataTable({ columns: cols, rows: groups, getId: (g) => `${g.relay}#${esc(g.batch_no)}`, rowLink: false, label: '线路机合批', cls: 'is-sticky-first' })}</div></section>`;
    }
    function groupProgress(g, now) {
      if (g.state === 'collecting') {
        const left = Math.max(0, Math.ceil((g.window_end ?? now) - now));
        const pct = g.window_s ? (left / g.window_s) * 100 : 0;
        return `<div class="ob-window"><span class="nowrap">窗口剩 <span class="v" data-live="win-${esc(g.relay)}">${left}</span> 秒</span><div class="meter-track"><span class="meter-fill" data-live-bar="${esc(g.relay)}" style="width:${pct.toFixed(1)}%"></span></div></div>`;
      }
      if (g.state === 'merging') return '<span class="t-info">窗口结束，正在合并 Xray 更新</span>';
      return `Xray 重载 <span class="num">${esc(g.xray_reloads)}</span> 次 · 探针重启 <span class="num">${esc(g.probe_restarts)}</span> 次`;
    }

    // ─── 矩阵 ───
    function matchRow(b, r) {
      const w = rowWord(b, r);
      if (P.status && w.bucket !== P.status) return false;
      if (P.q) {
        const hay = [r.name, r.ip, r.relay, r.relay_port, r.plan, r.provider, r.service_id, r.team].filter(Boolean).join(' ').toLowerCase();
        if (!P.q.toLowerCase().split(/\s+/).every((t) => hay.includes(t))) return false;
      }
      return true;
    }
    function counts(b) {
      const c = { '': b.rows.length, attention: 0, running: 0, failed: 0, done: 0 };
      for (const r of b.rows) { const k = rowWord(b, r).bucket; if (c[k] != null) c[k]++; }
      return c;
    }
    function matrixShell(b) {
      const c = counts(b);
      return `<header class="panel-head"><h2 class="panel-title">上架矩阵</h2><span class="panel-count" data-live="mcount">${b.rows.length} 台</span><span class="panel-sub ob-legend">${stepLegend()}</span></header>
        <div class="toolbar"><div class="toolbar-row">
          ${segmented({ name: 'ob-status', items: BUCKETS.map((x) => ({ ...x, count: c[x.value] })), value: P.status, label: '按状态筛选行', disableZero: true })}
          <div class="toolbar-end">${searchBox({ name: 'q', value: P.q, placeholder: '搜索节点、IP、线路机', label: '搜索矩阵的行' })}${segmented({ name: 'ob-cols', items: COLS, value: P.cols, label: '列' })}</div>
        </div></div>
        <div data-slot="table"></div>`;
    }
    function columnsFor(b) {
      const now = st.now();
      const nameCol = {
        key: 'name', label: '节点',
        cell: (r) => `<span class="ob-name"><span class="strong">${esc(r.name)}</span>${r.canary ? tagChip('金丝雀', '先跑这一台，通过后再放开其余') : ''}${r.mode === 'proxy' ? tagChip('代理', '代理产品：无主机权限，跳过重装与系统步骤') : ''}</span>`,
        sub: (r) => `<span class="ob-clip" data-tip="${esc(subLine(r))}">${esc(subLine(r))}</span>`,
      };
      const summary = (codes) => {
        let done = 0, appl = 0;
        for (const r of b.rows) {
          const cells = codes.map((s) => r.cellMap.get(s));
          if (cells.every((c) => c.status === 'skipped')) continue;
          appl++;
          if (cells.every((c) => c.status === 'succeeded' || c.status === 'skipped')) done++;
        }
        return `${done}/${appl}`;
      };
      const stepCols = P.cols === 'steps'
        ? b.steps.map((s) => ({ key: s.code, label: s.name, cls: 'is-step', head: thHead(s.name, summary([s.code])), cell: (r) => oneStep(b, r, s.code, now) }))
        : b.phases.map((ph) => ({ key: ph.key, label: ph.name, cls: 'is-step', head: thHead(ph.name, summary(ph.steps)), cell: (r) => onePhase(b, r, ph, now) }));
      const stateCol = {
        key: 'state', label: '状态', cls: 'ob-statecol',
        cell: (r) => { const w = rowWord(b, r); return tonePill(w.word, w.tone, { dot: PULSE.has(w.state) ? 'pulse' : w.state === 'queued' || w.state === 'cancelled' ? 'ring' : 'solid' }); },
        sub: (r) => stateSub(b, r, now),
      };
      const actCol = { key: 'act', label: '操作', align: 'right', cls: 'ob-actcol', cell: (r) => iconButton({ icon: 'ellipsis', label: `${r.name} 的更多操作`, size: 'sm', attrs: { 'data-act': 'row-menu', 'data-rowid': r.name, 'aria-haspopup': 'menu' } }) };
      return [nameCol, ...stepCols, stateCol, actCol];
    }
    const subLine = (r) => [r.ip, r.relay ? `${r.relay}${r.relay_port ? `:${r.relay_port}` : ' 端口待分配'}` : null, r.plan].filter(Boolean).join(' · ');
    function stateSub(b, r, now) {
      const w = rowWord(b, r);
      const c = currentCell(r);
      const name = b.byCode.get(c.step)?.name ?? c.step;
      if (w.state === 'active' || w.state === 'dry_done') { const appl = r.cells.filter((x) => x.status !== 'skipped').length; return `${r.cells.filter((x) => x.status === 'succeeded').length}/${appl} 步`; }
      if (w.state === 'in_relay_batch') { const g = groupOf(b, r); const left = g?.window_end ? Math.max(0, Math.ceil(g.window_end - now)) : 0; return g?.state === 'merging' ? '正在合并' : `窗口剩 <span class="num" data-live="rwin-${esc(r.name)}">${left}</span> 秒`; }
      if (w.state === 'cancelled') return '已放弃';
      if (w.state === 'waiting_gate' || w.state === 'queued') return esc(clean(c.detail) || '排队');
      return `${esc(c.step)} ${esc(name)}`;
    }
    function tipFor(b, r, codes, now, title) {
      const lines = codes.map((code) => {
        const c = r.cellMap.get(code);
        const d = b.byCode.get(code);
        const dur = cellDuration(c, now);
        const bits = [`${code} ${d.name}`, fmt.step(c.status).word];
        if (dur != null && c.status !== 'queued' && c.status !== 'skipped') bits.push(fmt.duration(dur, { units: 2 }).text);
        if (c.status === 'running' || c.status === 'failed') bits.push(`预计 ${d.estWord}`);
        return bits.join(' · ');
      });
      const focus = codes.map((x) => r.cellMap.get(x)).find((c) => c.detail && !['succeeded', 'skipped'].includes(c.status)) ?? codes.map((x) => r.cellMap.get(x)).find((c) => c.detail);
      return [title, ...lines, focus?.detail ? clean(focus.detail) : null].filter(Boolean).join('\n');
    }
    function onePhase(b, r, ph, now) {
      const pc = phaseCell(r, ph, now);
      const run = pc.cells.find((c) => c.status === 'running');
      const text = pc.status === 'running' ? (run ? fmt.duration(cellDuration(run, now) ?? 0, { units: 2 }).text : fmt.duration(pc.duration_s, { units: 1 }).text) : cellText(pc.status, pc.duration_s, pc.focus?.detail);
      const tip = tipFor(b, r, ph.steps, now, `${ph.name} · ${fmt.step(pc.status).word}`);
      return stepCell(pc.status, { text, tip, fill: true, attrs: { 'data-cell': `${r.name}|${pc.focus?.step ?? ph.steps[0]}`, 'data-codes': ph.steps.join(','), 'aria-label': `${r.name} ${ph.name}：${fmt.step(pc.status).word}${text && text !== fmt.step(pc.status).word ? ` · ${text}` : ''}` } });
    }
    function oneStep(b, r, code, now) {
      const c = r.cellMap.get(code);
      const dur = cellDuration(c, now);
      const text = cellText(c.status, dur, c.detail);
      const d = b.byCode.get(code);
      return stepCell(c.status, { text, tip: tipFor(b, r, [code], now, `${d.name} · ${fmt.step(c.status).word}`), fill: true, attrs: { 'data-cell': `${r.name}|${code}`, 'data-codes': code, 'aria-label': `${r.name} ${d.name}：${fmt.step(c.status).word}${text && text !== fmt.step(c.status).word ? ` · ${text}` : ''}` } });
    }
    // 需处理的行：常驻说明行（提示条 + 按钮组，sm）
    function noteRow(b, r) {
      const w = rowWord(b, r);
      const id = esc(r.name);
      const act = (label, a, variant = 'secondary', ic) => button({ label, icon: ic, variant, size: 'sm', attrs: { 'data-ract': a, 'data-rowid': r.name } });
      if (b.live) return liveNoteRow(b, r, w, act);
      if (w.state === 'failed') {
        const c = r.cells.find((x) => x.status === 'failed');
        return `<div class="ob-note">${note({ tone: 'bad', body: esc(clean(c.detail) || '这一步失败') })}<div class="ob-note-actions">${c.step === 'P6' ? act('再等 10 分钟', 'wait', 'secondary', 'timer') + act('打开面板 VNC', 'vnc', 'secondary', 'external-link') : ''}${SKIPPABLE.has(c.step) ? act('跳过', 'skip', 'quiet', 'skip-forward') : ''}${act('从这一步重试', 'retry', 'primary', 'rotate-ccw')}${act('放弃此行', 'abandon', 'danger-quiet')}</div></div>`;
      }
      if (w.state === 'waiting_input') {
        const c = r.cells.find((x) => x.status === 'waiting_input');
        const tries = r.attempts[c.step] ?? 0;
        const text = r.secret === 'rejected' ? `密码被拒（邮件密码可能不对），已尝试 ${Math.max(1, tries)}/2 次；请从供应商面板复制 root 密码` : clean(c.detail);
        return `<div class="ob-note">${note({ tone: 'warn', body: esc(text) })}<div class="ob-note-actions">${secretInput({ name: `secret-${r.name}`, size: 'sm', width: 240, label: `${r.name} 的 root 密码`, attrs: { 'data-secret-for': r.name } })}${act('提交', 'secret', 'primary')}${button({ label: '打开供应商面板', variant: 'link', size: 'sm', iconEnd: 'external-link', attrs: { 'data-ract': 'provider', 'data-rowid': r.name } })}</div></div>`;
      }
      if (w.state === 'blocked') {
        const detail = r.risk?.detail ? `<div class="mono ob-fp">${esc(clean(r.risk.detail))}（演示值）</div>` : '';
        return `<div class="ob-note">${note({ tone: 'bad', body: `${esc(r.risk?.message ?? clean(r.cells.find((x) => x.status === 'blocked')?.detail) ?? '已停止')}${detail}` })}<div class="ob-note-actions">${act('重新盘点', 'retry', 'secondary', 'scan-search')}${act('复制工单模板', 'ticket', 'secondary', 'copy')}${act('改 IP', 'ip', 'secondary', 'pencil')}${act('放弃此行', 'abandon', 'danger-quiet')}</div></div>`;
      }
      void id;
      return '';
    }
    /** 实时批次的说明行：后台没有的动作（跳过、再等、改 IP、打开面板）不出现。 */
    function liveNoteRow(b, r, w, act) {
      const detail = (st_) => clean(r.cells.find((x) => x.status === st_)?.detail) || clean(r.reason) || '';
      if (w.state === 'failed') return `<div class="ob-note">${note({ tone: 'bad', body: esc(detail('failed') || '这一步失败') })}<div class="ob-note-actions">${act('放弃此行', 'abandon', 'danger-quiet')}${act('从这一步重试', 'retry', 'primary', 'rotate-ccw')}</div></div>`;
      if (w.state === 'waiting_input') {
        const c = r.cells.find((x) => x.status === 'waiting_input');
        if (isGate(r, c)) {
          return `<div class="ob-note">${note({ tone: 'warn', body: esc(detail('waiting_input') || '请在供应商面板重装后继续') })}<div class="ob-note-actions">${secretInput({ name: `secret-${r.name}`, size: 'sm', width: 240, label: `${r.name} 重装后的 root 密码（选填）`, attrs: { 'data-secret-for': r.name } })}${act('重装完成，继续', 'continue', 'primary', 'play')}</div></div>`;
        }
        return `<div class="ob-note">${note({ tone: 'warn', body: esc(detail('waiting_input') || '需要 root 密码') })}<div class="ob-note-actions">${secretInput({ name: `secret-${r.name}`, size: 'sm', width: 240, label: `${r.name} 的 root 密码`, attrs: { 'data-secret-for': r.name } })}${act('提交', 'secret', 'primary')}</div></div>`;
      }
      if (w.state === 'blocked') return `<div class="ob-note">${note({ tone: 'bad', body: esc(r.risk?.message || detail('blocked') || '已停止（风险）') })}<div class="ob-note-actions">${act('复制工单模板', 'ticket', 'secondary', 'copy')}${act('放弃此行', 'abandon', 'danger-quiet')}${act('已处置，重试', 'retry', 'primary', 'rotate-ccw')}</div></div>`;
      return '';
    }
    function tableHtml(b) {
      const rows = b.rows.filter((r) => matchRow(b, r));
      order = rows.map((r) => r.name);
      const drawerRow = P.row && batchOf() === b ? P.row : null;
      return dataTable({
        columns: columnsFor(b), rows, getId: (r) => r.name, rowLink: false, label: '上架矩阵',
        cls: P.cols === 'steps' ? 'is-pin-first ob-matrix is-steps' : 'is-pin-first ob-matrix',
        rowAttrs: (r) => {
          const w = rowWord(b, r);
          const attn = w.state === 'failed' || w.state === 'blocked' ? 'bad' : w.state === 'waiting_input' ? 'warn' : null;
          return { class: `is-link${r.name === drawerRow ? ' is-current' : ''}${w.state === 'cancelled' ? ' is-muted' : ''}`, tabindex: '0', 'data-attn': attn, 'aria-label': `${r.name}：${w.word}` };
        },
        after: (r) => noteRow(b, r),
        empty: empty({ icon: 'inbox', text: b.rows.length ? '没有符合筛选条件的行' : '这个批次还没有行', action: b.rows.length ? { label: '清除筛选', attrs: { 'data-act': 'clear' } } : null }),
      });
    }
    function renderTable() {
      const b = batchOf();
      const box = slot('matrix').querySelector('[data-slot="table"]');
      // 保留：横向滚动位置、正在输入的密码、焦点所在行
      const wrap = box.querySelector('.table-wrap');
      const sl = wrap?.scrollLeft ?? 0;
      const typing = [...box.querySelectorAll('[data-secret-for]')].map((i) => [i.dataset.secretFor, i.value, i === document.activeElement]);
      const focusRow = document.activeElement?.closest?.('tr[data-row]')?.dataset.row;
      const focusCell = document.activeElement?.dataset?.cell;
      if (box.contains(document.activeElement) || box.matches(':hover')) hideTip();
      box.innerHTML = tableHtml(b);
      const w2 = box.querySelector('.table-wrap');
      if (w2) w2.scrollLeft = sl;
      for (const [rid, v, f] of typing) { const i = box.querySelector(`[data-secret-for="${CSS.escape(rid)}"]`); if (i) { i.value = v; if (f) i.focus(); } }
      syncPin();
      if (focusCell) box.querySelector(`[data-cell="${CSS.escape(focusCell)}"]`)?.focus({ preventScroll: true });
      else if (focusRow && !typing.some((t) => t[2])) box.querySelector(`tr[data-row="${CSS.escape(focusRow)}"]`)?.focus({ preventScroll: true });
    }
    function renderMatrix() {
      slot('matrix').innerHTML = matrixShell(batchOf());
      renderTable();
      hydrate(slot('matrix'));
    }
    // 阶段列在宽屏放得下时不固定首列（没有多余的竖线）；放不下、要横向滚动时才固定（步骤 16 总是固定）
    function syncPin() {
      const wrap = slot('matrix')?.querySelector('.table-wrap');
      const t = wrap?.querySelector('table');
      if (!t) return;
      const need = P.cols === 'steps' || wrap.scrollWidth > wrap.clientWidth + 1;
      if (t.classList.contains('is-pin-first') !== need) t.classList.toggle('is-pin-first', need);
    }
    const ro = new ResizeObserver(() => syncPin());
    if (!jobsOnly) ro.observe(slot('matrix'));

    // ─── 全部批次（批次列表：新流水线批次 + 旧流程任务，单台也算一个批次） ───
    /** 新后台的批次摘要 → 列表用的最小批次（完整视图没读过的批次只有计数）。 */
    function summaryBatch(x) {
      const loaded = st.batches.find((b) => b.id === x.id);
      if (loaded) return loaded;
      const states = [...Array(x.active ?? 0).fill('active'), ...Array(x.attention ?? 0).fill('failed'), ...Array(Math.max(0, (x.total ?? 0) - (x.active ?? 0) - (x.attention ?? 0))).fill('queued')];
      return { id: x.id, live: true, summary: true, title: x.title, type_word: '家宽节点', operator: x.operator, started_at: x.started_at ?? x.created_at ?? null, finished_at: x.finished_at ?? null,
        apiStatus: LIST_STATUS[x.status] ?? 'draft', states, total: x.total ?? 0 };
    }
    const summaryStates = (b) => (b.summary ? b.states.map((s_) => ROW_AS_STEP[s_] ?? 'queued') : b.rows.map((r) => ROW_AS_STEP[rowWord(b, r).state] ?? 'queued'));
    function listPanel() {
      const cur = batchOf();
      const items = [
        ...(liveOb ? ob.state.list.map((x) => ({ kind: 'batch', id: x.id, b: summaryBatch(x) })) : st.batches.map((b) => ({ kind: 'batch', id: b.id, b }))),
        ...st.history.map((h) => ({ kind: 'task', id: h.id, h })),
      ];
      const cols = [
        { key: 'title', label: '批次', cls: 'nowrap', cell: (x) => (x.kind === 'batch' ? `<span class="strong">${esc(x.b.title)}</span>` : `<span class="strong">${esc(x.h.title)}</span>`), sub: (x) => (x.kind === 'batch' ? `<span class="mono">${esc(x.b.id)}</span>${x.b.demo ? ' · 演示批次' : x.b.created ? ' · 本次会话新建' : x.b.live ? ' · 新流水线' : ''}` : `任务 <span class="mono">${esc(x.h.id)}</span> · 旧流程`) },
        { key: 'type', label: '类型', width: 100, cell: (x) => tagChip(x.kind === 'batch' ? x.b.type_word : x.h.type_word) },
        { key: 'count', label: '台数', align: 'right', width: 64, cell: (x) => `<span class="v">${esc(x.kind === 'batch' ? (x.b.summary ? x.b.total : x.b.rows.length) : x.h.count)}</span>` },
        { key: 'prog', label: '步骤进度', width: 190, cell: (x) => {
          if (x.kind === 'batch') { const states = summaryStates(x.b); return stepBar(states, { text: `${states.filter((s) => s === 'succeeded').length}/${states.length}`, label: '成功台数' }); }
          return stepBar(x.h.steps.map((s) => s.status), { text: `${x.h.ok_steps}/${x.h.total_steps}`, label: '成功步数' });
        } },
        { key: 'status', label: '状态', width: 110, cell: (x) => {
          if (x.kind === 'batch') { const s = batchStatus(x.b); const [w, t] = BATCH_WORD[s]; return tonePill(w, t, { dot: s === 'running' ? 'pulse' : 'solid' }); }
          const [w, t] = { succeeded: ['成功', 'ok'], rolled_back: ['已回滚', 'warn'], failed: ['失败', 'bad'] }[x.h.status] ?? [x.h.status_word, 'neutral'];
          return tonePill(w, t);
        }, sub: (x) => (x.kind === 'task' && x.h.error ? `<span class="ob-clip" data-tip="${esc(x.h.error)}">${esc(x.h.error)}</span>` : '') },
        { key: 'start', label: '开始', width: 104, cell: (x) => { const t = x.kind === 'batch' ? x.b.started_at : x.h.started_at; return t ? `<span class="nowrap" data-tip="${esc(fmt.stamp(t).exact ?? '')}">${esc(fmt.stamp(t).text)}</span>` : '<span class="t-muted">未开始</span>'; } },
        { key: 'dur', label: '耗时', align: 'right', width: 104, cell: (x) => {
          if (x.kind === 'batch') {
            if (!x.b.started_at) return '<span class="t-muted">—</span>';
            const live = ['running', 'paused'].includes(batchStatus(x.b));
            return `<span class="v" ${live ? `data-live="bdur-${esc(x.b.id)}"` : ''}>${esc(fmt.duration(Math.max(0, (x.b.finished_at ?? st.now()) - x.b.started_at), { units: 2 }).text)}</span>`;
          }
          return `<span class="v">${esc(fmt.duration(Math.max(1, Math.round(x.h.duration_s)), { units: 2 }).text)}</span>`;
        } },
        { key: 'op', label: '操作者', width: 80, cell: (x) => { const o = x.kind === 'batch' ? x.b.operator : x.h.operator; return o ? esc(o) : '<span class="t-muted" data-tip="旧流程没有记录操作者">—</span>'; } },
      ];
      const current = P.task || cur?.id;
      return `<section class="panel" aria-label="全部批次"><header class="panel-head"><h2 class="panel-title">全部批次</h2><span class="panel-count">${items.length} 个</span><span class="panel-sub">${jobsOnly ? '旧流程的接入任务（最近 50 条）；家宽节点 8 步、线路机 11 步' : liveOb ? '新流水线的批次（16 步）在前；旧流程的单台接入任务在后' : '单台上架也算一个批次；旧流程家宽节点 8 步、线路机 11 步'}</span></header>
        <div class="panel-body is-flush">${dataTable({ columns: cols, rows: items, getId: (x) => `${x.kind}:${x.id}`, current: P.task ? `task:${P.task}` : `batch:${current}`, rowLink: true, label: '全部批次', cls: 'is-sticky-first' })}</div></section>`;
    }

    let renderedBatch = null;
    function renderAll() {
      if (jobsOnly) { slot('note').innerHTML = liveNote(); slot('list').innerHTML = st.history.length ? listPanel() : `<section class="panel"><div class="panel-body">${empty({ icon: 'inbox', text: '最近没有接入任务' })}</div></section>`; hydrate(el); return; }
      const b = batchOf();
      if (!b) { renderNoBatch(); return; }
      renderedBatch = b.id;
      slot('batch').innerHTML = batchCard(b);
      slot('kpis').innerHTML = kpisHtml(b);
      slot('relay').innerHTML = relayPanel(b);
      renderMatrix();
      slot('list').innerHTML = listPanel();
      hydrate(el);
    }
    /** 结构变化后：重画批次卡、KPI、合批、矩阵表体（保留输入与滚动）、批次列表；工具条只更新计数。 */
    /** 实时：还没读到批次、没有批次、流水线没启用、或读失败时的整页状态（批次卡位置），列表照常。 */
    function renderNoBatch() {
      renderedBatch = null;
      const S = ob?.state;
      const text = !S?.loaded ? '正在读取批次' : S.disabled ? '新后台没有启用上架流水线' : S.error ? `读取批次失败：${S.error}` : curId() ? '正在读取这个批次' : '还没有批次';
      const action = S?.loaded && !S.disabled && !S.error && !curId() ? { label: '新建批次', attrs: { 'data-act': 'new' } } : null;
      slot('batch').innerHTML = `<section class="panel"><div class="panel-body">${empty({ icon: S?.error ? 'circle-alert' : 'inbox', text, action })}</div></section>`;
      for (const k of ['kpis', 'relay', 'matrix']) slot(k).innerHTML = '';
      slot('list').innerHTML = (ob?.state.list.length || st.history.length) ? listPanel() : '';
      hydrate(el);
    }
    function renderChange() {
      if (jobsOnly) { refreshHistory(st); renderAll(); markCurrent(); refreshStepDrawer(); return; }
      if (liveOb) refreshHistory(st);
      const b = batchOf();
      if (!b || b.id !== renderedBatch || !slot('matrix').firstElementChild) { renderAll(); return; }
      slot('batch').innerHTML = batchCard(b);
      const m = kpiModel(b);
      const k = slot('kpis');
      for (const key of ['done', 'running', 'attention', 'eta']) patchKpi(k, key, m[key]);
      slot('relay').innerHTML = relayPanel(b);
      const seg = slot('matrix').querySelector('[data-seg="ob-status"]');
      if (seg) setSegmentCounts(seg, counts(b));
      const mc = slot('matrix').querySelector('[data-live="mcount"]');
      if (mc) mc.textContent = `${b.rows.length} 台`;
      renderTable();
      slot('list').innerHTML = listPanel();
      hydrate(el);
    }

    // ─── 每秒：只改计时文字（§5.3 规则 1–2，不重画、不跳位） ───
    function renderTick() {
      const b = batchOf();
      if (!b || renderedBatch !== b.id) return;
      const now = st.now();
      const k = kpisOf(b, now);
      const setText = (sel, t) => { const e = el.querySelector(sel); if (e && e.textContent !== t) e.textContent = t; };
      setText('[data-live="elapsed"]', fmt.duration(k.elapsed, { units: 2 }).text);
      setText('[data-live="eta"]', etaWord(k.eta));
      const m = kpiModel(b);
      patchKpi(slot('kpis'), 'eta', m.eta);
      for (const g of b.relayGroups) {
        if (g.state !== 'collecting') continue;
        const left = Math.max(0, Math.ceil((g.window_end ?? now) - now));
        setText(`[data-live="win-${CSS.escape(g.relay)}"]`, String(left));
        const bar = el.querySelector(`[data-live-bar="${CSS.escape(g.relay)}"]`);
        if (bar) bar.style.width = `${((left / (g.window_s || 90)) * 100).toFixed(1)}%`;
        for (const mbr of g.members) setText(`[data-live="rwin-${CSS.escape(mbr)}"]`, String(left));
      }
      for (const bb of st.batches) setText(`[data-live="bdur-${CSS.escape(bb.id)}"]`, fmt.duration(Math.max(0, now - bb.started_at), { units: 2 }).text);
      // 进行中的格子：只改文字与 tooltip
      for (const cell of slot('matrix').querySelectorAll('.step[data-step="running"][data-cell]')) {
        const [rid, code] = cell.dataset.cell.split('|');
        const r = b.rows.find((x) => x.name === rid);
        if (!r) continue;
        const codes = cell.dataset.codes.split(',');
        let html;
        if (codes.length > 1) { const ph = b.phases.find((p) => p.steps.join(',') === cell.dataset.codes); html = onePhase(b, r, ph, now); }
        else html = oneStep(b, r, code, now);
        const t = document.createElement('template');
        t.innerHTML = html;
        const fresh = t.content.firstElementChild;
        const n1 = cell.querySelector('.num'), n2 = fresh.querySelector('.num');
        if (n1 && n2 && n1.textContent !== n2.textContent) n1.textContent = n2.textContent;
        if (cell.dataset.tip !== fresh.dataset.tip) { cell.dataset.tip = fresh.dataset.tip; refreshTip(cell); }
        const al = fresh.getAttribute('aria-label');
        if (cell.getAttribute('aria-label') !== al) cell.setAttribute('aria-label', al);
      }
      tickStepDrawer();
    }

    // ─── 抽屉（URL 驱动；打开 = pushState，浏览器后退 = 关闭） ───
    let pushed = false, closingByRoute = false;
    const dctx = {
      st,
      onNav(dir) {
        if (P.task) { const list = st.history; const i = list.findIndex((h) => h.id === P.task); const nx = list[(i + dir + list.length) % list.length]; app.setParams({ task: nx.id }, { replace: true }); return; }
        const b = batchOf();
        const i = b.steps.findIndex((s) => s.code === P.step);
        const nx = b.steps[(i + dir + b.steps.length) % b.steps.length];
        app.setParams({ step: nx.code }, { replace: true });
      },
      onStep(code) { app.setParams({ step: code, stab: 'log' }, { replace: true }); },
      onTab(t) { app.setParams(P.task ? { ttab: t === 'timeline' ? null : t } : { stab: t === 'log' ? null : t }, { replace: true }); },
      onClose() {
        if (closingByRoute) return;
        if (pushed) { pushed = false; history.back(); }
        else app.setParams({ row: null, step: null, stab: null, task: null, ttab: null }, { replace: true });
      },
      act: (a, r, step, payload) => act(a, r, step, payload),
    };
    function openRow(rowName, step) {
      const open = stepDrawerOpen();
      if (!open) pushed = true;
      app.setParams({ row: rowName, step, stab: null, task: null, ttab: null }, { replace: open });
    }
    function openTask(id) {
      const open = stepDrawerOpen();
      if (!open) pushed = true;
      app.setParams({ task: id, ttab: null, row: null, step: null, stab: null }, { replace: open });
    }
    function syncDrawer() {
      const b = batchOf();
      if (P.task) {
        const t = st.history.find((h) => h.id === P.task);
        if (t) { openTaskDrawer(dctx, t, P.ttab); return; }
      } else if (P.row && b) {
        const r = b.rows.find((x) => x.name === P.row);
        if (r) {
          const step = r.cellMap.has(P.step) ? P.step : currentCell(r).step;
          openStepDrawer(dctx, { batch: b, row: r, step, tab: ['log', 'timeline', 'io'].includes(P.stab) ? P.stab : 'log' });
          if (b.live && !b.events.length) ob.events(b.id).then(() => refreshStepDrawer(), () => {});
          return;
        }
      }
      if (stepDrawerOpen()) { closingByRoute = true; closeStepDrawer(); closingByRoute = false; pushed = false; }
    }
    function markCurrent() {
      for (const tr of slot('matrix')?.querySelectorAll('tr[data-row]') ?? []) tr.classList.toggle('is-current', tr.dataset.row === P.row);
      for (const tr of slot('list').querySelectorAll('tr[data-row]')) tr.classList.toggle('is-current', tr.dataset.row === (P.task ? `task:${P.task}` : `batch:${batchOf()?.id}`));
    }

    // ─── 操作 ───
    async function act(a, r, step, { batch = batchOf(), anchor, password } = {}) {
      if (batch?.live) return liveAct(a, r, { batch, anchor, password });
      const s = step || currentCell(r).step;
      const sname = batch.byCode.get(s)?.name ?? s;
      if (a === 'retry') { ACTIONS.retry(st, batch, r, s); toast({ message: s === 'P1' && r.risk ? `${r.name} 重新盘点：再扫描 10 次主机公钥` : `${r.name} 从“${sname}”重新开始`, tone: 'info' }); }
      else if (a === 'wait') { ACTIONS.waitMore(st, batch, r, s); toast({ message: `${r.name} 的${sname}再等 10 分钟`, tone: 'info', action: { label: '撤销', run: () => { const c = r.cellMap.get(s); c.status = 'failed'; c.duration_s = 1200; c.since = null; c.target_s = null; c.extended = false; c.detail = '重装验收超时（20 分钟）：expected 公钥未出现，主机不可达 14 分钟；可能卡在安装环境'; st.emit('change'); } } }); }
      else if (a === 'skip') { ACTIONS.skip(st, batch, r, s); toast({ message: `${r.name} 已跳过“${sname}”` }); }
      else if (a === 'vnc') toast({ message: `原型不会打开 ${r.provider} 面板；真实环境会在新标签页打开服务号 ${r.service_id} 的 VNC`, tone: 'info' });
      else if (a === 'provider') toast({ message: `原型不会打开 ${r.provider} 面板；真实环境会打开服务号 ${r.service_id} 的管理页`, tone: 'info' });
      else if (a === 'secret') { ACTIONS.submitSecret(st, batch, r); toast({ message: `已提交 ${r.name} 的密码（只写，不回显），重新推送密钥` }); }
      else if (a === 'ticket') {
        const text = `【${r.provider} 工单】服务号 ${r.service_id} / IP ${r.ip}\n问题：该 IP 上有两台机器应答 SSH（主机公钥不一致），我们已停止上架、未发送密码。\n证据：${clean(r.risk?.detail ?? '')}\n请确认该 IP 是否分配给了其他客户，或更换一个新的 IP。`;
        const ok = await copyText(text, null);
        toast({ message: ok ? '已复制工单模板' : '复制失败：剪贴板被拒绝', tone: ok ? 'ok' : 'bad' });
      } else if (a === 'ip') changeIpDialog(batch, r);
      else if (a === 'abandon') {
        const yes = await confirmAction({ title: `放弃此行 ${r.name}`, sub: `${r.ip} · ${r.provider} · 服务号 ${r.service_id}`, body: '这一行还没完成的步骤全部取消，预留的入口端口会释放；已完成的步骤不会回滚，主机不会被删除。', impact: '不可撤销：要重新上架这台机器，需要在新批次里再录入一次。', confirm: '放弃此行', danger: true, typeToConfirm: r.name });
        if (yes) { ACTIONS.abandon(st, batch, r); toast({ message: `已放弃 ${r.name}` }); }
      }
      void anchor;
    }
    /** 实时批次的行操作（新后台 actions + resume / cancel）。 */
    async function liveAct(a, r, { batch, anchor, password }) {
      if (a === 'ticket') return act_ticket(r);
      if (a === 'ip') { toast({ message: '改 IP：先放弃此行，再用新 IP 建一个批次（新后台按 IP + SSH 端口识别机器）', tone: 'info', timeout: 8000 }); return; }
      if (!['retry', 'secret', 'continue', 'abandon'].includes(a)) { toast({ message: '新后台暂不支持这个操作', tone: 'info' }); return; }
      if (guard('onboarding.row')) return;
      const busy = (on_) => { if (anchor) { anchor.disabled = on_; anchor.toggleAttribute('aria-busy', on_); } };
      const risk = rowWord(batch, r).state === 'blocked';
      if (a === 'abandon') {
        const yes = await confirmAction({ action: 'onboarding.row', title: `放弃此行 ${r.name}`, sub: `${r.ip} · ${r.provider}${r.service_id ? ` · 服务号 ${r.service_id}` : ''}`,
          body: '已完成且可以补偿的步骤会逆序撤销：线路机上本行的入口按 tag 撤下、预留端口作废；主机不会被删除，也不会重装回去。',
          impact: '不可撤销：要重新上架这台机器，需要在新批次里再录入一次。', confirm: '放弃此行', danger: true, typeToConfirm: r.name });
        if (!yes) return;
        busy(true);
        try { const out = await ob.cancel(batch.id, r.name); toast({ message: out?.status === 'cancelling' ? `正在放弃 ${r.name}：这一步跑完后逆序补偿` : `已放弃 ${r.name}，补偿完成` }); }
        catch (e) { if (!e.auth) toast({ message: `放弃 ${r.name} 失败：${e.message}`, tone: 'bad' }); }
        finally { busy(false); ob.refresh(); }
        return;
      }
      if (a === 'retry' && risk) {
        const yes = await confirmAction({ action: 'onboarding.row', title: `重试风险停止的行 ${r.name}`, sub: `${r.ip} · ${r.provider}`,
          body: `这一行因为“${clean(r.risk?.message || r.reason || '身份或冲突检查')}”停下，没有发送密码、没有改动。只有在你已人工确认（例如联系供应商、核对主机公钥）之后才重试。`,
          impact: '重试会从停下的步骤重新检查；如果风险仍在，会再次停下。', confirm: '已处置，重试', danger: true, typeToConfirm: r.name });
        if (!yes) return;
      }
      busy(true);
      try {
        const action = a === 'secret' ? 'provide_input' : a === 'continue' ? (password ? 'provide_input' : 'continue') : 'retry';
        const out = await ob.rowAction(batch.id, r.name, action, { password, force: a === 'retry' && risk });
        const tail = out?.resumed?.running ? '（批次正在执行，会接着处理这一行）' : '';
        toast({ message: `${out?.message || `${r.name} 将继续`}${tail}` });
      } catch (e) {
        if (!e.auth) toast({ message: `${r.name}：${e.message}`, tone: 'bad' });
      } finally { busy(false); ob.refresh(); }
    }
    async function act_ticket(r) {
      const text = `【${r.provider} 工单】服务号 ${r.service_id ?? '—'} / IP ${r.ip}\n问题：${clean(r.risk?.message || r.reason || '上架预检发现主机身份异常')}\n我们已停止上架、未发送密码。\n请确认该 IP 是否分配给了其他客户，或更换一个新的 IP。`;
      const ok = await copyText(text, null);
      toast({ message: ok ? '已复制工单模板' : '复制失败：剪贴板被拒绝', tone: ok ? 'ok' : 'bad' });
    }
    function changeIpDialog(batch, r) {
      const dlg = openDialog({
        title: `改 IP ${r.name}`, sub: `现在 ${r.ip} · ${r.provider} · 服务号 ${r.service_id}`, size: 'sm',
        body: `<label class="field"><span class="field-label">新的 IP</span><input class="input" data-newip placeholder="例如 198.51.100.221" autocomplete="off" spellcheck="false"><span class="field-hint">供应商换了 IP 之后填这里；改完从“预检”重新开始</span><span class="field-error" data-iperr hidden></span></label>`,
        foot: button({ label: '取消', attrs: { 'data-dlg': 'cancel' } }) + button({ label: '改 IP 并重新预检', variant: 'primary', attrs: { 'data-ipok': '' } }),
        init(d) {
          const input = d.querySelector('[data-newip]');
          const err = d.querySelector('[data-iperr]');
          const ok = () => {
            const v = input.value.trim();
            const bad = !/^(\d{1,3})(\.\d{1,3}){3}$/.test(v) || v.split('.').some((x) => Number(x) > 255) ? '不是有效的 IPv4 地址'
              : store.nodes().some((n) => n.ip === v) ? `与在用节点 ${store.nodes().find((n) => n.ip === v).name} 重复`
              : v === r.ip ? '和现在的 IP 一样' : '';
            if (bad) { err.hidden = false; err.textContent = bad; input.setAttribute('aria-invalid', 'true'); input.focus(); return; }
            ACTIONS.changeIp(st, batch, r, v);
            dlg.close();
            toast({ message: `${r.name} 的 IP 已改为 ${v}，重新预检` });
          };
          d.querySelector('[data-ipok]').addEventListener('click', ok);
          input.addEventListener('keydown', (e) => { if (e.key === 'Enter') ok(); else { err.hidden = true; input.removeAttribute('aria-invalid'); } });
        },
      });
    }
    function rowMenu(btn, r) {
      const b = batchOf();
      const c = currentCell(r);
      const w = rowWord(b, r);
      const items = [
        { value: 'open', label: '查看步骤详情', icon: 'logs' },
        { value: 'timeline', label: '查看时间线', icon: 'history' },
        { value: 'copy-ip', label: `复制 IP ${r.ip}`, icon: 'copy' },
      ];
      if (['failed', 'blocked'].includes(w.state)) items.push({ value: 'retry', label: `从“${b.byCode.get(c.step).name}”重试`, icon: 'rotate-ccw' });
      if (!b.live && SKIPPABLE.has(c.step) && ['running', 'failed'].includes(c.status)) items.push({ value: 'skip', label: `跳过“${b.byCode.get(c.step).name}”`, icon: 'skip-forward' });
      if (!['active', 'dry_done', 'cancelled'].includes(w.state)) items.push({ sep: true }, { value: 'abandon', label: '放弃此行', danger: true });
      openMenu(btn, {
        label: `${r.name} 的操作`, items,
        onSelect: (v) => {
          if (v === 'open') openRow(r.name, c.step);
          else if (v === 'timeline') { openRow(r.name, c.step); app.setParams({ stab: 'timeline' }, { replace: true }); }
          else if (v === 'copy-ip') copyText(r.ip, null).then((ok) => toast({ message: ok ? `已复制 ${r.ip}` : '复制失败', tone: ok ? 'ok' : 'bad' }));
          else act(v, r, c.step);
        },
      });
    }

    // ─── 首绘 + 事件 ───
    if (liveOb) {
      ob = attachOnboard(st, {
        current: () => curId(),
        wantEvents: () => (P.row && stepDrawerOpen() ? curId() : null),
        onChange: () => { if (!el.isConnected) return; renderChange(); syncDrawer(); markCurrent(); refreshStepDrawer(); },
        onError: (e) => { if (!e?.auth && el.isConnected) { ob.state.error = e.message; if (!batchOf()) renderNoBatch(); } },
      });
      ob.refresh();
    }
    renderAll();
    syncDrawer();
    // 其它页的“新增家宽节点”跳过来（#/onboarding?new=1）：直接打开新建批次
    if (params.get('new') === '1') { app.setParams({ new: null }, { replace: true }); setTimeout(() => { if (el.isConnected) newBatch(); }, 0); }
    const offs = [watchStuck(el)];
    offs.push(onSegment(el, 'density', (v) => app.setDensity(v)));
    offs.push(onSegment(el, 'ob-status', (v) => app.setParams({ status: v || null })));
    offs.push(onSegment(el, 'ob-cols', (v) => app.setParams({ cols: v === 'steps' ? 'steps' : null })));
    offs.push(onSearch(el, 'q', (v) => app.setParams({ q: v || null })));
    offs.push(on(el, 'click', '[data-act]', (e, b_) => {
      const a = b_.dataset.act;
      const b = batchOf();
      if (a === 'new') newBatch();
      else if (a === 'ob-start' || a === 'ob-resume' || a === 'ob-cancel') liveBatchAct(a, b_);
      else if (a === 'refresh' && liveOb) { b_.classList.add('is-busy'); ob.refresh().finally(() => b_.classList.remove('is-busy')); }
      else if (a === 'refresh' && isLive() && app.live.refresh) { b_.classList.add('is-busy'); const done = () => { b_.classList.remove('is-busy'); if (el.isConnected) renderChange(); }; app.live.refresh({ force: ['slow'] }).then(done, done); }
      else if (a === 'refresh') { b_.classList.add('is-busy'); renderChange(); setTimeout(() => b_.classList.remove('is-busy'), 600); }
      else if (!b || b.live) return;
      else if ((a === 'pause' || a === 'resume' || a === 'cancel-unstarted') && guard(a === 'cancel-unstarted' ? 'onboarding.batch.cancelUnstarted' : 'onboarding.batch.pause')) return;
      else if (a === 'pause') { ACTIONS.pause(st, b, true); toast({ message: '批次已暂停：进行中的步骤跑完后不再开始新步骤', tone: 'info', action: { label: '撤销', run: () => ACTIONS.pause(st, b, false) } }); }
      else if (a === 'resume') { ACTIONS.pause(st, b, false); toast({ message: '批次已继续' }); }
      else if (a === 'cancel-unstarted') {
        const list = b.rows.filter((r) => !r.cancelled && r.cells.every((c) => ['queued', 'waiting_gate', 'skipped'].includes(c.status)));
        confirmAction({ title: `取消未开始的 ${list.length} 行`, sub: b.title, body: `${list.map((r) => r.name).join('、')} 还没开始任何一步，取消后不会占用线路机端口。`, impact: '已经开始的行不受影响。', confirm: '取消这些行', danger: true })
          .then((yes) => { if (yes) { const out = ACTIONS.cancelUnstarted(st, b); toast({ message: `已取消 ${out.length} 行` }); } });
      } else if (a === 'clear') app.setParams({ status: null, q: null });
      else if (a === 'row-menu') { e.stopPropagation(); rowMenu(b_, b.rows.find((r) => r.name === b_.dataset.rowid)); }
    }));
    async function liveBatchAct(a, btn) {
      const b = batchOf();
      if (!b?.live) return;
      if (guard(a === 'ob-cancel' ? 'onboarding.batch.cancelUnstarted' : 'onboarding.batch.new')) return;
      if (a === 'ob-start') { confirmStart({ ob, batch: b, onStarted: () => ob.refresh() }); return; }
      if (a === 'ob-cancel') {
        const left = b.rows.filter((r) => !['active', 'cancelled'].includes(rowWord(b, r).state)).length;
        const yes = await confirmAction({ action: 'onboarding.batch.cancelUnstarted', title: `取消批次 ${b.title}`, sub: b.id,
          body: `还没上线的 ${left} 行全部停止（正在执行的步骤先跑完）；已完成且可以补偿的步骤逆序撤销（线路机上本批加的入口按 tag 撤下）。已上线的行不受影响。`,
          impact: '不可撤销：要继续上架这些机器，需要重新建批次。', confirm: '取消批次', danger: true, typeToConfirm: b.id });
        if (!yes) return;
      }
      btn.disabled = true; btn.setAttribute('aria-busy', 'true');
      try {
        if (a === 'ob-resume') { const r = await ob.resume(b.id); toast({ message: r?.running ? '批次正在执行中' : '已继续执行' }); }
        else { const r = await ob.cancel(b.id); toast({ message: r?.status === 'cancelling' ? `正在取消 ${b.title}：进行中的步骤跑完后逆序补偿` : `已取消 ${b.title}` }); }
      } catch (e) { if (!e.auth) toast({ message: `操作失败：${e.message}`, tone: 'bad' }); }
      finally { btn.disabled = false; btn.removeAttribute('aria-busy'); ob.refresh(); }
    }
    // KPI 点击 = 行筛选（再点一次取消）
    offs.push(on(el, 'click', '[data-kpi] .kpi-hit', (e, h) => { const v = h.closest('[data-kpi]').dataset.kpi; app.setParams({ status: P.status === v ? null : v }); slot('matrix').scrollIntoView({ block: 'start', behavior: matchMedia('(prefers-reduced-motion: reduce)').matches ? 'auto' : 'smooth' }); }));
    // 说明行里的按钮
    offs.push(on(el, 'click', '[data-ract]', (e, b_) => {
      e.stopPropagation();
      const b = batchOf();
      const r = b?.rows.find((x) => x.name === b_.dataset.rowid);
      if (!r) return;
      if (!['ticket'].includes(b_.dataset.ract) && guard('onboarding.row')) return;
      let password;
      if (b_.dataset.ract === 'secret' || b_.dataset.ract === 'continue') {
        const input = slot('matrix').querySelector(`[data-secret-for="${CSS.escape(r.name)}"]`);
        if (b_.dataset.ract === 'secret' && !input?.value) { input?.setAttribute('aria-invalid', 'true'); input?.focus(); return; }
        password = input?.value || undefined;
        if (input) input.value = '';
      }
      act(b_.dataset.ract, r, null, { anchor: b_, password });
    }));
    offs.push(on(el, 'keydown', '[data-secret-for]', (e, input) => {
      input.removeAttribute('aria-invalid');
      if (e.key === 'Enter') { e.preventDefault(); input.closest('.ob-note').querySelector('[data-ract="secret"], [data-ract="continue"]')?.click(); }
    }));
    // 矩阵：点格子 → 那一步；点行其它地方 → 当前步
    if (!jobsOnly) offs.push(on(slot('matrix'), 'click', 'tr[data-row]', (e, tr) => {
      if (e.target.closest('[data-act], [data-ract], input, a')) return;
      const cell = e.target.closest('[data-cell]');
      const [rid, code] = (cell?.dataset.cell ?? `${tr.dataset.row}|`).split('|');
      const r = batchOf()?.rows.find((x) => x.name === rid);
      if (r) openRow(r.name, code || currentCell(r).step);
    }));
    if (!jobsOnly) offs.push(on(slot('matrix'), 'keydown', 'tr[data-row]', (e, tr) => {
      if ((e.key === 'Enter' || e.key === ' ') && e.target === tr) { e.preventDefault(); const r = batchOf()?.rows.find((x) => x.name === tr.dataset.row); if (r) openRow(r.name, currentCell(r).step); }
    }));
    // 批次列表
    offs.push(on(slot('list'), 'click', 'tr[data-row]', (e, tr) => {
      const [kind, id] = tr.dataset.row.split(':');
      if (kind === 'task') openTask(id);
      else { app.setParams({ batch: id === (liveOb ? listIds()[0] : st.batches[0]?.id) ? null : id, status: null, q: null, row: null, step: null }); el.querySelector('.page-head')?.scrollIntoView({ block: 'start', behavior: 'auto' }); window.scrollTo({ top: 0 }); }
    }));
    offs.push(on(slot('list'), 'keydown', 'tr[data-row]', (e, tr) => { if (e.key === 'Enter' && e.target === tr) { e.preventDefault(); tr.click(); } }));

    function newBatch() {
      if (liveOb) {
        if (guard('onboarding.batch.new')) return;
        openLiveBatch({ app, ob, onCreated: (id) => { app.setParams({ batch: id, status: null, q: null }); ob.refresh(); } });
        return;
      }
      openNewBatch({ app, st, onCreated: (b) => app.setParams({ batch: b.id === st.batches[0]?.id ? null : b.id, status: null, q: null }) });
    }

    // 模型事件：'tick' 每秒（只改计时），'change' 结构变化（重画受影响的区块）
    const offModel = st.on((kind, d) => {
      if (!el.isConnected) return;
      if (kind === 'change') { renderChange(); refreshStepDrawer(); }
      else if (kind === 'tick') renderTick();
      else if (kind === 'canary' && d.batch === batchOf()) toast({ message: `金丝雀 ${d.row.name} 已通过，放开其余 ${d.batch.rows.length - 1} 行` });
    });
    offs.push(offModel);
    const timer = jobsOnly ? null : setInterval(() => tick(st), 1000);
    // 实时：接入任务列表或任务详情（步骤）更新 → 重画列表
    if (jobsOnly || liveOb) offs.push(app.store.on('change', (d) => { if (d?.fleet?.onboarding && el.isConnected) renderChange(); }));

    // 快捷键
    const unbind = app.keys.bindAll([
      { combo: 'n', run: () => newBatch(), desc: '新建批次', group: '上架流水线', when: () => !stepDrawerOpen() },
      { combo: 'j', run: () => moveRow(1), desc: '矩阵下一行', group: '上架流水线', when: () => !stepDrawerOpen() },
      { combo: 'k', run: () => moveRow(-1), desc: '矩阵上一行', group: '上架流水线', when: () => !stepDrawerOpen() },
      { combo: 'o', run: () => { const tr = document.activeElement?.closest?.('tr[data-row]'); if (tr && (slot('matrix') ?? slot('list')).contains(tr)) tr.click(); }, desc: '打开焦点所在行的步骤详情', group: '上架流水线', when: () => !stepDrawerOpen() },
      { combo: 'c', run: () => app.setParams({ cols: P.cols === 'steps' ? null : 'steps' }), desc: '切换 阶段 / 步骤 列', group: '上架流水线', when: () => !stepDrawerOpen() },
      { combo: '[', run: () => dctx.onNav(-1), desc: '抽屉里上一步', group: '抽屉', when: () => stepDrawerOpen(), hidden: true },
      { combo: ']', run: () => dctx.onNav(1), desc: '抽屉里下一步', group: '抽屉', when: () => stepDrawerOpen(), hidden: true },
    ]);
    function moveRow(d) {
      const trs = [...(slot('matrix') ?? slot('list')).querySelectorAll('tr[data-row]')];
      if (!trs.length) return;
      const i = trs.indexOf(document.activeElement?.closest?.('tr[data-row]'));
      const next = trs[i < 0 ? (d > 0 ? 0 : trs.length - 1) : Math.max(0, Math.min(trs.length - 1, i + d))];
      next.focus();
      next.scrollIntoView({ block: 'nearest' });
    }

    return {
      update(params) {
        const prev = P;
        P = readP(params);
        const b = batchOf();
        if (jobsOnly) { /* 只有任务列表：只同步抽屉与当前行 */ }
        else if (prev.batch !== P.batch) { closingByRoute = true; if (!P.row && !P.task) closeStepDrawer(); closingByRoute = false; renderAll(); if (liveOb && !batchOf()) ob.refresh({ list: false }); }
        else if (!b) { /* 实时：批次还没读到 */ }
        else if (prev.cols !== P.cols || prev.status !== P.status || prev.q !== P.q) {
          const segS = slot('matrix').querySelector('[data-seg="ob-status"]');
          if (segS) setSegmented(segS, P.status);
          const segC = slot('matrix').querySelector('[data-seg="ob-cols"]');
          if (segC) setSegmented(segC, P.cols);
          setSearch(slot('matrix'), 'q', P.q);
          renderTable();
          const k = slot('kpis');
          for (const key of ['done', 'running', 'attention']) k.querySelector(`[data-kpi="${key}"] .kpi-hit`)?.setAttribute('aria-pressed', String(P.status === key));
        }
        syncDrawer();
        markCurrent();
        void b;
      },
      unmount() {
        ob?.dispose();
        clearInterval(timer);
        ro.disconnect();
        offs.forEach((f) => f());
        unbind();
        closingByRoute = true; closeStepDrawer(); closingByRoute = false;
      },
    };
  },
};
