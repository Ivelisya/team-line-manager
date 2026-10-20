// table.js — DataTable 的渲染助手（SPEC §3.10）+ 表内小部件（进度线、步骤单元格 StepCell §6.6）。
// 状态（排序、筛选、分组、光标、URL）用 shared/table.js 的 createTable；这里只负责把状态画成统一外观的 HTML。

import { fmt } from '../../../shared/index.js';
import { icon } from '../icons.js';
import { esc, attrs } from './dom.js';
import { empty } from './layout.js';

/**
 * dataTable({ columns, rows, getId, sortOf, current, two, rowLink, empty, groups, label })
 *   columns: [{ key, label, align:'left'|'right'|'center', sortable, width, cell(row) → html, sub(row) → html, cls }]
 *   sortOf(key) → { dir:'asc'|'desc'|null, ariaSort }（直接传 table.sortOf）
 *   rowLink: true → 整行可点（tr.is-link、tabindex、行尾 ›）；点击事件由页面委托 [data-row]
 *   two: true → 双行行高（主值 + 次行，--row-2）
 *   groups: [{ key, word, count, meta, rows, collapsed }]（table.groups() 的形状）→ 插入分组行
 *   cls: 追加到 <table> 的类，例如 'is-sticky-first'（窄屏首列吊在左侧）；columns[].sorts: [{ key, label }] 一列两个排序键；groups[].end: 分组行右侧小计
 *   sections: [{ title, count, rows }] → 同一张表里分小节（无 title 的小节不画标题行），列宽上下对齐；patchTable 传各小节 rows 拼起来的数组
 *   head: false → 不画表头带（原版总览“实时传输排行”“需要关注”那种列表式小表；列含义由面板标题和 ↓ ↑ 箭头说明）
 *   columns[].head: 表头 HTML（覆盖 label，例如 thHead('预检', '7/8') 两行表头）；columns[].thCls: 只加在 <th> 上的类
 *   rowAttrs(row) → 追加到 <tr> 的属性（class 合并），例如 { 'data-attn': 'bad' }（首列左侧 2 px 状态色）
 *   after(row) → HTML：紧跟该行、跨全部列的常驻说明行（tr.note-tr > td.note-td，--sunken 底；上架流水线的需处理行）
 *   expand: { isOpen(row) → bool, render(row) → HTML } → 可展开行（§3.10）：行上 aria-expanded，行尾 › 展开时转 90°；
 *     展开内容是紧跟的 tr.expand-tr（--sunken 底、内边距 16）。点击由页面委托 [data-row] 处理（线路页入口表）
 * rowIcon / rowTip：行尾图标与提示。默认 ›（打开抽屉）；点行打开的是对话框时换成 ScanSearch + “查看证据”，两种结果一眼分得开（consistency #23）。
 * 返回 `<div class="table-wrap"><table class="dt">…</table></div>`，表格放进面板时用 panel({ flush:true })。
 */
