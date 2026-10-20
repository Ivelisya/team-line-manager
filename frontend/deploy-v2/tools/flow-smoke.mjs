// flow-smoke.mjs — end-to-end flows of the WRITES=1 build against the fleetd staging instance on a395, through the
// tokenless same-origin front (staging/fleet_front.py --port 18399: no injected session, like production behind Caddy):
//
//   1 login page: 401 → the v2 login page (fleetd account), wrong password → error, right password → the app
//   2 account page: user, login method, two-step state, sessions
//   3 onboarding: new batch (CSV) → plan from fleetd → confirm hash → start → per-step status in the matrix →
//     step drawer log from fleetd events → cancel the batch (compensation)
//   4 logout → back to the login page
//
//   UITEST_PW=… ~/.local/bin/node deploy/v2/tools/flow-smoke.mjs [--base http://127.0.0.1:18399] [--out deploy/v2/shots/flows]
// The password comes from the environment (a395:~/team-ops-eval/fleet-stage/test-user.txt); it is never printed.
// Only talks to 127.0.0.1 (the tunnel); never production.  Staging batches use documentation IPs (198.51.100.0/24).
import fs from 'node:fs';
import path from 'node:path';
import { launch, sleep, js } from './cdp.mjs';

const HERE = path.dirname(new URL(import.meta.url).pathname);
const arg = (k, d) => { const i = process.argv.indexOf(k); return i > 0 ? process.argv[i + 1] : d; };
const BASE = arg('--base', 'http://127.0.0.1:18399');
const OUT = path.resolve(arg('--out', path.join(HERE, '..', 'shots', 'flows')));
const USER = process.env.UITEST_USER ?? 'uitest';
const PW = process.env.UITEST_PW ?? '';
if (!/^http:\/\/(127\.0\.0\.1|localhost)(:\d+)?$/.test(BASE)) { console.error('flow smoke only runs against a local tunnel (127.0.0.1)'); process.exit(2); }
if (!PW) { console.error('UITEST_PW is not set'); process.exit(2); }
fs.mkdirSync(OUT, { recursive: true });

const results = [];
const check = (name, ok, info = '') => { results.push({ name, ok: !!ok, info }); console.log(`${ok ? '✓' : '✗'} ${name}${info ? ` — ${String(info).slice(0, 300)}` : ''}`); };
const S = (x) => JSON.stringify(x);

async function setInput(b, sel, value) {
  await b.eval(`(() => { const i = document.querySelector(${S(sel)}); i.focus(); i.value = ${S(value)}; i.dispatchEvent(new Event('input', { bubbles: true })); i.dispatchEvent(new Event('change', { bubbles: true })); return true; })()`);
}
const click = (b, sel) => b.eval(`(() => { const x = document.querySelector(${S(sel)}); if (!x) return false; x.click(); return true; })()`);
const toastText = (b) => b.eval(`[...document.querySelectorAll('.toasts .toast .toast-msg')].map((t) => t.textContent).join(' | ')`);

