// writes-smoke.mjs — the wired class (a) write actions end to end, on the a395 staging DB copy.
//
//   WRITES=1 deploy/v2/build.sh && STAGE_WRITES=1 deploy/v2/staging/run.sh      (staging only; never production)
//   ~/.local/bin/node deploy/v2/tools/writes-smoke.mjs [--base http://127.0.0.1:18395]
//
// Drives the real UI: 登记续费 · 编辑资产 · 监控与维护 (drawer), a stale-revision edit (server error path), a client-side
// validation error, a class (b) action (fleetd has not opened it: “新后台尚未开放” notice, nothing sent), behavior / incident ack when
// the lists have rows.  Checks the POSTs (path, headers, body), the toasts, and that the store reflects the change.
import { launch } from '../../../prototype/original-refined/tools/shoot.mjs';
import path from 'node:path';

const arg = (k, d) => { const i = process.argv.indexOf(k); return i > 0 ? process.argv[i + 1] : d; };
const BASE = arg('--base', 'http://127.0.0.1:18395');
if (!/^http:\/\/127\.0\.0\.1/.test(BASE)) { console.error('staging tunnel only'); process.exit(2); }
const OUT = path.join(path.dirname(new URL(import.meta.url).pathname), '..', 'shots', 'phase3');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const b = await launch({ port: 9700 + Math.floor(Math.random() * 200) });
// capture POSTs through the page (fetch wrapper installed before the app runs)
await b.send('Page.addScriptToEvaluateOnNewDocument', { source: `window.__posts = []; const F = window.fetch; window.fetch = async (u, i = {}) => { const r = await F(u, i); if ((i.method || 'GET') !== 'GET') { const c = r.clone(); let body = null; try { body = await c.json(); } catch {} window.__posts.push({ url: String(u), method: i.method, headers: i.headers, req: i.body ? JSON.parse(i.body) : null, status: r.status, body }); } return r; };` });
await b.viewport(1440, 900);
await b.go(`${BASE}/`);
await b.until('document.documentElement.dataset.ready === "1"', 30000);
const results = [];
const check = (name, ok, detail) => { results.push({ name, ok: !!ok, detail }); console.log(`${ok ? '✓' : '✗'} ${name}${detail ? ' — ' + (typeof detail === 'string' ? detail : JSON.stringify(detail)) : ''}`); };
const lastToast = () => b.eval(`[...document.querySelectorAll('.toasts .toast-msg')].map((t) => t.textContent).pop() ?? null`);
const clearToasts = () => b.eval(`document.querySelectorAll('.toasts .toast').forEach((t) => t.remove()); 0`);
const lastPost = () => b.eval('window.__posts[window.__posts.length - 1] ?? null');
const nPosts = () => b.eval('window.__posts.length');

