// 在本机复刻上逐屏截图并量取计算样式。
// 先启动：node server.mjs 5198   再运行：node capture.mjs [场景名过滤正则]
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { launch } from './chrome.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const BASE = 'https://127.0.0.1:5198';
const OUT = path.join(here, 'renders');
const MEAS = path.join(here, 'measure');
fs.mkdirSync(OUT, { recursive: true });
fs.mkdirSync(MEAS, { recursive: true });
const PROBE = fs.readFileSync(path.join(here, 'probe.js'), 'utf8');
const filter = process.argv[2] ? new RegExp(process.argv[2]) : null;

const b = await launch({});
const W = 1440, H = 900;
const FRAME = `document.querySelector('iframe[title="节点管理"]')`;
const ops = code => b.eval(`${FRAME}.contentWindow.eval(${JSON.stringify(code)})`);
const opsReady = () => b.until(`(()=>{const f=${FRAME};const d=f&&f.contentDocument;return d&&d.querySelector('#main .page-heading h1')&&!d.querySelector('#main[aria-busy]')&&!d.querySelector('#main .skeleton')})()`, 20000);
const click = sel => ops(`(()=>{const e=document.querySelector(${JSON.stringify(sel)});if(!e)return false;e.click();return true})()`);
const waitOps = (expr, t = 10000) => b.until(`${FRAME}.contentWindow.eval(${JSON.stringify(expr)})`, t);

async function prep({ dark = false, density = 'comfortable', login = true }) {
  await fetch(BASE + '/__mock/login?state=' + (login ? 1 : 0)).catch(() => {});
  await b.viewport(W, H);
  await b.media(dark);
  await b.go(BASE + '/komari-admin/manifest.json', 300);
  await b.eval(`localStorage.setItem('appearance', ${JSON.stringify(JSON.stringify(dark ? 'dark' : 'light'))});localStorage.setItem('ops-density', ${JSON.stringify(density)});localStorage.removeItem('ops-inbound-size');sessionStorage.clear();true`);
}

async function snap(name, { probe, full = false, fullNative = false } = {}) {
  await b.sleep(400);
  await b.shot(path.join(OUT, name + '.png'));
  const res = {};
  if (probe) for (const k of [].concat(probe)) res[k] = await b.eval(`(${PROBE})(${JSON.stringify(k)})`).catch(e => ({ error: String(e) }));
  if (Object.keys(res).length) fs.writeFileSync(path.join(MEAS, name + '.json'), JSON.stringify(res, null, 1));
  if (full) {
    const h = await b.eval(`${FRAME}.contentDocument.scrollingElement.scrollHeight`);
    const top = await b.eval(`${FRAME}.getBoundingClientRect().top`);
    await b.viewport(W, Math.min(9000, Math.ceil(h + top)));
    await b.sleep(700);
    await b.shot(path.join(OUT, name + '-full.png'));
    await b.viewport(W, H);
    await b.sleep(300);
  }
  if (fullNative) {
    const h = await b.eval(`(()=>{const c=document.querySelector('.km-admin-panel-content > div');return c?c.scrollHeight+c.getBoundingClientRect().top:document.body.scrollHeight})()`);
    await b.viewport(W, Math.min(6000, Math.ceil(h + 8)));
    await b.sleep(700);
    await b.shot(path.join(OUT, name + '-full.png'));
    await b.viewport(W, H);
    await b.sleep(300);
  }
  console.log('shot', name);
}