export function dataTable({ columns, rows = [], getId = (r) => r.id, sortOf, current = null, two = true, rowLink = true, empty: emptyHtml, groups = null, sections = null, label = '', head: showHead = true, cls = '', rowAttrs = null, after = null, expand = null, rowIcon = 'chevron-right', rowTip = '' }) {
  const head = columns.map((c) => {
    const thCls = [c.align === 'right' ? 'r' : c.align === 'center' ? 'c' : '', c.cls].filter(Boolean).join(' ');
    const al = thCls ? ` class="${thCls}"` : '';
    const w = c.width ? ` style="width:${typeof c.width === 'number' ? c.width + 'px' : c.width}"` : '';
    if (c.sorts) {
      // 一列两个排序键（节点页“月费 / 到期”“延迟 TCP / 出口”）：两个表头按钮，中间 “/”
      const st = c.sorts.map((k) => ({ ...k, s: sortOf?.(k.key) ?? {} }));
      const on_ = st.find((k) => k.s.ariaSort && k.s.ariaSort !== 'none' && k.s.ariaSort !== 'other');
      const btn = (k) => {
        const arrow = k.s.dir === 'asc' ? icon('arrow-up', 12) : k.s.dir === 'desc' ? icon('arrow-down', 12) : '';
        return `<button type="button" class="th-sort" data-sort="${esc(k.key)}" aria-sort="${k.s.ariaSort ?? 'none'}">${c.align === 'right' ? arrow : ''}${esc(k.label)}${c.align === 'right' ? '' : arrow}</button>`;
      };
      return `<th scope="col"${al}${w} aria-sort="${on_?.s.ariaSort ?? 'none'}"><span class="th-multi">${st.map(btn).join('<span class="th-sep" aria-hidden="true">/</span>')}</span></th>`;
    }
    if (c.head != null || c.thCls) {
      const thc = [al.replace(/^ class="|"$/g, ''), c.thCls].filter(Boolean).join(' ');
      if (c.head != null || !c.sortable) return `<th scope="col"${thc ? ` class="${thc}"` : ''}${w}>${c.head ?? esc(c.label)}</th>`;
    }
    if (!c.sortable) return `<th scope="col"${al}${w}>${esc(c.label)}</th>`;
    const s = sortOf?.(c.key) ?? {};
    const arrow = s.dir === 'asc' ? icon('arrow-up', 12) : s.dir === 'desc' ? icon('arrow-down', 12) : '';
    return `<th scope="col"${al}${w} aria-sort="${s.ariaSort ?? 'none'}"><button type="button" class="th-sort" data-sort="${esc(c.key)}" aria-sort="${s.ariaSort ?? 'none'}">${c.align === 'right' ? arrow : ''}${esc(c.label)}${c.align === 'right' ? '' : arrow}</button></th>`;
  }).join('') + (rowLink ? '<th class="chev" aria-hidden="true"></th>' : '');
  const colspan = columns.length + (rowLink ? 1 : 0);
  const rowHtml = (r) => {
    const id = getId(r);
    const cells = columns.map((c) => {
      const al = c.align === 'right' ? 'r' : c.align === 'center' ? 'c' : '';
      const cls = [al, c.cls].filter(Boolean).join(' ');
      return `<td${cls ? ` class="${cls}"` : ''}>${cellHtml(c, r)}</td>`;
    }).join('');
    const extra = rowAttrs?.(r) ?? {};
    const note = after?.(r) ?? '';
    const open = expand ? !!expand.isOpen(r) : null;
    const trCls = [rowLink && 'is-link', rowLink && id === current && 'is-current', note && 'has-note', open && 'is-open', extra.class].filter(Boolean).join(' ');
    const link = `${trCls ? ` class="${trCls}"` : ''}${rowLink ? ' tabindex="0"' : ''}${open != null ? ` aria-expanded="${open}"` : ''}${attrs({ ...extra, class: null })}`;
    const noteRow = note ? `<tr class="note-tr" data-after="${esc(id)}"><td class="note-td" colspan="${colspan}">${note}</td></tr>` : '';
    const expandRow = open ? `<tr class="expand-tr" data-expand="${esc(id)}"><td colspan="${colspan}"><div class="expand-body">${expand.render(r)}</div></td></tr>` : '';
    return `<tr data-row="${esc(id)}"${link}>${cells}${rowLink ? `<td class="chev"${rowTip ? ` data-tip="${esc(rowTip)}"` : ''}>${icon(rowIcon, 14)}</td>` : ''}</tr>${noteRow}${expandRow}`;
  };
  let body = '';
  if (sections) body = sections.filter((x) => x.rows.length).map((x) => `${x.title ? `<tr class="section-tr"><td colspan="${colspan}">${esc(x.title)}${x.count != null ? ` <span class="num">${esc(fmt.num(x.count))}</span>` : ''}</td></tr>` : ''}${x.rows.map(rowHtml).join('')}`).join('');
  else if (!rows.length && !groups?.length) body = `<tr class="empty-tr"><td colspan="${colspan}">${emptyHtml ?? empty({ text: '没有数据' })}</td></tr>`;
  else if (groups) {
    body = groups.map((g) => `<tr class="group-tr" data-group="${esc(g.key)}"><td colspan="${colspan}"><button type="button" class="disclosure" aria-expanded="${!g.collapsed}" data-group-toggle="${esc(g.key)}">${icon('chevron-right', 14)}<span>${esc(g.word ?? g.key)}</span><span class="disclosure-meta">${esc(`${g.count ?? g.rows.length} 台`)}${g.meta ? ` · ${g.meta}` : ''}</span>${g.end ? `<span class="group-end" data-group-end>${g.end}</span>` : ''}</button></td></tr>${g.collapsed ? '' : g.rows.map(rowHtml).join('')}`).join('');
  } else body = rows.map(rowHtml).join('');
  return `<div class="table-wrap"><table class="dt${two ? ' is-two' : ''}${showHead ? '' : ' is-headless'}${cls ? ` ${esc(cls)}` : ''}"${label ? ` aria-label="${esc(label)}"` : ''}>${showHead ? `<thead><tr>${head}</tr></thead>` : ''}<tbody>${body}</tbody></table></div>`;
}

/** 单元格内容：主值 + 可选次行（dataTable 与 patchTable 共用，保证补丁写出的 HTML 与首绘一致）。 */
function cellHtml(c, r) {
  const sub = c.sub ? c.sub(r) : '';
  return `${c.cell ? c.cell(r) : esc(r[c.key] ?? '—')}${sub ? `<span class="sub">${sub}</span>` : ''}`;
}

/**
 * patchTable(root, { columns, rows, getId }) — 实时更新：行的集合与顺序没变时，只改写内容变化的单元格（§5.3 规则 1），返回 true；
 * 行集合或顺序变了返回 false，由调用方决定何时整表重画（按速率排序时节流到 5 秒一次、鼠标或焦点在表里时暂停）。
 * 比较的是上次写入的 HTML 字符串（缓存在元素上），不读 innerHTML，避免浏览器规范化造成的误判。
 */
