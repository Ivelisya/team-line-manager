// scroll-check.mjs — every overlay with long content must scroll (owner: 「这些什么都不能下滑」).
// For each case and viewport (1440×900, 390×844): open the overlay, find its scroll container, send real wheel events
// (CDP Input.dispatchMouseEvent) over it, assert scrollTop grew and the last row is reachable; also PageDown / Space /
// ArrowDown by keyboard, and that the page behind did not move.  Local / staging only.
//   node deploy/v2/tools/scroll-check.mjs [--base http://127.0.0.1:18398] [--out shots/scroll]
import fs from 'node:fs';
import path from 'node:path';
import { launch, sleep } from './cdp.mjs';

const arg = (k, d) => { const i = process.argv.indexOf(k); return i > 0 ? process.argv[i + 1] : d; };
const BASE = arg('--base', 'http://127.0.0.1:18398');
const OUT = arg('--out', 'shots/scroll');
if (!/^http:\/\/127\.0\.0\.1/.test(BASE)) { console.error('local tunnel only'); process.exit(2); }
fs.mkdirSync(OUT, { recursive: true });

// open: JS that opens the overlay (returns nothing); box: the scroll container; last: the last item inside it.
const CASES = [
  { name: 'dialog-evidence', open: "location.hash = '#/behavior?view=events'", after: "document.querySelector('[data-list] tr[data-row], [data-table] tr[data-row]')?.click()",
    box: '.dialog .dialog-body', last: '.dialog .evidence-table tbody tr:last-child, .dialog .dialog-body > :last-child', over: '.dialog .evidence-table tbody' },
  { name: 'dialog-evidence-targets', open: "location.hash = '#/behavior?view=targets'", after: "document.querySelector('[data-list] tr[data-row], [data-table] tr[data-row]')?.click()",
    box: '.dialog .dialog-body', last: '.dialog .evidence-table tbody tr:last-child, .dialog .dialog-body > :last-child', over: '.dialog .evidence-table tbody' },
  // a long dialog through the app's own openDialog (same module instance): 40 rows in the page's table markup
  { name: 'dialog-long', open: "location.hash = '#/behavior'", after: "import('/app/js/ui/layers.js').then((m) => m.openDialog({ title: '长对话框', size: 'lg', body: `<p class=\"dialog-text\">滚动测试</p><div class=\"evidence-table\"><div class=\"table-wrap\"><table class=\"dt\"><tbody>${Array.from({ length: 40 }, (_, i) => `<tr><td>第 ${i + 1} 行</td><td>198.51.100.${i}</td></tr>`).join('')}</tbody></table></div></div>` }))",
    box: '.dialog .dialog-body', last: '.dialog .dialog-body tr:last-child', over: '.dialog .dialog-body tbody' },
  // a table that truly needs horizontal scrolling (wider than any dialog): the vertical wheel over it must still scroll the dialog
  { name: 'dialog-wide', open: "location.hash = '#/behavior'", after: "import('/app/js/ui/layers.js').then((m) => m.openDialog({ title: '宽表', size: 'lg', body: `<div class=\"table-wrap\"><table class=\"dt\" style=\"min-width:1400px\"><tbody>${Array.from({ length: 40 }, (_, i) => `<tr><td>第 ${i + 1} 行</td><td>198.51.100.${i}</td><td>很长很长很长的一列</td></tr>`).join('')}</tbody></table></div>` }))",
    box: '.dialog .dialog-body', last: '.dialog .dialog-body tr:last-child', over: '.dialog .dialog-body tbody' },
  { name: 'drawer-access', open: "location.hash = '#/home'", after: "__app.openNode(__app.store.nodes().filter((n) => !n.isRelay).sort((a, b) => (b.rate?.down_bps ?? 0) - (a.rate?.down_bps ?? 0))[0].id, 'access')",
    box: '.drawer .drawer-body', last: '.drawer .drawer-body > :last-child', over: '.drawer [data-recent] tbody' },
  { name: 'drawer-overview', open: "location.hash = '#/home'", after: "__app.openNode(__app.store.nodes().find((n) => n.hasHost && !n.isRelay).id, 'overview')",
    box: '.drawer .drawer-body', last: '.drawer .drawer-body > :last-child' },
  // pages: the wheel over a wide table scrolls the page (the table's horizontal scroll must not swallow it)
  { name: 'page-nodes', open: "location.hash = '#/nodes'", box: 'html', last: '.dt tbody tr:last-child', over: '.dt tbody', page: true },
  { name: 'page-lines', open: "location.hash = '#/lines'", box: 'html', last: '[data-relay]:last-of-type', over: '.relay .dt tbody, [data-relay] .dt tbody', page: true },
  { name: 'palette', open: "location.hash = '#/home'", key: 'mod+k', box: '.palette .palette-list', last: '.palette .palette-list > :last-child' },
];
const MODES = [{ name: '1440', w: 1440, h: 900 }, { name: '1440-short', w: 1440, h: 520 }, { name: '390', w: 390, h: 844, mobile: true }, { name: '390-short', w: 390, h: 560, mobile: true }];

