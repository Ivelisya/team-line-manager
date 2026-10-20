// onboarding-drawer.js — 步骤抽屉（SPEC §6.6）：与节点抽屉同一个外壳（右侧 760、灰底白卡、顶部页签、底部固定按钮）。
//   新流水线的格子：头 `团队1-42 · 重装验收` + 状态 pill；副行 `P6 · 耗时 20 分钟 · 预计 5–10 分钟 · IP · 线路机`；
//   页签 日志 │ 时间线 │ 输入输出；底部 从这一步重试（主）、跳过（只对可跳过的步骤）等。
//   旧流程任务（history）：同一外壳，页签 步骤 │ 日志 │ 输入输出。
// 状态写在 URL（row + step + stab，或 task），浏览器后退关闭抽屉（§5.5）。

import { fmt } from '../../shared/index.js';
import {
  esc, on, openDrawer, tabs as tabsHtml, tonePill, button, iconButton, kv, note, empty,
  logView, followLog, logLines, timeline, secretInput, searchBox, onSearch, copyText, neutralChip, hydrate,
} from './components.js';
import { ROW, BATCH_WORD, SKIPPABLE, clean, cellDuration, cellText, rowWord, logFor, legacyLog, ioFor, estWord } from './onboarding-model.js';

const STEP_TABS = [{ value: 'log', label: '日志' }, { value: 'timeline', label: '时间线' }, { value: 'io', label: '输入输出' }];
const TASK_TABS = [{ value: 'timeline', label: '步骤' }, { value: 'log', label: '日志' }, { value: 'io', label: '输入输出' }];

/** 步骤状态 pill：进行中 / 等待闸门呼吸点，其余实心点。 */
export function stepPill(status) {
  const s = fmt.step(status);
  const tone = { ok: 'ok', warn: 'warn', bad: 'bad', info: 'info' }[s.tone] ?? 'neutral';
  return tonePill(s.word, tone, { dot: status === 'running' || status === 'waiting_gate' ? 'pulse' : status === 'queued' || status === 'skipped' ? 'ring' : 'solid' });
}

let cur = null; // { api, kind, key, ... }

export const stepDrawerOpen = () => !!cur?.api?.el?.isConnected;
export function closeStepDrawer() { if (cur?.api) { const a = cur.api; cur = null; a.close(); } }

/**
 * openStepDrawer(ctx, { batch, row, step, tab }) — 已打开时原地切换（不重建外壳）。
 * ctx: { st, onNav(dir), onClose(), onTab(tab), onStep(code), act(name, row, step, payload) }
 */
export function openStepDrawer(ctx, { batch, row, step, tab = 'log' }) {
  const key = `step|${batch.id}|${row.name}|${step}`;
  if (cur && cur.api.el.isConnected && cur.kind === 'step') {
    const changed = cur.key !== key;
    Object.assign(cur, { key, batch, row, step, ctx });
    cur.api.setHead(stepHead(ctx.st, batch, row, step));
    cur.api.setFoot(stepFoot(batch, row, step));
    if (changed || cur.api.tab !== tab) { cur.query = ''; cur.api.setTab(tab, { silent: true }); }
    return cur.api;
  }
  closeStepDrawer();
  const state = { kind: 'step', key, batch, row, step, ctx, query: '', follow: true, logCount: 0 };
  cur = state;
  state.api = openDrawer({
    label: `步骤详情 ${row.name}`,
    ...stepHead(ctx.st, batch, row, step),
    tabsHtml: tabsHtml({ name: 'step-drawer', items: STEP_TABS, value: tab, label: '步骤详情页签' }),
    tab,
    navLabels: ['上一步（[）', '下一步（]）'],
    onNav: (dir) => ctx.onNav(dir),
    foot: stepFoot(batch, row, step),
    render: (t, body) => renderStep(state, t, body),
    onTab: (t) => ctx.onTab(t),
    onClose: () => { if (cur === state) cur = null; state.offs?.forEach((f) => f()); ctx.onClose(); },
  });
  wireFoot(state);
  return state.api;
}

function stepHead(st, batch, row, code) {
  const c = row.cellMap.get(code);
  const def = batch.byCode.get(code);
  const now = st.now();
  const dur = cellDuration(c, now);
  const meta = [
    code,
    c.status === 'queued' ? '还没开始' : c.status === 'skipped' ? '已跳过' : `耗时 ${fmt.duration(dur ?? 0, { units: 2 }).text}`,
    `预计 ${def.estWord}`,
    row.ip,
    row.relay ? `${row.relay}${row.relay_port ? `:${row.relay_port}` : ''}` : null,
  ].filter(Boolean).join(' · ');
  return { title: `${row.name} · ${def.name}`, pill: stepPill(c.status), meta: `<span data-live="dmeta">${esc(meta)}</span>` };
}

