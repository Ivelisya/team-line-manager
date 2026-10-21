// 上架表单的数据模型：行 → ProvisionItem、字段适用性、从表格软件粘贴的 TSV 解析、本地校验。
import type { Kind, Plan, Provider, ProvisionItem, Team } from '@/lib/types';

export interface Row {
  /** 稳定的行 id（React key、单元格回调用），不进入提交内容 */
  id: string;
  kind: Kind; ip: string; ssh_port: string; team_id: string; number: string; label: string; relay_id: string;
  provider_id: string; plan_id: string; service_ref: string; note: string;
  login_user: string; login_pw: string;
  up_protocol: string; up_host: string; up_port: string; up_user: string; up_pw: string;
  ss_port: string; due_date: string; price: string;
  /** 粘贴时没匹配上的文本：字段 → 原文，用于本地报错 */
  unresolved: Record<string, string>;
}
export type Field = Exclude<keyof Row, 'id' | 'unresolved' | 'kind'>;

let rowSeq = 0;
export const newRow = (p?: Partial<Row>): Row => ({
  id: `r${++rowSeq}`, kind: 'vps', ip: '', ssh_port: '', team_id: '', number: '', label: '', relay_id: '', provider_id: '', plan_id: '', service_ref: '', note: '',
  login_user: '', login_pw: '', up_protocol: 'socks5', up_host: '', up_port: '', up_user: '', up_pw: '', ss_port: '', due_date: '', price: '', unresolved: {}, ...p,
});

export type ColType = 'text' | 'num' | 'date' | 'kind' | 'team' | 'relay' | 'provider' | 'plan' | 'proto' | 'secret';
export interface Col { key: 'kind' | Field; label: string; w: number; type: ColType; kinds: Kind[]; ph?: string }
const ALL: Kind[] = ['relay', 'vps', 'proxy'];
/** 表头顺序即粘贴的列顺序 */
export const COLS: Col[] = [
  { key: 'kind', label: '类型', w: 104, type: 'kind', kinds: ALL },
  { key: 'ip', label: 'IP', w: 132, type: 'text', kinds: ALL, ph: '198.51.100.20' },
  { key: 'ssh_port', label: 'SSH 端口', w: 84, type: 'num', kinds: ['relay', 'vps'], ph: '22' },
  { key: 'team_id', label: '团队', w: 104, type: 'team', kinds: ['vps', 'proxy'] },
  { key: 'number', label: '编号', w: 76, type: 'num', kinds: ['vps', 'proxy'], ph: '自动' },
  { key: 'label', label: '线路机名', w: 108, type: 'text', kinds: ['relay'], ph: '线路机3' },
  { key: 'relay_id', label: '挂在线路机', w: 116, type: 'relay', kinds: ['vps', 'proxy'] },
  { key: 'provider_id', label: '供应商', w: 112, type: 'provider', kinds: ALL },
  { key: 'plan_id', label: '套餐', w: 150, type: 'plan', kinds: ALL },
  { key: 'service_ref', label: '服务编号', w: 104, type: 'text', kinds: ALL },
  { key: 'ss_port', label: 'SS 端口', w: 84, type: 'num', kinds: ['vps'], ph: '8388' },
  { key: 'login_user', label: '登录用户', w: 92, type: 'text', kinds: ['relay', 'vps'], ph: 'root' },
  { key: 'login_pw', label: '登录密码', w: 108, type: 'secret', kinds: ['relay', 'vps'], ph: '留空用密钥' },
  { key: 'up_protocol', label: '上游协议', w: 92, type: 'proto', kinds: ['proxy'] },
  { key: 'up_host', label: '上游主机', w: 124, type: 'text', kinds: ['proxy'] },
  { key: 'up_port', label: '上游端口', w: 84, type: 'num', kinds: ['proxy'] },
  { key: 'up_user', label: '上游用户', w: 96, type: 'text', kinds: ['proxy'] },
  { key: 'up_pw', label: '上游密码', w: 104, type: 'secret', kinds: ['proxy'] },
  { key: 'due_date', label: '到期日', w: 148, type: 'date', kinds: ALL, ph: '2026-12-01' },
  { key: 'price', label: '周期费用', w: 96, type: 'text', kinds: ALL },
  { key: 'note', label: '备注', w: 140, type: 'text', kinds: ALL },
];
export const applies = (c: Col, kind: Kind) => c.kinds.includes(kind);

const KIND_ALIAS: Record<string, Kind> = { relay: 'relay', 线路机: 'relay', 中转: 'relay', vps: 'vps', 落地机: 'vps', 节点: 'vps', 落地: 'vps', proxy: 'proxy', 家宽: 'proxy', 家宽代理: 'proxy' };

export interface Ctx { teams: Team[]; relays: { id: string; name: string }[]; providers: Provider[]; plans: Plan[] }

