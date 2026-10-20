// audit.mjs — SPEC §8.2 验收里能自动化的几条：V3 字号/字重/圆角/控件高、V4 术语、V5 套餐原文、V6 布局抖动、V8 窄屏溢出、V13 控制台。
// 用法：~/.local/bin/node tools/audit.mjs [hash=#/home] [clsSeconds=20]
// V2（裸 hex、!important）用 grep：grep -nE '#[0-9a-fA-F]{3,8}\b|!important' css/*.css

import { launch } from './shoot.mjs';

const BASE = 'http://127.0.0.1:5199/original-refined/index.html';
const hash = process.argv[2] ?? '#/home';
const clsSeconds = Number(process.argv[3] ?? 20);

const RUNTIME = `(() => {
  const FS = new Set(['11px','12px','13px','15px','16px','24px','27px']);
  const FW = new Set(['400','500','600']);
  const RADII = new Set(['0px','4px','8px','10px','14px','999px','50%']);
  const bad = { fs: new Map(), fw: new Map(), radius: new Map(), height: [] };
  const add = (m, k, el) => { if (!m.has(k)) m.set(k, (el.className?.baseVal ?? el.className ?? el.tagName) + ''); };
  for (const el of document.querySelectorAll('body *')) {
    if (el.closest('svg') || el.closest('.u-wrap canvas')) continue;
    const r = el.getBoundingClientRect();
    if (!r.width && !r.height) continue;
    const cs = getComputedStyle(el);
    const hasText = [...el.childNodes].some((n) => n.nodeType === 3 && n.textContent.trim());
    if (hasText && !FS.has(cs.fontSize)) add(bad.fs, cs.fontSize, el);
    if (hasText && !FW.has(cs.fontWeight)) add(bad.fw, cs.fontWeight, el);
    for (const c of ['borderTopLeftRadius','borderTopRightRadius','borderBottomLeftRadius','borderBottomRightRadius']) {
      const v = cs[c];
      if (v && !RADII.has(v) && !(parseFloat(v) >= 999) && !(v.endsWith('%'))) add(bad.radius, v, el);
    }
    if (el.matches('.btn:not(.btn-link), .icon-btn, .input, .select, .seg, .fchip, .top-btn, .search-trigger')) {
      const h = Math.round(r.height);
      if (![20,28,32,36].includes(h)) bad.height.push((el.className?.baseVal ?? el.className) + ' ' + h);
    }
  }
  return { fontSizes: [...bad.fs], fontWeights: [...bad.fw], radii: [...bad.radius], heights: bad.height.slice(0, 20) };
})()`;

const TERMS = `(async () => {
  const { fmt } = await import('/shared/index.js');
  const text = document.body.innerText;
  const extra = ['异常', '节点可用', '连接不稳定', '上传', '下载', '出站', '入站', 'KiB/s', 'MiB/s', '拦截', '个节点', '入口流量', '需要关注']; // 拦截 / 个节点 / 入口流量：consistency #8、#9（shared/format.js 的 BANNED 不在本目录，这里补查）
  const rawPlans = ['基础版 VPS', '标准版 VPS', 'LAX.Pulse', 'Acapulco', '2000 GB 出站', '1000 GB 出站', '高速代理 ·', '原文'];
  return { lint: fmt.lintTerms(text), extra: extra.filter((w) => text.includes(w)), rawPlans: rawPlans.filter((w) => text.includes(w)), length: text.length };
})()`;

const b = await launch({ port: Number(process.env.SHOOT_PORT) || 9371 }); // SHOOT_PORT：并行构建时各用各的调试端口
const out = {};
for (const [w, h, dark] of [[1440, 900, false], [1440, 900, true], [390, 844, false]]) {
  b.logs.length = 0;
  await b.viewport(w, h);
  await b.media({ dark, reduce: false });
  await b.go(`${BASE}${hash}`);
  await b.until('document.documentElement.dataset.ready === "1"', 20000);
  await b.sleep(1500);
  const key = `${w}${dark ? '-dark' : ''}`;
  out[key] = { runtime: await b.eval(RUNTIME), terms: await b.eval(TERMS), overflow: await b.eval('document.documentElement.scrollWidth - document.documentElement.clientWidth'), console: [...b.logs] };
}
// 抽屉的每个页签也过一遍术语与套餐原文
await b.viewport(1440, 900);
await b.media({ dark: false });
const drawerTerms = {};
for (const tab of ['overview', 'quality', 'traffic', 'host', 'billing', 'access', 'manage']) {
  for (const id of ['t1-01', 't1-13', 'r1', 't1-31']) {
    await b.go(`${BASE}#/home?node=${id}&tab=${tab}`);
    await b.until('document.documentElement.dataset.ready === "1"', 20000);
    await b.sleep(700);
    const t = await b.eval(TERMS);
    if (t.lint.length || t.extra.length || t.rawPlans.length) drawerTerms[`${id}:${tab}`] = t;
  }
}
out.drawerTerms = drawerTerms;
out.drawerConsole = [...b.logs];
// V6：实时更新跑 N 秒，累计 layout shift
await b.go(`${BASE}${hash}`);
await b.until('document.documentElement.dataset.ready === "1"', 20000);
await b.sleep(1500);
await b.eval(`(() => { window.__cls = 0; new PerformanceObserver((l) => { for (const e of l.getEntries()) if (!e.hadRecentInput) window.__cls += e.value; }).observe({ type: 'layout-shift', buffered: false }); return 1; })()`);
await b.sleep(clsSeconds * 1000);
out.cls = await b.eval('window.__cls');
out.clsSeconds = clsSeconds;
console.log(JSON.stringify(out, null, 1));
await b.close();
process.exit(0);
