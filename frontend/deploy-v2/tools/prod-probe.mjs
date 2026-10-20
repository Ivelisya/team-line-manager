// prod-probe.mjs — READ-ONLY look at production with a session cookie: load the app, wait, record timings,
// failed requests, console errors and non-GET requests (must be 0), screenshot.  Clicks nothing.
//   PROD_SESSION_FILE=~/.config/teamops/prod-session node deploy/v2/tools/prod-probe.mjs [--url https://38.58.57.22/#/home] [--out dir]
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import { launch, sleep } from './cdp.mjs';
const arg = (k, d) => { const i = process.argv.indexOf(k); return i > 0 ? process.argv[i + 1] : d; };
const URL_ = arg('--url', 'https://38.58.57.22/#/home');
const OUT = arg('--out', path.join(os.tmpdir(), 'prod-probe'));
const tok = fs.readFileSync(process.env.PROD_SESSION_FILE ?? path.join(os.homedir(), '.config/teamops/prod-session'), 'utf8').trim();
fs.mkdirSync(OUT, { recursive: true });
const b = await launch({ readOnly: true });
await b.send('Security.setIgnoreCertificateErrors', { ignore: true });
const host = new URL(URL_).hostname;
await b.send('Network.setCookie', { name: 'fleet_session', value: tok, domain: host, path: '/', secure: true, httpOnly: true, sameSite: 'Strict' });
await b.send('Emulation.setDeviceMetricsOverride', { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false });
const t0 = Date.now();
await b.send('Page.navigate', { url: URL_ });
const ready = await b.until("document.documentElement.dataset.ready === '1'", 40000);
const readyMs = Date.now() - t0;
await sleep(6000);
const st = await b.eval(`(() => ({ ready: document.documentElement.dataset.ready, fresh: document.querySelector('[data-fresh]')?.innerText, skel: document.querySelectorAll('.skel').length, cards: document.querySelectorAll('.ncard').length, sys: globalThis.TEAMOPS?.fleet?.system?.version, legacy: globalThis.TEAMOPS?.fleet?.system?.legacy_source, decom: globalThis.TEAMOPS?.decommissioned, status: globalThis.TEAMOPS?.poller?.status?.(), text: document.getElementById('content')?.innerText.slice(0, 300) }))()`);
await b.shot(path.join(OUT, 'home.png'));
console.log(JSON.stringify({ ready, readyMs, st, failed: b.ev.failed, console: b.ev.console, exceptions: b.ev.exceptions, nonGet: b.ev.api.filter((x) => x.method !== 'GET').map((x) => x.url), blocked: b.ev.blocked }, null, 1));
await b.close();