function stepFoot(batch, row, code) {
  const c = row.cellMap.get(code);
  const b = [];
  if (row.cancelled) return '';
  if (batch.live) return liveFoot(row, code, c);
  const abandon = button({ label: '放弃此行', variant: 'danger-quiet', cls: 'ob-foot-start', attrs: { 'data-dact': 'abandon' } });
  if (c.status === 'failed') {
    b.push(abandon);
    if (SKIPPABLE.has(code)) b.push(button({ label: '跳过', icon: 'skip-forward', variant: 'quiet', attrs: { 'data-dact': 'skip' } }));
    if (code === 'P6') b.push(button({ label: '再等 10 分钟', icon: 'timer', attrs: { 'data-dact': 'wait' } }), button({ label: '打开面板 VNC', icon: 'external-link', attrs: { 'data-dact': 'vnc' } }));
    b.push(button({ label: '从这一步重试', icon: 'rotate-ccw', variant: 'primary', attrs: { 'data-dact': 'retry' } }));
  } else if (c.status === 'blocked') {
    b.push(abandon, button({ label: '改 IP', icon: 'pencil', attrs: { 'data-dact': 'ip' } }), button({ label: '重新盘点', icon: 'scan-search', variant: 'primary', attrs: { 'data-dact': 'retry' } }));
  } else if (c.status === 'waiting_input') {
    b.push(`<span class="row">${secretInput({ name: 'drawer-secret', width: 240, label: `${row.name} 的 root 密码`, attrs: { 'data-dsecret': row.name } })}</span>`, button({ label: '提交', variant: 'primary', attrs: { 'data-dact': 'secret' } }));
  } else if (c.status === 'running' && SKIPPABLE.has(code)) {
    b.push(button({ label: '跳过这一步', icon: 'skip-forward', variant: 'quiet', attrs: { 'data-dact': 'skip' } }));
  }
  return b.join('');
}

/** 实时批次（新后台）：只有后台真有的动作——重试（风险停止要确认已处置）、放弃此行（逆序补偿）、补密码、DD 闸门继续。 */
function liveFoot(row, code, c) {
  const abandon = button({ label: '放弃此行', variant: 'danger-quiet', cls: 'ob-foot-start', attrs: { 'data-dact': 'abandon' } });
  if (c.status === 'failed') return abandon + button({ label: '从这一步重试', icon: 'rotate-ccw', variant: 'primary', attrs: { 'data-dact': 'retry' } });
  if (c.status === 'blocked') return abandon + button({ label: '已处置，重试', icon: 'rotate-ccw', variant: 'primary', attrs: { 'data-dact': 'retry' } });
  if (c.status === 'waiting_input') {
    const gate = ['P4', 'P5', 'P6'].includes(code);
    return `<span class="row">${secretInput({ name: 'drawer-secret', width: 240, label: gate ? `${row.name} 重装后的 root 密码（选填）` : `${row.name} 的 root 密码`, attrs: { 'data-dsecret': row.name } })}</span>`
      + button({ label: gate ? '重装完成，继续' : '提交', variant: 'primary', icon: gate ? 'play' : undefined, attrs: { 'data-dact': gate ? 'continue' : 'secret' } });
  }
  return '';
}

function wireFoot(state) {
  const foot = state.api.el.querySelector('.drawer-foot');
  state.offs = [
    on(foot, 'click', '[data-dact]', (e, btn) => {
      const a = btn.dataset.dact;
      let password;
      if (a === 'secret' || a === 'continue') {
        const input = foot.querySelector('[data-dsecret]');
        if (a === 'secret' && !input?.value) { input?.setAttribute('aria-invalid', 'true'); input?.focus(); return; }
        password = input?.value || undefined;
        if (input) input.value = '';
      }
      state.ctx.act(a, state.row, state.step, { batch: state.batch, anchor: btn, password });
    }),
    on(foot, 'keydown', '[data-dsecret]', (e, input) => { if (e.key === 'Enter') foot.querySelector('[data-dact="secret"], [data-dact="continue"]')?.click(); else input.removeAttribute('aria-invalid'); }),
  ];
}

