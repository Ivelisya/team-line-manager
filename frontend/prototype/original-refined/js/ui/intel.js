// intel.js — 来源画像 SourceIntel：来源 IP 的运营商、城市、接入方式与标记（数据来自 snapshot.ipintel，离线库富化）。
// 同一种写法用在：节点行为“来源地址”、行为变化“新来源”的对象、证据对话框的来源列、抽屉访问记录。
//   第一行：IP（可复制）+ 标签：`新来源`、`机房`、`线路机自身` 用标签 chip，`境外` 用 warn 语气 pill（.mark 只留给 IPv4 / IPv6 / LA / 原名，consistency #7）
//   次行：  `中国联通 · 郑州 · 固定宽带`（12 --fg-2；接入方式置信度低时退到 --fg-3）
//   悬停次行：完整画像（省市、ASN 与组织、接入方式与置信度、判断依据、库间分歧）
// 只用已有组件（copyable、mark）和 token，不引入新颜色：运营商不按品牌着色。

import { esc } from './dom.js';
import { copyable, tagChip, tonePill } from './controls.js';

const CONN_SHORT = { mobile: '移动网络', broadband: '固定宽带', datacenter: '云机房', hosting: 'IDC', education: '教育科研网', private: '内网地址', unknown: '未知' };
const CONN_FULL = { mobile: '移动网络（蜂窝）', broadband: '固定宽带', datacenter: '云机房', hosting: 'IDC / 主机托管', education: '教育科研网', private: '内网 / 保留地址', unknown: '未知' };
const CONF = { high: '高', medium: '中', low: '低' };
const CONFLICT = { geo_conflict: '库间位置不一致', db_conflict: '库间归属不一致', asn_conflict: '库间 ASN 不一致' };

const cache = new WeakMap();
/** 按 IP 查 snapshot.ipintel.sources 里的完整画像（没有时返回 null）。行级字段（carrier/city…）也能直接传给下面的函数。 */
export function intelOf(store, ip) {
  if (!ip || !store) return null;
  const raw = store.raw ?? store;
  const list = raw.ipintel?.sources ?? store.ipintel?.()?.sources ?? null;
  if (!list) return null;
  let map = cache.get(list);
  if (!map) { map = new Map(list.map((s) => [s.ip, s])); cache.set(list, map); }
  return map.get(ip) ?? null;
}

const trimPlace = (s) => (s ? String(s).replace(/(市|地区|自治州|盟)$/, '') : '');
/** 地点短写：中国地址写城市（去“市”），省级市写一次；境外写国家（有城市时加城市）。 */
function placeOf(x) {
  if (!x) return '';
  if (x.country && x.country !== 'CN') return [x.country_name ?? x.country, x.city && x.city !== x.country_name ? x.city : ''].filter(Boolean).join(' ');
  return trimPlace(x.city) || trimPlace(x.province) || '';
}

/** 次行文字：`中国联通 · 郑州 · 固定宽带`；没有画像时返回 ''。 */
export function sourceLine(x) {
  if (!x) return '';
  return [x.carrier, placeOf(x), CONN_SHORT[x.conn_type] ?? null].filter(Boolean).join(' · ');
}
/** 悬停说明（多行）：完整画像。 */
export function sourceTip(x, { ip } = {}) {
  if (!x) return '';
  const place = x.country && x.country !== 'CN'
    ? [x.country_name ?? x.country, x.province, x.city].filter(Boolean).join(' ')
    : [x.province, x.city && x.city !== x.province ? x.city : ''].filter(Boolean).join(' ');
  const asn = x.asn ? `AS${x.asn}${x.as_org ? ` ${x.as_org}` : ''}` : '';
  const conflicts = (x.flags ?? []).map((f) => CONFLICT[f]).filter(Boolean);
  return [
    ip ? `来源 ${ip}` : '',
    `运营商　${x.carrier ?? '—'}${asn ? `（${asn}）` : ''}`,
    `位置　　${place || '—'}`,
    `接入方式　${CONN_FULL[x.conn_type] ?? '—'}${x.conn_conf ? ` · 置信度${CONF[x.conn_conf] ?? x.conn_conf}` : ''}`,
    conflicts.length ? `提示　　${conflicts.join('、')}，以 ip2region 为准` : '',
    '离线库查询（ip2region / DB-IP / iptoasn），接入方式为启发式判断',
  ].filter(Boolean).join('\n');
}
/** 次行 HTML（带悬停完整画像；低置信度的接入方式退一级颜色）。 */
export function sourceLineHtml(x, { ip, prefix = '' } = {}) {
  if (!x) return prefix ? esc(prefix) : '';
  const parts = [x.carrier, placeOf(x)].filter(Boolean).map(esc);
  const conn = CONN_SHORT[x.conn_type];
  if (conn) parts.push(x.conn_conf === 'low' ? `<span class="src-conf-low">${esc(conn)}</span>` : esc(conn));
  const head = prefix ? `${esc(prefix)} · ` : '';
  return `<span class="src-line" data-tip="${esc(sourceTip(x, { ip }))}">${head}${parts.join(' · ')}</span>`;
}

/**
 * 标记（第一行 IP 后面）：新来源（24 小时内首次出现）、境外（warn）、机房、线路机自身。
 * relayName(id) → 线路机名，用来写“线路机1 自身地址”的说明。
 */
export function sourceBadges(x, { isNew, relayName } = {}) {
  if (!x) return '';
  const flags = new Set(x.flags ?? []);
  const out = [];
  if (isNew ?? x.new_in_h24) out.push(tagChip('新来源', '24 小时内第一次出现的来源 IP'));
  if (flags.has('foreign') || (x.country && x.country !== 'CN' && !flags.has('internal'))) out.push(tonePill('境外', 'warn', { dot: 'none', title: `境外来源：${x.country_name ?? x.country}` }));
  if (flags.has('internal')) out.push(tagChip('线路机自身', `${relayName?.(x.internal) ?? '线路机'} 自己的地址；连接很少，通常是链路探针`));
  else if (flags.has('datacenter') || x.conn_type === 'hosting' || x.conn_type === 'datacenter') out.push(tagChip('机房', '来源 ASN 属于云厂商或 IDC'));
  return out.join('');
}

/** 第一行：可复制的 IP + 标记。 */
export function sourceCell(ip, x, opts = {}) {
  return `<span class="src">${copyable(ip)}${sourceBadges(x, opts)}</span>`;
}