check('badge says writes are on', (await b.eval(`document.querySelector('.live-badge')?.innerText.trim()`)) === '实时数据');
const FLEET = await b.eval('!!globalThis.TEAMOPS?.fleet?.available');
// On a395 fleetd reads its own legacy copy (fleet/staging, GET only) while /api/team-ops/* POSTs land on the v2 staging copy:
// the team-ops DB writes are checked on the legacy origin (:18395), fleetd's own writes on the fleetd origin (:18398).
const id = await b.eval(`__app.store.nodes().find((n) => n.hasHost && !n.isRelay && n.billing?.due_date).id`);
let p = null;
// old system decommissioned (fleet/API.md §12.2): the same class (a) writes go to fleetd's /v2/api/v1/legacy-write/<old path>
const LW = await b.eval("!!globalThis.TEAMOPS?.fleet?.has?.('legacy.write')");
const W = LW ? '/v2/api/v1/legacy-write' : '/api/team-ops';
const HDR = LW ? 'X-Fleet-Request' : 'X-Ops-Request';
if (!FLEET || LW) {
const before = await b.eval(`({ ...TEAMOPS.asset(${JSON.stringify(id)}) })`);

// 1. 登记续费
await b.eval(`__app.openNode(${JSON.stringify(id)}, 'billing'); 0`);
await b.until(`!!document.querySelector('.drawer [data-dz="renew"]')`, 8000);
await b.eval(`document.querySelector('.drawer [data-dz="renew"]').click(); 0`);
await b.until(`!!document.querySelector('.dialog [data-live-submit]')`, 5000);
await b.shot(path.join(OUT, 'renew-dialog-light-1440.png'));
const newDue = await b.eval(`document.querySelector('.dialog [name="due_date"]').value`);
await clearToasts();
await b.eval(`document.querySelector('.dialog [data-live-submit]').click(); 0`);
await sleep(1500);
p = await lastPost();
check(`renew → POST ${W}/assets/edit`, p?.url === `${W}/assets/edit` && p.status === 200 && p.headers[HDR] === '1' && p.req.renewal === true && p.req.expected_revision === before.revision && p.req.due_date === newDue, { status: p?.status, req: p?.req, err: p?.body?.error });
check('renew toast', /^已登记续费 · /.test(await lastToast() ?? ''), await lastToast());
check('renew dialog closed', !(await b.eval(`!!document.querySelector('.dialog')`)));
await sleep(2500);
const after = await b.eval(`({ ...TEAMOPS.asset(${JSON.stringify(id)}) })`);
check('store shows new due date + revision', after.due_date === newDue && after.revision === before.revision + 1, { before: [before.due_date, before.revision], after: [after.due_date, after.revision] });

// 2. 编辑资产（改备注）
await b.eval(`document.querySelector('.drawer [data-dz="edit"]').click(); 0`);
await b.until(`!!document.querySelector('.dialog [name="note"]')`, 5000);
await b.eval(`const t = document.querySelector('.dialog [name="note"]'); t.value = 'v2 写入验收 ' + Date.now(); t.dispatchEvent(new Event('input', { bubbles: true })); 0`);
await clearToasts();
await b.eval(`document.querySelector('.dialog [data-live-submit]').click(); 0`);
await sleep(1500);
p = await lastPost();
check('edit → POST /assets/edit renewal:false', p?.status === 200 && p.req.renewal === false && /^v2 写入验收/.test(p.req.note), { status: p?.status, err: p?.body?.error });
check('edit toast', /^已保存 · /.test(await lastToast() ?? ''), await lastToast());

// 3. stale revision → server error toast, dialog stays open
await sleep(2500);
await b.eval(`TEAMOPS.asset(${JSON.stringify(id)}).revision = 1; 0`);
await b.eval(`document.querySelector('.drawer [data-dz="edit"]').click(); 0`);
await b.until(`!!document.querySelector('.dialog [data-live-submit]')`, 5000);
await clearToasts();
await b.eval(`document.querySelector('.dialog [data-live-submit]').click(); 0`);
await sleep(1500);
p = await lastPost();
check('stale revision → 400 with the server message', p?.status === 400 && /已变化/.test(p.body?.error ?? ''), p?.body);
check('error toast shows it, dialog stays open', /^操作失败：资产信息已变化/.test(await lastToast() ?? '') && (await b.eval(`!!document.querySelector('.dialog')`)), await lastToast());
await b.shot(path.join(OUT, 'edit-error-light-1440.png'));
await b.eval(`__app.live.refresh(); 0`); await sleep(1500);
await b.eval(`document.querySelector('.dialog [data-dlg="cancel"]')?.click(); 0`); await sleep(300);
await b.eval(`document.querySelector('.dialog [data-dlg]')?.click(); 0`); await sleep(300); // “放弃已填写的内容？” if dirty

// 4. client-side validation (renew date not later)
const n0 = await nPosts();
await b.eval(`document.querySelector('.drawer [data-dz="renew"]').click(); 0`);
await b.until(`!!document.querySelector('.dialog [name="due_date"]')`, 5000);
await b.eval(`const d = document.querySelector('.dialog [name="due_date"]'); d.value = '2020-01-01'; 0`);
await b.eval(`document.querySelector('.dialog [data-live-submit]').click(); 0`);
await sleep(400);
check('validation error in the dialog, no request', (await nPosts()) === n0 && /必须晚于/.test(await b.eval(`document.querySelector('.dialog [data-err]')?.textContent ?? ''`)));
await b.eval(`document.querySelector('.dialog [data-dlg="cancel"]')?.click(); 0`); await sleep(300);
await b.eval(`document.querySelector('.dialog .btn-primary, .dialog [data-dlg]')?.click(); 0`); await sleep(300);

// 5. 监控与维护
await b.eval(`__app.setDrawerParams(${JSON.stringify(id)}, 'manage', { replace: true }); 0`);
await b.until(`!!document.querySelector('.drawer [data-dz="monitor"]')`, 5000);
await b.eval(`document.querySelector('.drawer [data-dz="monitor"]').click(); 0`);
await b.until(`!!document.querySelector('.dialog [name="cpu_percent"]')`, 8000);
await b.eval(`document.querySelector('.dialog [name="cpu_percent"]').value = '88'; 0`);
await clearToasts();
await b.eval(`document.querySelector('.dialog [data-live-submit]').click(); 0`);
await sleep(1500);
p = await lastPost();
check('monitor → POST /node/profile', p?.url === `${W}/node/profile` && p.status === 200 && p.req.profile.thresholds.cpu_percent === 88 && p.req.profile.maintenance_until === 0, { status: p?.status, req: p?.req, err: p?.body?.error });
check('monitor toast', (await lastToast()) === '监控设置已保存', await lastToast());
// the monitor re-evaluates every 10 s and the page reads health on its next poll / SSE change: allow 45 s
const lim88 = await b.until(`__app.store.node(${JSON.stringify(id)}).limits?.cpu_percent === 88`, 45000);
check('limits.cpu_percent now 88 in the live data', lim88);

// 6. incident ack (only if the node / fleet has an open incident)
const inc = await b.eval(`(() => { const x = __app.store.incidents({ open: true })[0]; return x ? x.node : null; })()`);
if (inc) {
  await b.eval(`__app.openNode(${JSON.stringify(inc)}, 'quality'); 0`);
  if (await b.until(`!!document.querySelector('.drawer [data-dz="ack"]')`, 6000)) {
    await clearToasts();
    await b.eval(`document.querySelector('.drawer [data-dz="ack"]').click(); 0`);
    await sleep(1500);
    p = await lastPost();
    check('incident ack → POST /incidents/ack', p?.url === `${W}/incidents/ack` && p.status === 200, p?.body);
  } else check('incident ack (no open incident row in the drawer)', true, 'skipped');
} else check('incident ack (no open incident on staging)', true, 'skipped');

// 7. behavior event ack
await b.eval(`__app.closeDrawerParams(); location.hash = '#/behavior?view=events'; 0`);
if (await b.until(`!!document.querySelector('[data-act="ack"]')`, 8000)) {
  await b.eval(`document.querySelector('[data-act="ack"]').click(); 0`);
  await sleep(1500);
  p = await lastPost();
  check('behavior ack → POST /behavior/ack', p?.url === `${W}/behavior/ack` && p.status === 200, p?.body);
} else check('behavior ack (no unacked event on staging)', true, 'skipped');

}
// 7b. fleetd's own writes (事项与告警), when the page runs on fleetd (fleet_front on a395)
if (await b.eval('!!globalThis.TEAMOPS?.fleet?.available')) {
  await b.eval(`__app.closeDrawerParams(); location.hash = '#/alerts'; 0`);
  if (await b.until(`!!document.querySelector('[data-slot="list"] [data-act="ack"]')`, 10000)) {
    await clearToasts();
    await b.eval(`document.querySelector('[data-slot="list"] [data-act="ack"]').click(); 0`);
    await sleep(1500);
    p = await lastPost();
    check('alert ack → POST /v2/api/v1/alerts/{id}/ack with X-Fleet-Request', /^\/v2\/api\/v1\/alerts\/\d+\/ack$/.test(p?.url ?? '') && p.status === 200 && p.headers['X-Fleet-Request'] === '1', { url: p?.url, status: p?.status, err: p?.body?.error });
    check('alert ack toast', /^已确认/.test(await lastToast() ?? ''), await lastToast());
  } else check('alert ack (no unacked alert on the fleetd test instance)', true, 'skipped');
  await b.eval(`document.querySelector('[data-act="new-window"][data-kind="silence"]').click(); 0`);
  await b.until(`!!document.querySelector('.dialog [data-create]')`, 5000);
  await b.eval(`document.querySelector('.dialog [name="comment"]').value = 'v2 写入验收'; 0`);
  await clearToasts();
  await b.eval(`document.querySelector('.dialog [data-create]').click(); 0`);
  await sleep(1500);
  p = await lastPost();
  check('silence → POST /v2/api/v1/silences (201)', p?.url === '/v2/api/v1/silences' && p.status === 201 && p.req.matchers?.[0]?.name === 'relay' && p.req.duration_minutes === 120, { status: p?.status, req: p?.req, err: p?.body?.error });
  await sleep(1500);
  await b.shot(path.join(OUT, 'alerts-silence-light-1440.png'));
  if (await b.until(`!!document.querySelector('[data-act="del-window"]')`, 5000)) {
    await b.eval(`document.querySelector('[data-act="del-window"]').click(); 0`);
    await b.until(`!!document.querySelector('.dialog [data-dlg="ok"]')`, 5000);
    await sleep(400);
    await b.eval(`document.querySelector('.dialog [data-dlg="ok"]').click(); 0`);
    await sleep(1500);
    p = await lastPost();
    check('end silence → DELETE /v2/api/v1/silences/{id}', /^\/v2\/api\/v1\/silences\/\d+$/.test(p?.url ?? '') && p.method === 'DELETE' && p.status === 200, { url: p?.url, status: p?.status });
  } else check('silence listed after creation', false);
}

// 8. class b stays a notice and sends nothing
const n1 = await nPosts();
await b.eval(`location.hash = '#/lines'; 0`);
await b.until(`!!document.querySelector('[data-act="disable"]')`, 8000);
await clearToasts();
await b.eval(`document.querySelector('[data-act="disable"]').click(); 0`);
await sleep(500);
check('class b (停用入口) → 新后台尚未开放 notice, no POST, no dialog', (await nPosts()) === n1 && /新后台尚未开放/.test(await lastToast() ?? '') && !(await b.eval(`!!document.querySelector('.dialog')`)), await lastToast());
await b.shot(path.join(OUT, 'moved-notice-light-1440.png'));

const errs = b.logs.filter((l) => !/status of 400/.test(l) && !/\/v2\/api\/v1\/system/.test(l)); // the fleetd probe 404s by design where fleetd is absent
check('console clean (the intended 400 aside)', errs.length === 0, errs.slice(0, 3));
const bad = results.filter((r) => !r.ok).length;
console.log(`\n${results.length - bad}/${results.length} checks passed`);
await b.close();
process.exit(bad ? 1 : 0);