async function loginFlow(mode) {
  const b = await launch();
  await b.send('Emulation.setDeviceMetricsOverride', { width: mode.width, height: mode.height, deviceScaleFactor: 1, mobile: !!mode.mobile });
  await b.send('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-color-scheme', value: mode.dark ? 'dark' : 'light' }, { name: 'prefers-reduced-motion', value: 'reduce' }] });
  await b.send('Page.navigate', { url: `${BASE}/admin/v2/app/#/home` });
  const shown = await b.until("!!document.querySelector('.login-screen form')", 20000);
  const lp = await b.eval(js(() => {
    const f = document.querySelector('.login-screen form');
    const btn = f?.querySelector('.login-submit');
    const body = f?.querySelector('.dialog-body');
    return {
      brand: f?.querySelector('.login-brand')?.textContent, title: f?.querySelector('.dialog-title')?.textContent, sub: f?.querySelector('.dialog-sub')?.textContent,
      fullWidth: btn && body ? Math.abs(btn.getBoundingClientRect().width - (body.getBoundingClientRect().width - parseFloat(getComputedStyle(body).paddingLeft) - parseFloat(getComputedStyle(body).paddingRight))) < 2 : false,
      totpHidden: !!f && getComputedStyle(f.querySelector('[data-login-totp]')).display === 'none', legacyLink: !!f?.querySelector('.login-alt'), otherDialogs: document.querySelectorAll('.dialog:not(.login-card)').length,
      overflowX: document.documentElement.scrollWidth > document.documentElement.clientWidth + 1,
    };
  }));
  const loc = await b.eval('location.pathname + location.hash');
  check(`[${mode.name}] old address /admin/v2/app/#/home → site root, hash kept`, loc === '/#/home', loc);
  check(`[${mode.name}] 401 → v2 login page`, shown && lp.brand === '团队线路管理' && lp.title === '登录' && lp.fullWidth && lp.totpHidden && lp.otherDialogs === 0 && !lp.overflowX, S(lp));
  const unauth = b.ev.failed.filter((x) => /^401 /.test(x));
  check(`[${mode.name}] before login: one 401 (the fleetd detection), nothing else failed`, unauth.length === 1 && b.ev.failed.length === 1 && /\/v2\/api\/v1\/system$/.test(unauth[0]), S(b.ev.failed));
  await b.shot(path.join(OUT, `login-${mode.name}.png`));
  b.reset();
  // wrong password
  await setInput(b, '.login-screen [name="username"]', USER);
  await setInput(b, '.login-screen [name="password"]', 'definitely-wrong-password');
  await click(b, '.login-screen .login-submit');
  await b.until("!document.querySelector('[data-login-err]')?.hidden", 8000);
  const err = await b.eval("document.querySelector('[data-login-err]')?.textContent");
  check(`[${mode.name}] wrong password → error in the form`, err === '用户名或密码错误', err);
  if (mode.name === 'light-1440') await b.shot(path.join(OUT, `login-error-${mode.name}.png`));
  // right password → reload → app
  await setInput(b, '.login-screen [name="password"]', PW);
  await click(b, '.login-screen .login-submit');
  await sleep(500);
  const ready = await b.until("document.documentElement.dataset.ready === '1' && !!globalThis.TEAMOPS?.poller && !document.querySelector('.login-screen')", 30000);
  const app = await b.eval(js(() => ({ auth: globalThis.TEAMOPS?.fleet?.has?.('auth'), onboard: globalThis.TEAMOPS?.fleet?.has?.('onboard'), nodes: globalThis.__app?.store?.nodes?.().length, account: !!document.querySelector('.nav-item[data-nav="account"]'), writes: globalThis.TEAMOPS_WRITES })));
  check(`[${mode.name}] login → app with live data on the fleetd session`, ready && app.auth && app.nodes > 0 && app.account, S(app));
  return b;
}

async function accountFlow(b) {
  b.reset();
  await b.eval("location.hash = '#/account'; 0");
  await b.until("!!document.querySelector('[data-page=\"account\"] .kv')", 10000);
  await sleep(800);
  const a = await b.eval(js(() => ({
    kv: [...document.querySelectorAll('[data-page="account"] .kv-item')].map((x) => x.innerText.replace(/\s+/g, ' ').trim()),
    pw: !!document.querySelector('[data-form="pw"]'), totp: !!document.querySelector('[data-act="totp-setup"], [data-act="totp-off"]'),
    sessions: document.querySelectorAll('[data-page="account"] [data-slot="sessions"] tr[data-row]').length,
  })));
  check('account page: user, fleetd login, password form, two-step, sessions', a.kv.some((x) => x.includes(USER)) && a.kv.some((x) => x.includes('新后台账户')) && a.pw && a.totp && a.sessions >= 1, S(a));
  await b.shot(path.join(OUT, 'account-light-1440.png'));
  check('account page: no errors', !b.ev.failed.length && !b.ev.console.length && !b.ev.exceptions.length, S([...b.ev.failed, ...b.ev.console, ...b.ev.exceptions]));
}

