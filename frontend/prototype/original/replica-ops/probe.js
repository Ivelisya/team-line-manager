// 页面内执行：读取计算样式。返回 {tokens, els}。
// 用法：(PROBE)(docKind)  docKind = 'ops'（运营 iframe）| 'native'（原生外壳顶层文档）
(kind) => {
  const top = document;
  const frame = top.querySelector('iframe[title="节点管理"]');
  const doc = kind === 'ops' ? frame.contentDocument : top;
  const win = doc.defaultView;
  const props = ['font-family', 'font-size', 'font-weight', 'line-height', 'letter-spacing', 'font-variant-numeric', 'color', 'background-color', 'border-top', 'border-bottom', 'border-left', 'border-right', 'border-radius', 'padding', 'margin', 'gap', 'height', 'min-height', 'width', 'box-shadow', 'text-align', 'backdrop-filter', 'opacity', 'text-transform', 'white-space', 'grid-template-columns'];
  const skip = { 'box-shadow': 'none', 'backdrop-filter': 'none', opacity: '1', 'text-transform': 'none', 'letter-spacing': 'normal', margin: '0px', padding: '0px', gap: 'normal', 'font-variant-numeric': 'normal', 'white-space': 'normal', 'grid-template-columns': 'none', 'min-height': '0px', 'min-height ': 'auto', 'text-align': 'start' };
  const visible = el => { const r = el.getBoundingClientRect(); const cs = win.getComputedStyle(el); return r.width > 0 && r.height > 0 && cs.visibility !== 'hidden' && cs.display !== 'none'; };
  const pick = el => {
    const cs = win.getComputedStyle(el), o = {};
    for (const p of props) { const v = cs.getPropertyValue(p); if (v === '' || v === skip[p] || (p === 'min-height' && v === 'auto')) continue; if (p.startsWith('border-') && p !== 'border-radius' && /^0px /.test(v)) continue; if (p === 'border-radius' && v === '0px') continue; o[p] = v; }
    const r = el.getBoundingClientRect(); o._box = `${Math.round(r.width * 10) / 10}x${Math.round(r.height * 10) / 10}`;
    o._text = (el.innerText || el.textContent || '').trim().replace(/\s+/g, ' ').slice(0, 50);
    return o;
  };
  const OPS = {
    body: 'body', workspace: '.workspace', h1: '.page-heading h1', pageMeta: '#page-meta', pageTools: '.page-tools',
    densitySeg: '.page-tools .segment', densityBtn: '.page-tools .segment button:not(.active)', densityBtnActive: '.page-tools .segment button.active',
    btn: '.btn:not(.primary):not(.danger):not(.quiet):not(.small)', btnPrimary: '.btn.primary', btnSmall: '.btn.small:not(.quiet):not(.danger):not(.primary)', btnQuiet: '.btn.quiet', btnDanger: '.btn.danger', iconBtn: '.icon-button', iconSvg: '.icon-button svg',
    kpiGrid: '.kpi-grid', kpi: '#main .kpi-grid .kpi', kpiLabel: '#main .kpi-label', kpiValue: '#main .kpi-value', kpiValueSmall: '#main .kpi-value small', kpiValueBad: '#main .kpi-value.bad', kpiValueWarn: '#main .kpi-value.warn', kpiSub: '#main .kpi-sub',
    panel: '#main section.panel', panelHead: '#main .panel-head', panelH2: '#main .panel-head h2', panelH3: '.panel-head h3', panelBody: '#main .panel-body', nestedPanel: '.panel .panel',
    tableWrap: '#main .table-wrap', table: '#main table', th: '#main thead th', thNum: '#main thead th.td-num', td: '#main tbody td', tdNum: '#main tbody td.td-num', rowLinkTd: '#main tr.row-link td',
    identityStrong: '.identity strong', identityMono: '.identity .mono', tdSmall: 'td small', monoCell: 'td.mono',
    chipOk: '.chip-ok', chipWarn: '.chip-warn', chipBad: '.chip-bad', chipNeutral: '.chip-neutral', chipOutline: '.chip-outline', chipInfo: '.chip-info', chipDot: '.chip-ok .dot',
    buttonChip: 'button.chip:not(.active)', buttonChipActive: 'button.chip.active', tabsInline: '.tabs-inline',
    viewSeg: '.nodes-views', viewSegBtn: '.nodes-views button:not(.active)', viewSegBtnActive: '.nodes-views button.active', viewSegCount: '.nodes-views button small',
    input: '.input', select: 'select', textarea: 'textarea', filters: '.filters', searchCount: '.search-count',
    meter: '.meter', meterRow: '.meter-row', meterBar: '.meter-bar', meterFill: '.meter-bar i', availPct: '.avail-pct',
    chart: '.chart', chartSvgUp: '.chart polyline.up', chartSvgDown: '.chart polyline.down', chartGrid: '.chart line.grid', chartLabels: '.chart-labels', chartLegend: '.chart-legend', legendDot: '.legend-dot', legendDotCyan: '.legend-dot.cyan',
    inlineHelp: '.inline-help', chev: '.chev', gridEqual: '.grid-equal',
    relayCard: '.relay-card', relayCardH2: '.relay-card>.panel-head h2', lineCapacity: '.line-capacity', lineCapacityStrong: '.line-capacity strong', relayDesc: '.relay-description', relayMetrics: '.relay-metrics', relayFooter: '.relay-footer',
    badgeRow: '.badge-row', panelBadges: '.panel-badges', inboundFilters: '.inbound-filters', inboundFilterLabel: '.inbound-filters label', inboundTable: '.inbound-table', inboundTh: '.inbound-table th', inboundTd: '.inbound-table tbody td', inboundCell: '.inbound-table td>.cell', rowDetail: '.row-detail>td', pager: '.pager',
    techDetails: '.technical-details', techSummary: '.technical-details>summary', detailsGrid: '.details-grid', detailLine: '.detail-line', detailLabel: '.detail-line>span:first-child',
    jobsPanel: '#jobs-panel', jobRow: '.job-row', jobHead: '.job-head', jobResult: '.job-result', stepDot: '.step-dot',
    audit: '#audit', auditSummary: '.audit-record summary', codeBlock: '.code-block',
    behaviorTabs: '.behavior-page-tabs', filterPanel: '.filter-panel', filterMore: '.filter-more>summary',
    sectionTabs: '.section-tabs', sectionTab: '.section-tabs button:not(.active)', sectionTabActive: '.section-tabs button.active', unreadDot: '.unread-dot',
    behaviorCard: '.behavior-cards .card', changeTitle: '.change-title h3', pagination: '.pagination',
    selectionContext: '.selection-context', rulesLayout: '.rules-layout', scopeList: '.scope-list', scopeButton: '.scope-button:not(.active)', scopeButtonActive: '.scope-button.active', scopeHeading: '.scope-heading',
    field: '.field', fieldLabel: '.field label', fieldRow: '.field-row',
    domainList: '.domain-list li', domainRank: '.domain-rank', evidenceMeta: '.evidence-meta',
    drawer: '#drawer', drawerBackdrop: '#drawer-backdrop', drawerHead: '.drawer-head', drawerH2: '.drawer-head h2', drawerBody: '.drawer-body', drawerTabs: '.drawer-tabs', drawerTab: '.drawer-tabs button:not(.active)', drawerTabActive: '.drawer-tabs button.active',
    card: '.drawer .card', cardHead: '.card-head', cardH3: '.card-head h3', latency: '.latency', latencyB: '.latency b', latencyCell: '.latency-cells i', latencyCellOk: '.latency-cells i.ok', availCell: '.avail-cells i', availCellOk: '.avail-cells i.ok', drawerKpi: '.drawer .kpi', drawerKpiValue: '.drawer .kpi-value', drawerKpiLabel: '.drawer .kpi-label', quota: '.quota',
    modalBackdrop: '#modal', modal: '.modal', modalHead: '.modal-head', modalH2: '.modal-head h2', modalP: '.modal-head p', modalContent: '.modal-content', modalFoot: '.modal-foot',
    note: '.note', noteWarning: '.note.warning', noteError: '.note.error', divider: '.divider', checkboxLine: '.checkbox-line', checkbox: '.checkbox-line input',
    wizardSteps: '.wizard>ol.steps', wizardStep: '.wizard>ol.steps>li:not(.active)', wizardStepActive: '.wizard>ol.steps>li.active', radioGroup: '.radio-group', radioLabel: '.radio-group label', logBox: '.log-box',
    toast: '#toast', toastError: '#toast.error', empty: '.empty', skeleton: '.skeleton i', menuList: '.menu-list', schemaTable: '.schema-table td',
  };
  const NATIVE = {
    html: 'html', body: 'body', layout: '.km-admin-layout', topbar: '.km-admin-panel-topbar', topbarInner: '.km-admin-panel-topbar > .rt-Flex', brand: '.km-admin-panel-topbar label.text-xl', version: '.km-admin-panel-topbar label.text-sm',
    menuToggle: '.km-admin-panel-topbar .rt-IconButton', controls: '.km-admin-panel-controls', controlBtn: '.km-admin-panel-controls .rt-IconButton:not(.km-admin-panel-account)', logout: '.km-admin-panel-account',
    sidebar: '.km-admin-panel-nav', sidebarInner: '.km-admin-panel-nav > .rt-Flex', navLink: '.km-admin-panel-nav a', navRow: '.km-admin-panel-nav a > .rt-Flex', navText: '.km-admin-panel-nav a .text-base', navIcon: '.km-admin-panel-nav a svg', navGroup: '.km-admin-panel-nav [role=button]', navGroupText: '.km-admin-panel-nav [role=button] .text-base', navChildren: '.km-admin-panel-nav .ml-4',
    content: '.km-admin-panel-content', contentInner: '.km-admin-panel-content > div', frame: '.km-ops-native-frame',
    h1: '.km-admin-panel-content h1', text2xl: '.km-admin-panel-content .text-2xl', textXl: '.km-admin-panel-content .text-xl', muted: '.km-admin-panel-content .text-muted-foreground',
    card: '.km-admin-panel-content .rt-Card', settingCard: '.km-setting-card', settingTitle: '.km-setting-card .text-base.font-medium',
    tableRoot: '.km-admin-panel-content .rt-TableRoot', table: '.km-admin-panel-content table', th: '.km-admin-panel-content th', td: '.km-admin-panel-content td',
    btnSolid: '.km-admin-panel-content .rt-Button.rt-variant-solid', btnSoft: '.km-admin-panel-content .rt-Button.rt-variant-soft', btnOutline: '.km-admin-panel-content .rt-Button.rt-variant-outline', btnGhost: '.km-admin-panel-content .rt-Button.rt-variant-ghost', btnSurface: '.km-admin-panel-content .rt-Button.rt-variant-surface',
    iconBtn: '.km-admin-panel-content .rt-IconButton', textField: '.km-admin-panel-content .rt-TextFieldRoot', textInput: '.km-admin-panel-content .rt-TextFieldInput', selectTrigger: '.km-admin-panel-content .rt-SelectTrigger', switch: '.km-admin-panel-content .rt-SwitchRoot', checkbox: '.km-admin-panel-content .rt-CheckboxRoot',
    badge: '.km-admin-panel-content .rt-Badge', tabsList: '.km-admin-panel-content .rt-TabsList', tabsTrigger: '.km-admin-panel-content .rt-TabsTrigger', segmented: '.km-admin-panel-content .rt-SegmentedControlRoot', callout: '.rt-CalloutRoot',
    dialog: '.rt-DialogContent', dialogTitle: '.rt-DialogTitle', dialogDesc: '.rt-DialogDescription', dialogInput: '.rt-DialogContent .rt-TextFieldRoot', dialogBtn: '.rt-DialogContent .rt-Button',
    privateEntry: '.km-private-entry', privateBrand: '.km-private-brand', dashCard: '.km-dashboard-card', helpTrigger: '.native-help-trigger', empty: '.native-empty',
  };
  const map = kind === 'ops' ? OPS : NATIVE;
  const els = {};
  for (const [name, sel] of Object.entries(map)) {
    let list; try { list = [...doc.querySelectorAll(sel)]; } catch { continue; }
    const el = list.find(visible);
    if (el) els[name] = { sel, n: list.length, ...pick(el) };
  }
  if (kind === 'native') {
    const act = [...doc.querySelectorAll('.km-admin-panel-nav a > .rt-Flex')].find(e => /accent-4/.test(e.getAttribute('style') || ''));
    if (act) { els.navRowActive = { sel: 'a>.rt-Flex[style*=accent-4]', ...pick(act) }; const t = act.querySelector('.text-base'); if (t) els.navTextActive = { sel: 'active .text-base', ...pick(t) }; const i = act.querySelector('svg'); if (i) els.navIconActive = { sel: 'active svg', ...pick(i) }; }
  }
  const rootCs = win.getComputedStyle(doc.documentElement);
  const bodyCs = win.getComputedStyle(doc.body);
  const tokenNames = kind === 'ops'
    ? ['--bg', '--card', '--card-2', '--line', '--fg', '--fg-2', '--accent', '--accent-solid', '--accent-hover', '--accent-bg', '--on-accent', '--ok', '--warn', '--bad', '--info', '--ok-bg', '--warn-bg', '--bad-bg', '--info-bg', '--backdrop', '--radius-card', '--radius-block', '--font-mono', '--card-padding', '--row-height', '--cell-padding', '--control-height']
    : ['--native-page', '--native-panel', '--native-panel-soft', '--native-text', '--native-muted', '--native-border', '--native-accent', '--native-accent-text', '--color-page-background', '--color-panel', '--color-surface', '--default-font-family', '--radius-1', '--radius-2', '--radius-3', '--radius-4', '--radius-5', '--radius-6', '--font-size-1', '--font-size-2', '--font-size-3', '--font-size-4', '--font-size-5', '--font-size-6', '--space-1', '--space-2', '--space-3', '--space-4', '--space-5',
      '--accent-1', '--accent-2', '--accent-3', '--accent-4', '--accent-8', '--accent-9', '--accent-10', '--accent-11', '--accent-12', '--accent-a3', '--accent-a4', '--accent-contrast', '--gray-1', '--gray-2', '--gray-3', '--gray-4', '--gray-6', '--gray-a4', '--gray-a6', '--gray-11', '--gray-12', '--orange-9', '--orange-11', '--orange-a3'];
  const tokens = {};
  const themeEl = kind === 'native' ? (doc.querySelector('.radix-themes') || doc.documentElement) : doc.body;
  const themeCs = win.getComputedStyle(themeEl);
  for (const t of tokenNames) { const v = (themeCs.getPropertyValue(t) || rootCs.getPropertyValue(t) || bodyCs.getPropertyValue(t)).trim(); if (v) tokens[t] = v; }
  return { kind, dark: doc.documentElement.classList.contains('dark'), density: doc.body?.dataset?.density, url: top.location.pathname + top.location.search, viewport: `${top.defaultView.innerWidth}x${top.defaultView.innerHeight}`, scrollHeight: doc.scrollingElement ? doc.scrollingElement.scrollHeight : null, tokens, els };
}