export function patchTable(root, { columns, rows, getId = (r) => r.id }) {
  const trs = root ? [...root.querySelectorAll('tbody > tr[data-row]')] : [];
  if (trs.length !== rows.length || rows.some((r, i) => trs[i].dataset.row !== String(getId(r)))) return false;
  rows.forEach((r, i) => {
    const tds = trs[i].children;
    columns.forEach((c, j) => {
      const td = tds[j];
      if (!td) return;
      const html = cellHtml(c, r);
      if (td.__html === undefined) td.__html = td.innerHTML === html ? html : null;
      if (td.__html !== html) { td.innerHTML = html; td.__html = html; }
    });
  });
  return true;
}

/** 表格骨架：行高与真实行相同（§3.20）。 */
export function tableSkeleton(cols = 6, rows = 6, { two = true, head = true } = {}) {
  // 双行表的骨架格也是“20 行 + 18 行”两段，与真实单元格同高（含底线 57），数据到达时不跳（craft #6）
  const cell = two
    ? '<td><span class="skel" style="width:70%;height:12px;margin:4px 0 8px"></span><span class="skel" style="width:45%;height:10px;margin:0 0 4px"></span></td>'
    : '<td><span class="skel" style="width:70%;height:12px"></span></td>';
  return `<div class="table-wrap"><table class="dt${two ? ' is-two' : ''}${head ? '' : ' is-headless'}">${head ? `<thead><tr>${'<th>&nbsp;</th>'.repeat(cols)}</tr></thead>` : ''}<tbody>${`<tr>${cell.repeat(cols)}</tr>`.repeat(rows)}</tbody></table></div>`;
}

/**
 * 表内进度线：两条上下叠放（CPU / 内存），默认宽 120（§3.13）。items: [{ label, value(Fmt 或 { text }), pct, tone, tip }]
 * opts.width：列宽不同时改宽度（节点页主机列 168，放得下 `165 MiB / 2.57 GiB`）。
 */
export function tableMeter(items, { width } = {}) {
  return `<div class="tmeter"${width ? ` style="--tmeter-w:${Number(width)}px"` : ''}>${items.map((m) => `<div${m.tip ? ` data-tip="${esc(m.tip)}"` : ''}><div class="tmeter-row"><span>${esc(m.label)}</span><b>${esc(m.value?.text ?? '—')}</b></div><div class="meter-track"${m.tone ? ` data-tone="${m.tone}"` : ''}><span class="meter-fill" style="width:${Math.max(0, Math.min(100, m.pct ?? 0)).toFixed(1)}%"></span></div></div>`).join('')}</div>`;
}

/**
 * 步骤单元格 StepCell（§6.6）：高 28、圆角 8；状态图标 14 + 文字 12。
 * status：succeeded | running | waiting_input | waiting_gate | failed | blocked | rolled_back | queued | skipped | cancelled
 */
const STEP_ICON = {
  succeeded: 'circle-check', running: 'loader-circle', waiting_input: 'circle-pause', waiting_gate: 'circle-pause',
  failed: 'circle-x', blocked: 'shield-alert', rolled_back: 'undo-2', queued: 'circle', skipped: 'minus', cancelled: 'circle-x',
};
export function stepCell(status, { text, tip, attrs: a = {}, fill = false, word } = {}) {
  const st = fmt.step(status);
  const t = text ?? (status === 'queued' || status === 'skipped' ? '' : st.word);
  const w = word ?? st.word;
  return `<button type="button" class="step${fill ? ' is-fill' : ''}" data-step="${esc(status)}"${tip ? ` data-tip="${esc(tip)}"` : ''} aria-label="${esc(`${w}${t && t !== w ? ' · ' + t : ''}`)}"${attrs(a)}>${stepIcon(status)}${t ? `<span class="num">${esc(t)}</span>` : ''}</button>`;
}
/** 步骤状态图标（StepCell、时间线、图例共用同一套）：stepIcon('failed', 14) */
export function stepIcon(status, size = 14) { return icon(STEP_ICON[status] ?? 'circle', size, status === 'running' ? 'spin' : ''); }
/** 步骤图例：成功 · 进行中 · 等待输入 · 失败 · 已停止 · 跳过 · 排队（放在矩阵面板头的副行） */
export function stepLegend(list = ['succeeded', 'running', 'waiting_input', 'failed', 'blocked', 'skipped', 'queued'], words = {}) {
  // 图例里的“进行中”图标不旋转（静态说明，不抢注意力）
  return `<span class="step-legend">${list.map((s) => `<span data-step="${esc(s)}">${icon(STEP_ICON[s] ?? 'circle', 14)}${esc(words[s] ?? fmt.step(s).word.replace('（风险）', ''))}</span>`).join('')}</span>`;
}
/** 两行表头：thHead('预检', '7/8') → 阶段名 12/500 + 汇总（Inter 12 --fg-2），给 columns[].head 用 */
export function thHead(label, sub = '') { return `<span class="th-2l"><span>${esc(label)}</span>${sub ? `<span class="th-2l-sub num">${esc(sub)}</span>` : ''}</span>`; }