function renderStep(state, tab, body) {
  const { batch, row, step, ctx } = state;
  const now = ctx.st.now();
  const c = row.cellMap.get(step);
  const def = batch.byCode.get(step);
  state.logFollow?.dispose?.();
  state.logFollow = null;
  state.offBody?.();
  state.offBody = null;
  if (tab === 'log') {
    const lines = logFor(batch, row, step, now);
    state.logCount = lines.length;
    const detail = clean(c.detail);
    const tone = c.status === 'failed' || c.status === 'blocked' ? 'bad' : c.status === 'waiting_input' ? 'warn' : null;
    const riskLine = c.status === 'blocked' && row.risk?.detail ? `<div class="mono">${esc(clean(row.risk.detail))}${batch.live ? '' : '（演示值）'}</div>` : '';
    body.innerHTML = `${tone && detail ? note({ tone, body: `${esc(detail)}${riskLine}` }) : ''}
      <section class="dz-card">
        <header class="dz-head ob-loghead"><h3 class="dz-title">日志</h3><span class="muted nowrap">${esc(`${step} ${def.name}`)}</span>
          <div class="dz-tools">${searchBox({ name: 'logq', value: state.query, placeholder: '搜索日志', pageSearch: false, width: 200 })}
            <label class="check" data-tip="有新日志时自动滚到末尾">${'<input type="checkbox" data-follow' + (state.follow ? ' checked' : '') + '>'}<span>跟随末尾</span></label>
            ${iconButton({ icon: 'copy', label: '复制日志', size: 'sm', attrs: { 'data-copylog': '' } })}</div></header>
        ${logView({ lines, query: state.query, empty: c.status === 'queued' ? '这一步还没开始' : '这一步没有日志', label: `${row.name} ${def.name} 日志`, height: 380 })}
        <div class="log-foot muted" data-live="loghits">${esc(hitsText(lines, state.query))}</div>
      </section>`;
    const logEl = body.querySelector('.log');
    state.logFollow = followLog(logEl, { follow: state.follow, onFollowChange: (f) => { state.follow = f; const sw = body.querySelector('[data-follow]'); if (sw) sw.checked = f; } });
    if (state.query) body.querySelector('.log-hit')?.scrollIntoView({ block: 'center' });
    const offs = [
      onSearch(body, 'logq', (v) => {
        state.query = v;
        const all = logFor(batch, row, step, ctx.st.now());
        logEl.innerHTML = all.length ? logLines(all, { query: v }) : logEl.innerHTML;
        body.querySelector('[data-live="loghits"]').textContent = hitsText(all, v);
        const hit = logEl.querySelector('.log-hit');
        if (hit) { state.follow = false; body.querySelector('[data-follow]').checked = false; hit.scrollIntoView({ block: 'center' }); }
      }, { delay: 200 }),
      on(body, 'change', '[data-follow]', (e, sw) => { state.follow = sw.checked; state.logFollow?.setFollow(sw.checked); }),
      on(body, 'click', '[data-copylog]', (e, b) => copyText(logFor(batch, row, step, ctx.st.now()).map((l) => `${l.t}  ${l.text}`).join('\n'), b)),
    ];
    state.offBody = () => offs.forEach((f) => f());
  } else if (tab === 'timeline') {
    const items = row.cells.map((x) => {
      const d = batch.byCode.get(x.step);
      const dur = cellDuration(x, now);
      const meta = x.status === 'queued' ? `预计 ${esc(d.estWord)}` : x.status === 'skipped' ? '跳过' : x.status === 'running' ? `<span class="num" data-live="tl-${esc(x.step)}">${esc(fmt.duration(dur ?? 0, { units: 2 }).text)}</span>` : x.status === 'succeeded' ? `<span class="num">${esc(fmt.duration(dur ?? 0, { units: 2 }).text)}</span>` : esc(cellText(x.status, dur, x.detail) || fmt.step(x.status).word);
      return { status: x.status, title: `<span class="num muted">${esc(x.step)}</span> ${esc(d.name)}`, meta, desc: x.detail ? esc(clean(x.detail)) : '', current: x.step === step, attrs: { 'data-tl': x.step, 'aria-label': `${x.step} ${d.name} · ${fmt.step(x.status).word}` } };
    });
    const done = row.cells.filter((x) => x.status === 'succeeded').length;
    const appl = row.cells.filter((x) => x.status !== 'skipped').length;
    const w = rowWord(batch, row);
    body.innerHTML = `<section class="dz-card">
        <header class="dz-head"><h3 class="dz-title">时间线</h3>${tonePill(w.word, w.tone, { dot: ['running', 'in_relay_batch', 'waiting_gate'].includes(w.state) ? 'pulse' : 'solid' })}<div class="dz-tools"><span class="num">${done}/${appl}</span> 步完成</div></header>
        ${timeline(items, { label: `${row.name} 的 16 步` })}
      </section>`;
    state.offBody = on(body, 'click', '[data-tl]', (e, b) => ctx.onStep(b.dataset.tl));
  } else {
    const io = ioFor(batch, row, step);
    const kvi = (list) => kv(list.map(([label, v]) => ({ label, value: esc(v ?? '—') })), { cols: 2 });
    const recipe = io.input.find(([k]) => k === '配方');
    body.innerHTML = `<section class="dz-card"><header class="dz-head"><h3 class="dz-title">输入</h3><span class="muted">这一行的上架参数</span></header><div class="stack-sm">${kvi(io.input.filter(([k]) => k !== '配方'))}${recipe ? kv([{ label: '配方', value: esc(recipe[1] ?? '—') }], { cols: 1 }) : ''}</div></section>
      <section class="dz-card"><header class="dz-head"><h3 class="dz-title">输出</h3><span class="muted">${esc(`${step} ${def.name}`)}</span></header>${io.output.length ? kvi(io.output) : empty({ icon: 'inbox', text: c.status === 'succeeded' ? '这一步没有需要记录的输出' : '这一步还没有输出' })}</section>
      ${note({ tone: 'info', body: '密码、连接凭据和连接链接只写不读：这里只显示状态，从不显示值。' })}`;
  }
  hydrate(body);
}
const hitsText = (lines, q) => {
  if (!q) return `${lines.length} 行`;
  const low = q.toLowerCase();
  const n = lines.reduce((a, l) => a + (l.text.toLowerCase().split(low).length - 1), 0);
  return n ? `${lines.length} 行 · 命中 ${n} 处` : `${lines.length} 行 · 没有找到“${q}”`;
};