const scenes = [
  // ---- 运营页（浅色、宽松） ----
  { name: 'ops-overview', url: '/admin/dashboard', run: async () => { await snap('ops-overview', { probe: ['ops', 'native'], full: true }); } },
  { name: 'ops-nodes', url: '/admin/nodes', run: async () => { await snap('ops-nodes', { probe: 'ops', full: true }); } },
  { name: 'ops-drawer', url: '/admin/nodes', run: async () => {
    await click('tr.row-link[data-value="t1-21"]');
    await waitOps(`document.querySelector('#drawer .drawer-tabs')`);
    for (const tab of ['overview', 'availability', 'traffic', 'billing', 'activity', 'actions']) {
      await click(`[data-action="drawer-tab"][data-value="${tab}"]`);
      await b.sleep(500);
      await snap('ops-drawer-' + tab, { probe: tab === 'overview' || tab === 'availability' ? 'ops' : undefined });
    }
  } },
  { name: 'ops-dialogs-asset', url: '/admin/nodes', run: async () => {
    await click('tr.row-link[data-value="t1-01"]');
    await waitOps(`document.querySelector('#drawer .drawer-tabs')`);
    await click('[data-action="drawer-tab"][data-value="billing"]'); await b.sleep(400);
    await click('[data-action="edit-asset"]'); await waitOps(`document.querySelector('#modal .modal')`); await snap('ops-dialog-edit-asset', { probe: 'ops' });
    await ops('closeModal()'); await click('[data-action="renew-asset"]'); await waitOps(`document.querySelector('#modal .modal')`); await snap('ops-dialog-renew');
    await ops('closeModal()'); await click('[data-action="drawer-tab"][data-value="actions"]'); await b.sleep(400);
    await click('[data-action="node-settings"]'); await waitOps(`document.querySelector('#monitor-form')`); await snap('ops-dialog-monitor');
  } },
  { name: 'ops-dialog-wizard', url: '/admin/nodes', run: async () => {
    await click('[data-action="enroll"]'); await waitOps(`document.querySelector('#provision-form') && !document.querySelector('[data-action=wizard-next][disabled]')`);
    await snap('ops-dialog-wizard-node', { probe: 'ops' });
    const h = await ops(`document.querySelector('#modal .modal').scrollHeight`);
    await b.viewport(W, Math.min(2600, h + 120)); await b.sleep(500); await b.shot(path.join(OUT, 'ops-dialog-wizard-node-full.png')); await b.viewport(W, H);
  } },
  { name: 'ops-lines', url: '/admin/lines', run: async () => {
    await b.sleep(800);
    await snap('ops-lines', { probe: 'ops', full: true });
    await click('.inbound-table tr.row-link[data-value="线路机1|20002"]'); await b.sleep(500);
    await ops(`document.querySelector('.row-detail')?.scrollIntoView({block:'center'})`); await b.sleep(300);
    await snap('ops-lines-expanded', { probe: 'ops' });
    await click('.job-row .job-head'); await b.sleep(800); await ops(`document.querySelector('#jobs-panel')?.scrollIntoView({block:'start'})`); await b.sleep(300);
    await snap('ops-lines-jobs');
    await ops(`document.querySelector('#audit')?.scrollIntoView({block:'start'})`); await b.sleep(300);
    await snap('ops-lines-audit', { probe: 'ops' });
  } },
  { name: 'ops-dialogs-panel', url: '/admin/lines', run: async () => {
    await b.sleep(800);
    await click('[data-action="panel-reset"]'); await waitOps(`document.querySelector('#modal .modal')`); await snap('ops-dialog-panel-reset', { probe: 'ops' });
    await ops('closeModal()'); await click('.inbound-table [data-action="panel-toggle"]'); await waitOps(`document.querySelector('#modal .modal')`); await snap('ops-dialog-panel-disable');
    await ops('closeModal()'); await click('[data-action="panel-restart"]'); await waitOps(`document.querySelector('#modal .modal')`); await snap('ops-dialog-xray-restart');
    await ops('closeModal()'); await click('[data-action="panel-logs"]'); await waitOps(`document.querySelector('#modal .log-box')`); await snap('ops-dialog-xray-logs');
    await ops('closeModal()'); await click('[data-action="provision-relay"]'); await waitOps(`document.querySelector('#provision-form')`); await b.sleep(600); await snap('ops-dialog-wizard-relay');
  } },
  { name: 'ops-behavior', url: '/admin/behavior', run: async () => {
    await b.sleep(800);
    await snap('ops-behavior', { probe: 'ops', full: true });
    for (const tab of ['events', 'sources', 'nodes']) { await click(`[data-action="behavior-tab"][data-value="${tab}"]`); await b.sleep(600); await snap('ops-behavior-' + tab, { probe: tab === 'events' ? 'ops' : undefined }); }
    await click('[data-action="behavior-tab"][data-value="targets"]'); await b.sleep(600);
    await click('[data-action="target-records"]'); await waitOps(`document.querySelector('#modal .evidence-meta')`); await snap('ops-dialog-evidence', { probe: 'ops' });
  } },
  { name: 'ops-rules', url: '/admin/behavior?tab=rules', run: async () => {
    await b.sleep(800); await snap('ops-rules', { probe: 'ops', full: true });
    await click('[data-action="policy-preview"]'); await waitOps(`document.querySelector('#modal .modal.wide')`); await snap('ops-dialog-policy-preview');
  } },
  { name: 'ops-activity', url: '/admin/activity', run: async () => { await b.sleep(600); await snap('ops-activity', { probe: 'ops', full: true }); } },
  { name: 'ops-apikeys', url: '/admin/api-keys', run: async () => {
    await b.sleep(600); await snap('ops-apikeys', { probe: 'ops', full: true });
    await click('[data-action="apikey-new"]'); await waitOps(`document.querySelector('#apikey-name')`); await snap('ops-dialog-apikey-new', { probe: 'ops' });
    await ops('closeModal()'); await ops(`toast('已刷新')`); await b.sleep(250); await snap('ops-toast'); await ops(`toast('操作失败（HTTP 500）',true)`); await b.sleep(250); await snap('ops-toast-error', { probe: 'ops' });
  } },
  // ---- 紧凑密度 ----
  { name: 'ops-compact', url: '/admin/nodes', density: 'compact', run: async () => { await snap('ops-nodes-compact', { probe: 'ops', full: true }); } },
  { name: 'ops-compact-lines', url: '/admin/lines', density: 'compact', run: async () => { await b.sleep(800); await snap('ops-lines-compact', { probe: 'ops', full: true }); } },
  // ---- 深色 ----
  { name: 'dark-overview', url: '/admin/dashboard', dark: true, run: async () => { await snap('dark-ops-overview', { probe: ['ops', 'native'], full: true }); } },
  { name: 'dark-nodes', url: '/admin/nodes', dark: true, run: async () => { await snap('dark-ops-nodes', { probe: 'ops' }); await click('tr.row-link[data-value="t1-21"]'); await waitOps(`document.querySelector('#drawer .drawer-tabs')`); await b.sleep(500); await snap('dark-ops-drawer', { probe: 'ops' }); } },
  { name: 'dark-lines', url: '/admin/lines', dark: true, run: async () => { await b.sleep(800); await snap('dark-ops-lines', { probe: 'ops' }); await click('[data-action="panel-reset"]'); await waitOps(`document.querySelector('#modal .modal')`); await snap('dark-ops-dialog', { probe: 'ops' }); } },
  { name: 'dark-behavior', url: '/admin/behavior', dark: true, run: async () => { await b.sleep(800); await snap('dark-ops-behavior'); } },
  // ---- 原生外壳与原生页 ----
  { name: 'native-login', url: '/admin/home', login: false, native: true, run: async () => { await b.until(`document.querySelector('.km-private-entry .rt-DialogContent, .km-restricted-login-dialog')`, 10000); await snap('native-login', { probe: 'native' }); } },
  { name: 'native-login-dark', url: '/admin/home', login: false, dark: true, native: true, run: async () => { await b.until(`document.querySelector('.km-restricted-login-dialog')`, 10000); await snap('native-login-dark', { probe: 'native' }); } },
  { name: 'native-servers', url: '/admin/servers', native: true, run: async () => {
    await b.until(`document.querySelector('.km-admin-panel-content table')`, 12000); await b.sleep(800);
    await b.eval(`(()=>{const g=[...document.querySelectorAll('.km-admin-panel-nav [role=button]')].find(e=>/运维/.test(e.textContent));g&&g.click();return true})()`); await b.sleep(600);
    await snap('native-servers', { probe: 'native', fullNative: true });
  } },
  { name: 'native-site', url: '/admin/settings/site', native: true, run: async () => { await b.sleep(2500); await snap('native-settings-site', { probe: 'native', fullNative: true }); } },
  { name: 'native-sessions', url: '/admin/sessions', native: true, run: async () => { await b.sleep(2500); await snap('native-sessions', { probe: 'native' }); } },
  { name: 'native-logs', url: '/admin/logs', native: true, run: async () => { await b.sleep(2500); await snap('native-logs', { probe: 'native' }); } },
  { name: 'native-account', url: '/admin/account', native: true, run: async () => { await b.sleep(2500); await snap('native-account', { probe: 'native' }); } },
  { name: 'native-ping', url: '/admin/ping', native: true, run: async () => { await b.sleep(2500); await snap('native-ping', { probe: 'native' }); } },
  { name: 'native-notify', url: '/admin/notification/general', native: true, run: async () => { await b.sleep(2500); await snap('native-notification-general', { probe: 'native' }); } },
  { name: 'native-metrics', url: '/admin/metrics-overview', native: true, run: async () => { await b.sleep(3500); await snap('native-metrics-overview', { probe: 'native', fullNative: true }); } },
  { name: 'native-dark-servers', url: '/admin/servers', dark: true, native: true, run: async () => { await b.until(`document.querySelector('.km-admin-panel-content table')`, 12000); await b.sleep(800); await snap('dark-native-servers', { probe: 'native' }); } },
  { name: 'native-dark-site', url: '/admin/settings/site', dark: true, native: true, run: async () => { await b.sleep(2500); await snap('dark-native-settings-site', { probe: 'native' }); } },
];

for (const s of scenes) {
  if (filter && !filter.test(s.name)) continue;
  try {
    await prep(s);
    await b.go(BASE + s.url, 800);
    if (!s.native) { const ok = await opsReady(); if (!ok) console.log('WARN not ready', s.name); await b.sleep(600); }
    else await b.sleep(1200);
    await s.run();
  } catch (e) { console.log('FAIL', s.name, e.message); }
}
fs.writeFileSync(path.join(MEAS, '_console.txt'), b.logs.join('\n'));
await b.close();
console.log('done');