async function onboardingFlow(b) {
  b.reset();
  await b.eval("location.hash = '#/onboarding'; 0");
  await b.until("!!document.querySelector('[data-page=\"onboarding\"] [data-slot=\"batch\"] .panel')", 15000);
  await sleep(1500);
  await b.shot(path.join(OUT, 'onboarding-before-light-1440.png'));
  await click(b, '[data-page="onboarding"] .page-head [data-act="new"]');
  const dlg = await b.until("document.querySelector('.dialog .dialog-title')?.textContent === '新建批次'", 5000);
  check('onboarding: 新建批次 opens the live dialog', dlg);
  const stamp = Date.now() % 250;
  const csv = `provider,ip,ssh_port,auth,plan,team,number,service_id,note\nATTVPS,198.51.100.${stamp + 1},22,key,ATTVPS 基础版,团队1,${90 + (stamp % 9)},SMOKE-${stamp},v2 flow smoke（文档地址段，不存在的机器）`;
  await setInput(b, '.dialog [data-ob-text]', csv);
  await sleep(200);
  await b.shot(path.join(OUT, 'onboarding-dialog-csv-light-1440.png'));
  const apiBefore = b.ev.api.length;
  await click(b, '.dialog [data-ob="plan"]');
  const planned = await b.until("!!document.querySelector('.dialog .ob-sum') || !!document.querySelector('.dialog .field-error')", 60000);
  const plan = await b.eval(js(() => ({
    sum: document.querySelector('.dialog .ob-sum')?.innerText.replace(/\s+/g, ' ').trim() ?? null,
    err: document.querySelector('.dialog .field-error')?.textContent ?? null,
    issues: [...document.querySelectorAll('.dialog .ob-issues li')].map((x) => x.textContent),
    rows: document.querySelectorAll('.dialog .ob-preview tr[data-row]').length,
    next: !document.querySelector('.dialog [data-ob="next"]')?.disabled,
  })));
  const posts = b.ev.api.slice(apiBefore).filter((x) => x.method === 'POST').map((x) => new URL(x.url).pathname);
  check('onboarding: 生成计划 → POST /onboard/batches → plan from fleetd', planned && plan.sum && plan.rows === 1 && posts.length === 1 && posts[0] === '/v2/api/v1/onboard/batches', S({ plan, posts }));
  await b.shot(path.join(OUT, 'onboarding-plan-light-1440.png'));
  if (!plan.next) { check('onboarding: plan has no errors (needed to start)', false, S(plan.issues)); return; }
  await click(b, '.dialog [data-ob="next"]');
  await b.until("!!document.querySelector('.dialog [data-ob-ok]')", 5000);
  const hash = await b.eval("document.querySelector('.dialog .kv')?.innerText.match(/[0-9a-f]{16}/)?.[0] ?? null");
  const startDisabled = await b.eval("document.querySelector('.dialog [data-ob=\"start\"]').disabled");
  check('onboarding: confirm step shows the plan hash; start needs the checkbox', !!hash && startDisabled, hash);
  await b.shot(path.join(OUT, 'onboarding-confirm-light-1440.png'));
  await click(b, '.dialog [data-ob-ok]');
  const n0 = b.ev.api.length;
  await click(b, '.dialog [data-ob="start"]');
  await b.until("!document.querySelector('.dialog')", 15000);
  const startPost = b.ev.api.slice(n0).filter((x) => x.method === 'POST').map((x) => new URL(x.url).pathname);
  check('onboarding: 开始 → POST /start {plan_hash}', startPost.length === 1 && /\/onboard\/batches\/b-[0-9a-f]+\/start$/.test(startPost[0]), S(startPost));
  // per-step status from fleetd (the staging namespace has no network: P1 fails or stops quickly)
  const moved = await b.until("[...document.querySelectorAll('[data-page=\"onboarding\"] .ob-matrix [data-cell]')].some((c) => ['running', 'failed', 'blocked', 'waiting_input', 'succeeded'].includes(c.dataset.step))", 60000);
  await sleep(4000);
  const m = await b.eval(js(() => ({
    title: document.querySelector('[data-page="onboarding"] [data-slot="batch"] .panel-title')?.textContent,
    status: document.querySelector('[data-page="onboarding"] [data-slot="batch"] .pill')?.textContent,
    cells: [...document.querySelectorAll('[data-page="onboarding"] .ob-matrix [data-cell]')].map((c) => c.dataset.step),
    notes: [...document.querySelectorAll('[data-page="onboarding"] .ob-note .note-body')].map((x) => x.textContent),
    tools: [...document.querySelectorAll('[data-page="onboarding"] [data-slot="batch"] .panel-tools .btn')].map((x) => x.textContent.trim()),
  })));
  check('onboarding: matrix shows the live per-step status', moved && m.cells.length >= 7, S(m));
  await b.shot(path.join(OUT, 'onboarding-running-light-1440.png'));
  // step drawer with the fleetd event log
  await click(b, '[data-page="onboarding"] .ob-matrix tr[data-row]');
  await b.until("!!document.querySelector('.drawer .log')", 8000);
  await sleep(2500);
  const log = await b.eval("[...document.querySelectorAll('.drawer .log .log-line, .drawer .log [data-line]')].map((x) => x.textContent.trim()).slice(0, 12)");
  check('onboarding: step drawer shows the log from fleetd events', Array.isArray(log) && log.length > 0, S(log));
  await b.shot(path.join(OUT, 'onboarding-drawer-light-1440.png'));
  await b.eval("document.querySelector('.drawer [data-dz=\"close\"], .drawer .drawer-head .icon-btn')?.click(); history.back(); 0");
  await sleep(800);
  // the doc-IP machine does not exist: wait for the run to stop (待介入 / 失败), show it, then cancel
  const stopped = await b.until("!/进行中/.test(document.querySelector('[data-page=\"onboarding\"] [data-slot=\"batch\"] .pill')?.textContent ?? '进行中')", 150000);
  const after = await b.eval(js(() => ({
    status: document.querySelector('[data-page="onboarding"] [data-slot="batch"] .pill')?.textContent,
    cells: [...document.querySelectorAll('[data-page="onboarding"] .ob-matrix [data-cell]')].map((c) => c.dataset.step),
    notes: [...document.querySelectorAll('[data-page="onboarding"] .ob-note .note-body')].map((x) => x.textContent),
    actions: [...document.querySelectorAll('[data-page="onboarding"] .ob-note-actions .btn')].map((x) => x.textContent.trim()),
  })));
  check('onboarding: the run stops at P1 (no such machine) → row needs attention, with actions', stopped && after.cells[0] !== 'running' && after.actions.length > 0, S(after));
  await b.shot(path.join(OUT, 'onboarding-attention-light-1440.png'));
  // cancel the batch (compensation) — typed confirmation with the batch id
  const bid = (startPost[0] ?? '').split('/')[6];
  const hasCancel = await click(b, '[data-page="onboarding"] [data-act="ob-cancel"]');
  if (hasCancel) {
    await b.until("!!document.querySelector('.dialog [data-type-confirm]')", 5000);
    await setInput(b, '.dialog [data-type-confirm]', bid);
    const n1 = b.ev.api.length;
    await click(b, '.dialog [data-dlg="ok"]');
    await b.until("[...document.querySelectorAll('[data-page=\"onboarding\"] [data-slot=\"batch\"] .pill')].some((p) => p.textContent.includes('已取消'))", 30000);
    const cancelPost = b.ev.api.slice(n1).filter((x) => x.method === 'POST').map((x) => new URL(x.url).pathname);
    const st = await b.eval("document.querySelector('[data-page=\"onboarding\"] [data-slot=\"batch\"] .pill')?.textContent");
    check('onboarding: 取消批次 → POST /cancel → 已取消', cancelPost.length === 1 && cancelPost[0].endsWith('/cancel') && /已取消/.test(st ?? ''), S({ cancelPost, st, toast: await toastText(b) }));
    await b.shot(path.join(OUT, 'onboarding-cancelled-light-1440.png'));
  } else check('onboarding: 取消批次 available', false, S(m.tools));
  // a second batch, cancelled while it runs: 202 cancelling → 取消中 → 已取消 (the engine finishes the step in flight first)
  await click(b, '[data-page="onboarding"] .page-head [data-act="new"]');
  await b.until("document.querySelector('.dialog .dialog-title')?.textContent === '新建批次'", 5000);
  await setInput(b, '.dialog [data-ob-text]', csv.replace(/198\.51\.100\.(\d+)/, (m, x) => `198.51.100.${(Number(x) % 250) + 2}`).replace(/,团队1,(\d+),/, (m, n) => `,团队1,${Number(n) + 1},`).replace(/SMOKE-(\d+)/, 'SMOKE-$1b'));
  await click(b, '.dialog [data-ob="plan"]');
  await b.until("!!document.querySelector('.dialog .ob-sum')", 60000);
  await click(b, '.dialog [data-ob="next"]');
  await b.until("!!document.querySelector('.dialog [data-ob-ok]')", 5000);
  await click(b, '.dialog [data-ob-ok]');
  await click(b, '.dialog [data-ob="start"]');
  await b.until("!document.querySelector('.dialog')", 15000);
  await b.until("/进行中/.test(document.querySelector('[data-page=\"onboarding\"] [data-slot=\"batch\"] .pill')?.textContent ?? '')", 30000);
  const title2 = await b.eval("document.querySelector('[data-page=\"onboarding\"] [data-slot=\"batch\"] .panel-title')?.textContent");
  const bid2 = await b.eval("[...document.querySelectorAll('[data-page=\"onboarding\"] [data-slot=\"list\"] tr[data-row]')].map((t) => t.dataset.row).find((x) => x.startsWith('batch:'))?.slice(6)");
  await click(b, '[data-page="onboarding"] [data-act="ob-cancel"]');
  await b.until("!!document.querySelector('.dialog [data-type-confirm]')", 5000);
  const want = await b.eval("document.querySelector('.dialog .field-label b')?.textContent");
  await setInput(b, '.dialog [data-type-confirm]', want);
  const n2 = b.ev.api.length;
  await click(b, '.dialog [data-dlg="ok"]');
  await sleep(1500);
  const t2 = await toastText(b);
  const done2 = await b.until("[...document.querySelectorAll('[data-page=\"onboarding\"] [data-slot=\"batch\"] .pill')].some((p) => p.textContent.includes('已取消'))", 120000);
  const post2 = b.ev.api.slice(n2).filter((x) => x.method === 'POST').map((x) => new URL(x.url).pathname);
  check('onboarding: cancel a running batch → 正在取消 → 已取消', done2 && post2.length === 1 && post2[0].endsWith('/cancel') && /取消/.test(t2), S({ title2, bid2, want, post2, t2 }));
  await b.shot(path.join(OUT, 'onboarding-cancelled-running-light-1440.png'));
  const noise = [...b.ev.failed, ...b.ev.console, ...b.ev.exceptions, ...b.ev.csp];
  check('onboarding flow: no failed requests / console errors / CSP', !noise.length, S(noise));
}

async function logoutFlow(b) {
  b.reset();
  await click(b, '[data-top="logout"]');
  const back = await b.until("!!document.querySelector('.login-screen form')", 20000);
  const posts = b.ev.api.filter((x) => x.method === 'POST').map((x) => new URL(x.url).pathname);
  check('logout → POST /auth/logout → login page again', back && posts.includes('/v2/api/v1/auth/logout'), S(posts));
}

const MODES = [
  { name: 'light-1440', width: 1440, height: 900 },
  { name: 'dark-1440', width: 1440, height: 900, dark: true },
  { name: 'light-390', width: 390, height: 844, mobile: true },
];
for (const mode of MODES) {
  const b = await loginFlow(mode);
  if (mode.name === 'light-1440') {
    await accountFlow(b);
    await onboardingFlow(b);
  }
  await logoutFlow(b);
  await b.close();
}
const bad = results.filter((r) => !r.ok);
console.log(`\nflows: ${results.length - bad.length}/${results.length} passed · screenshots: ${OUT}`);
fs.writeFileSync(path.join(OUT, 'flows.json'), JSON.stringify({ base: BASE, results }, null, 2));
process.exit(bad.length ? 1 : 0);