/** 每秒调用：更新副行耗时、时间线计时、追加新日志行（不重画，不打断滚动与搜索）。 */
export function tickStepDrawer() {
  if (!cur || !cur.api.el.isConnected || cur.kind !== 'step') return;
  const { api, batch, row, step, ctx } = cur;
  const now = ctx.st.now();
  const head = stepHead(ctx.st, batch, row, step);
  const m = api.el.querySelector('[data-live="dmeta"]');
  const txt = head.meta.replace(/<[^>]+>/g, '').replace(/&amp;/g, '&');
  if (m && m.textContent !== txt) m.textContent = txt;
  if (api.tab === 'log' && cur.logFollow) {
    const lines = logFor(batch, row, step, now);
    if (lines.length > cur.logCount) {
      cur.logFollow.append(lines.slice(cur.logCount).map((l) => ({ ...l })), cur.query);
      cur.logCount = lines.length;
      const h = api.body.querySelector('[data-live="loghits"]');
      if (h) h.textContent = hitsText(lines, cur.query);
    } else if (lines.length && row.cellMap.get(step).status === 'running') {
      // 最后一行是实时计数（例如合批窗口剩余秒数）：原地改写
      const last = api.body.querySelector('.log-line:last-child .log-text');
      const want = lines[lines.length - 1].text;
      if (last && last.textContent !== want && /窗口剩|正在合并/.test(want)) last.textContent = want;
    }
  } else if (api.tab === 'timeline') {
    for (const x of row.cells) if (x.status === 'running') { const e = api.body.querySelector(`[data-live="tl-${CSS.escape(String(x.step))}"]`); const t = fmt.duration(cellDuration(x, now) ?? 0, { units: 2 }).text; if (e && e.textContent !== t) e.textContent = t; }
  }
}
/** 结构变化（状态、说明）后：刷新头、底栏与当前页签。 */
export function refreshStepDrawer() {
  if (!cur || !cur.api.el.isConnected) return;
  if (cur.kind === 'step') {
    const { api, batch, row, step, ctx } = cur;
    api.setHead(stepHead(ctx.st, batch, row, step));
    const f = stepFoot(batch, row, step);
    const foot = api.el.querySelector('.drawer-foot');
    const typing = foot.contains(document.activeElement) && document.activeElement.matches('input');
    if (!typing) api.setFoot(f);
    const scroller = api.body;
    const top = scroller.scrollTop;
    const logEl = api.body.querySelector('.log');
    const logTop = logEl?.scrollTop;
    api.refresh();
    scroller.scrollTop = top;
    const nl = api.body.querySelector('.log');
    if (nl && logTop != null && !cur.follow) nl.scrollTop = logTop;
  }
}
export const drawerKey = () => cur?.key ?? null;