const results = [];
const check = (name, ok, info) => { results.push({ name, ok: !!ok, info }); console.log(`${ok ? '✓' : '✗'} ${name}${info ? ' — ' + JSON.stringify(info).slice(0, 260) : ''}`); };

for (const m of MODES) {
  const b = await launch();
  await b.send('Emulation.setDeviceMetricsOverride', { width: m.w, height: m.h, deviceScaleFactor: 1, mobile: !!m.mobile });
  if (m.mobile) await b.send('Emulation.setTouchEmulationEnabled', { enabled: true, maxTouchPoints: 5 });
  await b.send('Page.navigate', { url: `${BASE}/#/home` });
  await b.until("document.documentElement.dataset.ready === '1'", 30000);
  await sleep(1500);
  for (const c of CASES) {
    await b.eval(`document.querySelectorAll('.dialog [data-dlg="cancel"]').forEach((x) => x.click()); __app.closeDrawerParams?.(); document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true })); 0`);
    await sleep(500);
    await b.eval(`${c.open}; 0`);
    await sleep(2500);
    if (c.after) { await b.eval(`${c.after}; 0`); }
    if (c.key) { await b.send('Input.dispatchKeyEvent', { type: 'keyDown', key: 'k', code: 'KeyK', modifiers: 4, windowsVirtualKeyCode: 75 }); await b.send('Input.dispatchKeyEvent', { type: 'keyUp', key: 'k', code: 'KeyK', modifiers: 4 }); }
    const opened = await b.until(`!!document.querySelector(${JSON.stringify(c.box)})`, 8000);
    await sleep(3000); // late content (drawer detail, evidence rows)
    const bg = () => b.eval(`(() => { const o = { win: scrollY }; for (const e of document.querySelectorAll('#app *')) if (e.scrollTop > 0) o[e.className || e.tagName] = e.scrollTop; return JSON.stringify(o); })()`);
    const bg0 = await bg();
    const g = await b.eval(`(() => { const e = document.querySelector(${JSON.stringify(c.box)}); if (!e) return null; const o = ${JSON.stringify(c.over ?? null)} && document.querySelector(${JSON.stringify(c.over ?? '')}); const er = e === document.documentElement ? { x: 0, y: 0, width: innerWidth, height: innerHeight, bottom: innerHeight, top: 0 } : e.getBoundingClientRect(); const r = o ? o.getBoundingClientRect() : er; const y = Math.max(er.y + 10, Math.min(r.y + Math.min(r.height / 2, 120), Math.min(er.bottom, innerHeight) - 10)); return { x: Math.max(er.x + 10, Math.min(r.x + r.width / 2, er.x + er.width - 20, innerWidth - 10)), y, overTable: !!o, sh: e.scrollHeight, ch: e.clientHeight, top: e.scrollTop, inView: e === document.documentElement || (er.bottom <= innerHeight + 1 && er.top >= -1), pageY: scrollY }; })()`);
    if (!opened || !g) { check(`[${m.name}] ${c.name}: opens`, false, { opened }); continue; }
    if (c.box.startsWith('.dialog')) {
      const b0 = await bg();
      for (let i = 0; i < 10; i++) await b.send('Input.dispatchMouseEvent', { type: 'mouseWheel', x: 8, y: m.h - 8, deltaX: 0, deltaY: 300 });
      await sleep(300);
      check(`[${m.name}] ${c.name}: wheel over the backdrop does not move the page behind`, b0 === await bg(), b0);
    }
    const needs = g.sh > g.ch + 4;
    if (!needs) {
      // nothing to scroll: then the last row must be fully visible (a clipped inner box would hide rows with no way to reach them)
      const lv = await b.eval(`(() => { const e = document.querySelector(${JSON.stringify(c.box)}); const l = document.querySelector(${JSON.stringify(c.last)}); if (!l) return false; const lr = l.getBoundingClientRect(); const er = e.getBoundingClientRect(); let p = l.parentElement, clipBottom = Math.min(er.bottom, innerHeight); while (p && p !== e) { const cs = getComputedStyle(p); if (cs.overflowY !== 'visible') clipBottom = Math.min(clipBottom, p.getBoundingClientRect().bottom); p = p.parentElement; } return lr.bottom <= clipBottom + 2; })()`);
      check(`[${m.name}] ${c.name}: fits — container in the viewport and the last row visible`, g.inView && lv, { ...g, lastVisible: lv });
      continue;
    }
    // the page's own forwarding (real trackpads latch a wheel gesture to a horizontally scrollable table; CDP wheel events
    // do not, so dispatch a DOM wheel event on a table cell and check the page hands it to the vertical scroller)
    if (c.over && !c.page) {
      const fw = await b.eval(`(() => { const e = document.querySelector(${JSON.stringify(c.box)}); const cell = document.querySelector(${JSON.stringify(c.over)})?.querySelector('td'); if (!cell) return null; e.scrollTop = 0; const ev = new WheelEvent('wheel', { deltaY: 300, deltaX: 2, bubbles: true, cancelable: true }); cell.dispatchEvent(ev); return { handled: ev.defaultPrevented, top: e.scrollTop, can: e.scrollHeight > e.clientHeight + 4 }; })()`);
      if (fw?.can) check(`[${m.name}] ${c.name}: a wheel on a table cell is forwarded to the dialog / drawer body`, fw.handled && fw.top > 0, fw);
      await b.eval(`document.querySelector(${JSON.stringify(c.box)}).scrollTop = 0; 0`);
    }
    // wheel over the container
    for (let i = 0; i < 400; i++) {
      await b.send('Input.dispatchMouseEvent', { type: 'mouseWheel', x: g.x, y: g.y, deltaX: i % 3 === 0 ? 4 : 0, deltaY: 240 }); // trackpad-like: a little horizontal now and then
      if (i % 20 === 19 && await b.eval(`(() => { const e = document.querySelector(${JSON.stringify(c.box)}); return e.scrollTop >= e.scrollHeight - e.clientHeight - 2; })()`)) break;
    }
    await sleep(600);
    const w = await b.eval(`(() => { const e = document.querySelector(${JSON.stringify(c.box)}); const l = document.querySelector(${JSON.stringify(c.last)}); const lr = l?.getBoundingClientRect(); const er = e.getBoundingClientRect(); return { top: e.scrollTop, max: e.scrollHeight - e.clientHeight, lastVisible: !!lr && lr.bottom <= Math.min(er.bottom, innerHeight) + 2, pageY: scrollY }; })()`);
    const bg1 = await bg();
    const behindOk = c.page ? true : w.pageY === g.pageY && bg0 === bg1;
    check(`[${m.name}] ${c.name}: wheel${g.overTable ? ' over the table' : ''} scrolls to the end${c.page ? '' : ', the page behind stays put'}`, w.top > g.top && w.top >= w.max - 2 && w.lastVisible && behindOk, { before: g, after: w, behind: behindOk ? 'same' : [bg0, bg1] });
    await b.shot(path.join(OUT, `${c.name}-${m.name}.png`));
    // keyboard: back to top, then PageDown / Space / ArrowDown move it
    await b.eval(`document.querySelector(${JSON.stringify(c.box)}).scrollTop = 0; document.querySelector(${JSON.stringify(c.box)}).focus?.(); 0`);
    const k0 = await b.eval(`document.querySelector(${JSON.stringify(c.box)}).scrollTop`);
    if (c.name !== 'palette' && !c.page) {
      for (const key of [['PageDown', 34], ['ArrowDown', 40]]) { await b.send('Input.dispatchKeyEvent', { type: 'keyDown', key: key[0], code: key[0], windowsVirtualKeyCode: key[1] }); await b.send('Input.dispatchKeyEvent', { type: 'keyUp', key: key[0], code: key[0] }); await sleep(250); }
      const k1 = await b.eval(`document.querySelector(${JSON.stringify(c.box)}).scrollTop`);
      check(`[${m.name}] ${c.name}: keyboard (PageDown / ArrowDown) scrolls`, k1 > k0, { k0, k1 });
    }
  }
  await b.close();
}
const bad = results.filter((r) => !r.ok);
console.log(`\nscroll checks: ${results.length - bad.length}/${results.length} passed`);
fs.writeFileSync(path.join(OUT, 'scroll.json'), JSON.stringify(results, null, 2));
process.exit(bad.length ? 1 : 0);
