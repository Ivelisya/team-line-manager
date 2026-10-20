// 在父页面执行：进入首页 iframe，按选择器读取计算样式与 token。
(() => {
  const doc = document.getElementById('f')?.contentDocument || document;
  const win = doc.defaultView;
  const props = ['font-family', 'font-size', 'font-weight', 'line-height', 'letter-spacing', 'font-variant-numeric', 'color',
    'background-color', 'background-image', 'border-top', 'border-bottom', 'border-left', 'border-radius', 'padding', 'margin', 'gap', 'row-gap', 'column-gap',
    'width', 'height', 'min-height', 'box-shadow', 'grid-template-columns', 'grid-template-rows', 'display', 'opacity', 'text-transform', 'white-space', 'text-overflow'];
  const defaults = { 'box-shadow': 'none', 'background-image': 'none', opacity: '1', 'text-transform': 'none', 'grid-template-columns': 'none', 'grid-template-rows': 'none', 'letter-spacing': 'normal', margin: '0px', padding: '0px', gap: 'normal', 'row-gap': 'normal', 'column-gap': 'normal', 'white-space': 'normal', 'text-overflow': 'clip', 'font-variant-numeric': 'normal' };
  const pick = (el) => {
    const cs = win.getComputedStyle(el); const o = {};
    for (const p of props) { const v = cs.getPropertyValue(p); if (v && v !== defaults[p] && !(p.startsWith('border') && /^0px none/.test(v))) o[p] = v; }
    const r = el.getBoundingClientRect(); o._box = `${Math.round(r.width * 10) / 10}x${Math.round(r.height * 10) / 10}`;
    o._text = (el.textContent || '').trim().slice(0, 60);
    return o;
  };
  const sels = window.__SELS || [];
  const out = { selectors: {}, vars: {} };
  for (const s of sels) { const el = doc.querySelector(s); out.selectors[s] = el ? pick(el) : null; }
  const rootCs = win.getComputedStyle(doc.documentElement);
  for (const v of ['--bg', '--card', '--card-2', '--line', '--fg', '--fg-2', '--accent', '--accent-bg', '--ok', '--warn', '--bad', '--info', '--neutral', '--ok-bg', '--warn-bg', '--bad-bg', '--info-bg', '--neutral-bg',
    '--bg-0', '--surface', '--surface-a', '--surface-elev', '--surface-sunken', '--border', '--border-subtle', '--border-strong', '--hairline', '--text-primary', '--text-secondary', '--text-tertiary',
    '--fill-secondary', '--fill-tertiary', '--hover-bg', '--progress-bg', '--progress-cpu', '--progress-memory', '--progress-disk', '--progress-network', '--progress-load', '--status-success', '--status-warning', '--status-error', '--status-info',
    '--accent-500', '--accent-strong', '--traffic-up', '--traffic-down', '--status-online', '--status-offline', '--tag-gray-bg', '--tag-gray-fg', '--ring', '--floating-shadow', '--segment-shadow', '--font-sans', '--font-numeric', '--font-mono', '--canvas-glow', '--latency-excellent', '--latency-critical']) {
    out.vars[v] = rootCs.getPropertyValue(v).trim();
  }
  out.htmlBg = rootCs.backgroundColor; out.htmlBgImage = rootCs.backgroundImage;
  out.bodyFont = win.getComputedStyle(doc.body).fontFamily;
  out.appearance = doc.documentElement.dataset.appearance;
  out.viewport = `${win.innerWidth}x${win.innerHeight}`;
  const grid = doc.querySelector('.node-card-grid');
  if (grid) { const cs = win.getComputedStyle(grid); out.grid = { cols: cs.gridTemplateColumns, gap: cs.gap, style: grid.getAttribute('style') }; }
  const cards = [...doc.querySelectorAll('.node-card-grid > div')].slice(0, 45).map((d) => { const r = d.getBoundingClientRect(); const c = d.querySelector('article'); return { cls: c?.className, h: Math.round(r.height), w: Math.round(r.width), name: (d.querySelector('.server-card-title-link,.compact-node-title,.mini-node-title')?.textContent || '').trim() }; });
  out.cards = cards;
  return out;
})()