/** 把一个粘贴来的文本单元格写进行：选择型字段按名称匹配 */
export function setCell(row: Row, col: Col, raw: string, ctx: Ctx): Row {
  const v = raw.trim();
  const r = { ...row, unresolved: { ...row.unresolved } };
  delete r.unresolved[col.key];
  const find = <T,>(list: T[], pick: (t: T) => string[]) => list.find((t) => pick(t).some((s) => s && s.toLowerCase() === v.toLowerCase()));
  switch (col.type) {
    case 'kind': { const k = KIND_ALIAS[v.toLowerCase()] ?? KIND_ALIAS[v]; if (k) r.kind = k; else if (v) r.unresolved.kind = v; break; }
    case 'team': { const t = find(ctx.teams, (x) => [x.name, x.id]); r.team_id = t?.id ?? ''; if (v && !t) r.unresolved.team_id = v; break; }
    case 'relay': { const t = find(ctx.relays, (x) => [x.name, x.id]); r.relay_id = t?.id ?? ''; if (v && !t) r.unresolved.relay_id = v; break; }
    case 'provider': { const t = find(ctx.providers, (x) => [x.name, x.code, x.id]); r.provider_id = t?.id ?? ''; if (v && !t) r.unresolved.provider_id = v; break; }
    case 'plan': { const t = find(ctx.plans, (x) => [x.name, x.id]); r.plan_id = t?.id ?? ''; if (v && !t) r.unresolved.plan_id = v; break; }
    case 'proto': r.up_protocol = v.toLowerCase() === 'http' ? 'http' : 'socks5'; break;
    default: (r as unknown as Record<string, string>)[col.key] = v;
  }
  return r;
}

const num = (s: string) => (s.trim() === '' ? undefined : Number(s));
/** 订阅只带到期日和手填的费用：币种、周期取套餐，账单时区取供应商，都由服务端补 */
export function toItem(r: Row): ProvisionItem {
  const it: ProvisionItem = { kind: r.kind, ip: r.ip.trim() };
  if (r.kind !== 'proxy' && r.ssh_port) it.ssh_port = num(r.ssh_port);
  if (r.kind === 'relay') it.label = r.label.trim();
  else { it.team_id = r.team_id; it.number = r.number.trim() === '' ? null : Number(r.number); it.relay_id = r.relay_id; }
  if (r.provider_id) it.provider_id = r.provider_id;
  if (r.plan_id) it.plan_id = r.plan_id;
  if (r.service_ref.trim()) it.service_ref = r.service_ref.trim();
  if (r.note.trim()) it.note = r.note.trim();
  if (r.kind !== 'proxy' && (r.login_user.trim() || r.login_pw)) it.login = { user: r.login_user.trim() || 'root', password: r.login_pw || undefined };
  if (r.kind === 'proxy') it.upstream = { protocol: r.up_protocol === 'http' ? 'http' : 'socks5', host: r.up_host.trim(), port: Number(r.up_port) || 0, username: r.up_user.trim() || undefined, password: r.up_pw || undefined };
  if (r.kind === 'vps' && r.ss_port) it.ss_port = num(r.ss_port);
  if (r.due_date) it.subscription = { due_date: r.due_date, price_cents: r.price.trim() ? Math.round(Number(r.price) * 100) : undefined };
  return it;
}

/** 提交前的本地校验（不依赖后端）：返回每行的错误 */
export function localErrors(r: Row): string[] {
  const e: string[] = [];
  for (const [k, v] of Object.entries(r.unresolved)) { const c = COLS.find((x) => x.key === k); e.push(`${c?.label ?? k}「${v}」不存在`); }
  if (!r.ip.trim()) e.push('请填写 IP');
  if (r.kind === 'relay' && !r.label.trim()) e.push('请填写线路机名');
  if (r.kind !== 'relay') { if (!r.team_id && !r.unresolved.team_id) e.push('请选择团队'); if (!r.relay_id && !r.unresolved.relay_id) e.push('请选择挂载的线路机'); }
  if (r.number.trim() !== '' && (!/^\d+$/.test(r.number.trim()) || Number(r.number) < 1 || Number(r.number) > 999)) e.push('编号应为 1–999 的整数，或留空自动编号');
  for (const k of ['ssh_port', 'ss_port', 'up_port'] as const) if (r[k].trim() !== '' && !/^\d+$/.test(r[k].trim())) e.push(`${COLS.find((c) => c.key === k)!.label}应为整数`);
  if (r.kind === 'proxy' && (!r.up_host.trim() || !r.up_port.trim())) e.push('家宽代理要填上游协议（socks5/http）、主机和端口');
  if (r.due_date && !/^\d{4}-\d{2}-\d{2}$/.test(r.due_date)) e.push('到期日格式应为 YYYY-MM-DD');
  if (r.price.trim() && !isFinite(Number(r.price))) e.push('费用应为数字');
  // 费用只挂在订阅上：没有到期日就没有订阅，填了的费用会被悄悄丢掉——在这里拦下
  if (r.price.trim() && !r.due_date) e.push('填了费用，也需要填到期日');
  return e;
}

/** 把 TSV 文本解析成二维数组（支持引号包裹的单元格） */
export function parseTsv(text: string): string[][] {
  const rows: string[][] = []; let row: string[] = []; let cell = ''; let q = false;
  for (let i = 0; i < text.length; i++) {
    const c = text[i];
    if (q) { if (c === '"') { if (text[i + 1] === '"') { cell += '"'; i++; } else q = false; } else cell += c; }
    else if (c === '"' && cell === '') q = true;
    else if (c === '\t') { row.push(cell); cell = ''; }
    else if (c === '\n' || c === '\r') { if (c === '\r' && text[i + 1] === '\n') i++; row.push(cell); rows.push(row); row = []; cell = ''; }
    else cell += c;
  }
  if (cell !== '' || row.length) { row.push(cell); rows.push(row); }
  return rows.filter((r) => r.some((c) => c.trim() !== ''));
}