// ─── 旧流程任务 ─────────────────────────────────────────────────────────────

export function openTaskDrawer(ctx, task, tab = 'timeline') {
  const key = `task|${task.id}`;
  if (cur && cur.api.el.isConnected && cur.kind === 'task') {
    const changed = cur.key !== key;
    Object.assign(cur, { key, task });
    cur.api.setHead(taskHead(task));
    if (changed || cur.api.tab !== tab) cur.api.setTab(tab, { silent: true });
    return cur.api;
  }
  closeStepDrawer();
  const state = { kind: 'task', key, task, ctx };
  cur = state;
  state.api = openDrawer({
    label: `上架任务 ${task.title}`,
    ...taskHead(task),
    tabsHtml: tabsHtml({ name: 'task-drawer', items: TASK_TABS, value: tab, label: '上架任务页签' }),
    tab,
    navLabels: ['上一条（[）', '下一条（]）'],
    onNav: (dir) => ctx.onNav(dir),
    render: (t, body) => renderTask(state, t, body),
    onTab: (t) => ctx.onTab(t),
    onClose: () => { if (cur === state) cur = null; ctx.onClose(); },
  });
  return state.api;
}
function taskHead(task) {
  const [word, tone] = { succeeded: ['成功', 'ok'], rolled_back: ['已回滚', 'warn'], failed: ['失败', 'bad'] }[task.status] ?? [task.status_word, 'neutral'];
  const meta = [`旧流程 ${task.total_steps} 步`, task.type_word, `开始 ${fmt.stamp(task.started_at).text}`, `耗时 ${fmt.duration(Math.round(task.duration_s)).text}`, task.relay, `任务 ${task.id}`].filter(Boolean).join(' · ');
  return { title: task.title, pill: tonePill(word, tone), meta: esc(meta) };
}
function renderTask(state, tab, body) {
  const t = state.task;
  if (tab === 'timeline') {
    const items = t.steps.map((s, i) => ({
      status: s.status,
      title: `<span class="num muted">${i + 1}</span> ${esc(s.name)}`,
      meta: s.status === 'skipped' ? '未执行' : `<span class="num">${esc(fmt.duration(Math.max(1, Math.round(s.duration_s ?? 0))).text)}</span>`,
      desc: s.error ? `<span class="t-bad">${esc(s.error)}</span>` : '',
    }));
    body.innerHTML = `${t.error ? note({ tone: t.status === 'rolled_back' ? 'warn' : 'bad', title: t.status === 'rolled_back' ? '已回滚' : '失败', body: esc(t.error) }) : ''}
      <section class="dz-card"><header class="dz-head"><h3 class="dz-title">步骤</h3><div class="dz-tools"><span class="num">${esc(t.ok_steps)}/${esc(t.total_steps)}</span> 步成功</div></header>${timeline(items, { label: '旧流程步骤' })}</section>`;
  } else if (tab === 'log') {
    const lines = legacyLog(t);
    body.innerHTML = `<section class="dz-card"><header class="dz-head"><h3 class="dz-title">日志</h3><span class="muted">由各步耗时与错误整理</span></header>${logView({ lines, label: `${t.title} 日志`, height: 420 })}</section>`;
  } else {
    body.innerHTML = `<section class="dz-card"><header class="dz-head"><h3 class="dz-title">输入</h3></header>${kv([
      { label: '类型', value: esc(t.type_word) }, { label: '供应商', value: esc(t.provider ?? '—') },
      { label: '服务号', value: esc(t.service_id ?? '—') }, { label: '线路机', value: esc(t.relay ?? '—') },
      { label: '团队', value: esc(t.team ?? '—') }, { label: '操作者', value: t.operator ? esc(t.operator) : '<span class="t-muted">—</span>' },
      { label: '任务', value: `<span class="mono">${esc(t.id)}</span>` }, { label: '台数', value: `<span class="num">${esc(t.count)}</span>` },
    ], { cols: 2 })}</section>${note({ tone: 'info', body: '旧流程只记录了每步耗时与错误，没有逐行日志；新流水线的任务会保留完整日志。' })}`;
  }
  hydrate(body);
}
