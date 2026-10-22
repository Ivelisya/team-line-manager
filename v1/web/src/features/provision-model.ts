// 上架表单的数据模型：行 → ProvisionItem、字段适用性、从表格软件粘贴的 TSV 解析、本地校验。
import type { Kind, Plan, Provider, ProvisionItem, Team } from '@/lib/types';
import type { HostChoice } from '@/lib/queries';
import { parseCountry } from '@/lib/flags';
import { meta } from '@/lib/derive';

export interface Row {
  /** 稳定的行 id（React key、单元格回调用），不进入提交内容 */
  id: string;
  kind: Kind; ip: string; ssh_port: string; team_id: string; number: string; label: string; relay_id: string;
  provider_id: string; plan_id: string; service_ref: string; note: string;
  login_pw: string;
  /** 国家：两位代码，空 = 按 IP 自动 */
  country: string;
  up_protocol: string; up_host: string; up_port: string; up_user: string; up_pw: string;
  ss_port: string; due_date: string; price: string;
  /** 同机 IP 节点：add_ip 是单台表单的「在已有机器上添加 IP」开关（批量里随「宿主机」格填没填），host_id 是宿主机 */
  add_ip: boolean; host_id: string;
  /** 粘贴时没匹配上的文本：字段 → 原文，用于本地报错 */
  unresolved: Record<string, string>;
}
export type Field = Exclude<keyof Row, 'id' | 'unresolved' | 'kind' | 'add_ip'>;

let rowSeq = 0;
export const newRow = (p?: Partial<Row>): Row => ({
  id: `r${++rowSeq}`, kind: 'vps', ip: '', ssh_port: '', team_id: '', number: '', label: '', relay_id: '', provider_id: '', plan_id: '', service_ref: '', note: '',
  login_pw: '', country: '', up_protocol: 'socks5', up_host: '', up_port: '', up_user: '', up_pw: '', ss_port: '', due_date: '', price: '', add_ip: false, host_id: '', unresolved: {}, ...p,
});

export type ColType = 'text' | 'num' | 'date' | 'kind' | 'host' | 'team' | 'relay' | 'provider' | 'plan' | 'proto' | 'secret' | 'country';
export interface Col { key: 'kind' | Field; label: string; w: number; type: ColType; kinds: Kind[]; ph?: string }
const ALL: Kind[] = ['relay', 'vps', 'proxy'];
/** 不带表头粘贴时，表头顺序即列顺序。后加的列（宿主机、国家）排在最后：按旧表头整理的表格粘进来不会整体错一列；
 *  带表头（「复制表头」做的模板）粘贴时按列名对上，见 headerCols */
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
  { key: 'ss_port', label: 'SS 端口', w: 84, type: 'num', kinds: ['vps'], ph: String(meta().defaults.ss_port) }, // 只被懒加载的上架页引用，求值时 /meta 已到
  { key: 'login_pw', label: 'root 密码', w: 108, type: 'secret', kinds: ['relay', 'vps'], ph: '留空用密钥' },
  { key: 'up_protocol', label: '上游协议', w: 92, type: 'proto', kinds: ['proxy'] },
  { key: 'up_host', label: '上游主机', w: 124, type: 'text', kinds: ['proxy'] },
  { key: 'up_port', label: '上游端口', w: 84, type: 'num', kinds: ['proxy'] },
  { key: 'up_user', label: '上游用户', w: 96, type: 'text', kinds: ['proxy'] },
  { key: 'up_pw', label: '上游密码', w: 104, type: 'secret', kinds: ['proxy'] },
  { key: 'due_date', label: '到期日', w: 148, type: 'date', kinds: ALL, ph: '2026-12-01' },
  { key: 'price', label: '周期费用', w: 96, type: 'text', kinds: ALL, ph: '跟随套餐' },
  { key: 'note', label: '备注', w: 140, type: 'text', kinds: ALL },
  { key: 'host_id', label: '宿主机', w: 116, type: 'host', kinds: ['vps'] },
  { key: 'country', label: '国家', w: 132, type: 'country', kinds: ALL },
];
/** 同机 IP 节点：VPS 节点行打开了「在已有机器上添加 IP」（批量里是填了宿主机） */
export const isIpNode = (r: Pick<Row, 'kind' | 'add_ip'>) => r.kind === 'vps' && r.add_ip;
/** 同机 IP 节点不适用的列：SSH 端口、首次登录与供应商取宿主机的，SS 端口由中控在宿主机上分配，不带套餐 */
const HOST_NA = new Set<Col['key']>(['ssh_port', 'login_pw', 'ss_port', 'provider_id', 'plan_id']);
export const applies = (c: Col, r: Pick<Row, 'kind' | 'add_ip'>) => c.kinds.includes(r.kind) && !(isIpNode(r) && HOST_NA.has(c.key));

const KIND_ALIAS: Record<string, Kind> = { relay: 'relay', 线路机: 'relay', 中转: 'relay', vps: 'vps', 'vps 节点': 'vps', 落地机: 'vps', 节点: 'vps', 落地: 'vps', proxy: 'proxy', 家宽: 'proxy', 家宽代理: 'proxy' };

export interface Ctx { teams: Team[]; relays: { id: string; name: string }[]; hosts: HostChoice[]; providers: Provider[]; plans: Plan[] }

/** 把一个粘贴来的文本单元格写进行：选择型字段按名称匹配 */
export function setCell(row: Row, col: Col, raw: string, ctx: Ctx): Row {
  const v = raw.trim();
  const r = { ...row, unresolved: { ...row.unresolved } };
  delete r.unresolved[col.key];
  const find = <T,>(list: T[], pick: (t: T) => string[]) => list.find((t) => pick(t).some((s) => s && s.toLowerCase() === v.toLowerCase()));
  switch (col.type) {
    case 'kind': { const k = KIND_ALIAS[v.toLowerCase()] ?? KIND_ALIAS[v]; if (k) r.kind = k; else if (v) r.unresolved.kind = v; break; }
    case 'host': { const t = find(ctx.hosts, (x) => [x.name, x.id]); r.host_id = t?.id ?? ''; r.add_ip = !!v; if (v && !t) r.unresolved.host_id = v; break; }
    case 'team': { const t = find(ctx.teams, (x) => [x.name, x.id]); r.team_id = t?.id ?? ''; if (v && !t) r.unresolved.team_id = v; break; }
    case 'relay': { const t = find(ctx.relays, (x) => [x.name, x.id]); r.relay_id = t?.id ?? ''; if (v && !t) r.unresolved.relay_id = v; break; }
    case 'provider': { const t = find(ctx.providers, (x) => [x.name, x.code, x.id]); r.provider_id = t?.id ?? ''; if (v && !t) r.unresolved.provider_id = v; break; }
    case 'plan': { const t = find(ctx.plans, (x) => [x.name, x.id]); r.plan_id = t?.id ?? ''; if (v && !t) r.unresolved.plan_id = v; break; }
    case 'country': { const c = parseCountry(v); r.country = c ?? ''; if (c === undefined) r.unresolved.country = v; break; }
    case 'proto': r.up_protocol = v.toLowerCase() === 'http' ? 'http' : 'socks5'; break;
    default: (r as unknown as Record<string, string>)[col.key] = v;
  }
  return r;
}

/** 粘贴的第一行是不是表头：至少两格是列名、其余格是空的或已去掉的旧列名。是的话返回每格对应的列（旧列为 undefined，跳过） */
const OLD_LABELS = new Set(['登录用户', '登录密码']);
export function headerCols(line: string[]): (Col | undefined)[] | null {
  const cols = line.map((h) => COLS.find((c) => c.label === h.trim() || (c.key === 'login_pw' && h.trim() === '登录密码')));
  const ok = cols.filter(Boolean).length >= 2 && line.every((h, i) => cols[i] || !h.trim() || OLD_LABELS.has(h.trim()));
  return ok ? cols : null;
}

/** 把粘贴来的一行写进行：cols 是每格对应的列（不带表头时是从光标所在列起的 COLS），对这一行不适用的格子跳过。
 *  类型和宿主机决定别的格子适不适用，先写这两列——宿主机靠后，按列序写会先把同机 IP 节点不适用的格子（SSH 端口、登录、供应商…）写进去 */
export function pasteLine(row: Row, line: string[], cols: (Col | undefined)[], ctx: Ctx): Row {
  const cells = line.flatMap((cell, dx) => (cols[dx] ? [{ col: cols[dx]!, cell }] : []));
  const lead = ({ col }: { col: Col }) => col.key === 'kind' || col.key === 'host_id';
  let r = row;
  for (const { col, cell } of [...cells.filter(lead), ...cells.filter((x) => !lead(x))]) {
    if (col.key === 'kind' || applies(col, r)) r = setCell(r, col, cell, ctx);
  }
  return r;
}

const num = (s: string) => (s.trim() === '' ? undefined : Number(s));
/** 订阅只带到期日和手填的费用（单独设置）：价格、币种、周期、流量跟随套餐，账单时区、提醒、流量重置跟随供应商，都不抄进订阅。
 *  同机 IP 节点带 host_id，不带它不适用的字段（SSH 端口、登录、SS 端口、供应商、套餐；服务端也会忽略） */
export function toItem(r: Row): ProvisionItem {
  const it: ProvisionItem = { kind: r.kind, ip: r.ip.trim() };
  const ipNode = isIpNode(r);
  if (ipNode) it.host_id = r.host_id;
  if (r.kind !== 'proxy' && r.ssh_port && !ipNode) it.ssh_port = num(r.ssh_port);
  if (r.kind === 'relay') it.label = r.label.trim();
  else { it.team_id = r.team_id; it.number = r.number.trim() === '' ? null : Number(r.number); it.relay_id = r.relay_id; }
  if (r.provider_id && !ipNode) it.provider_id = r.provider_id;
  if (r.plan_id && !ipNode) it.plan_id = r.plan_id;
  if (r.service_ref.trim()) it.service_ref = r.service_ref.trim();
  if (r.note.trim()) it.note = r.note.trim();
  if (r.kind !== 'proxy' && !ipNode && r.login_pw) it.login = { password: r.login_pw };
  if (r.country) it.country = r.country;
  if (r.kind === 'proxy') it.upstream = { protocol: r.up_protocol === 'http' ? 'http' : 'socks5', host: r.up_host.trim(), port: Number(r.up_port) || 0, username: r.up_user.trim() || undefined, password: r.up_pw || undefined };
  if (r.kind === 'vps' && r.ss_port && !ipNode) it.ss_port = num(r.ss_port);
  if (r.due_date) it.subscription = { due_date: r.due_date, price_cents: r.price.trim() ? Math.round(Number(r.price) * 100) : undefined };
  return it;
}

/** 批量里没动过的行：用户要填的格子都空（类型、团队、线路机、供应商、套餐、宿主机是从上一行带下来的默认值，不算） */
export const isBlank = (r: Row) => !Object.keys(r.unresolved).length
  && [r.ip, r.ssh_port, r.number, r.label, r.service_ref, r.note, r.login_pw, r.up_host, r.up_port, r.up_user, r.up_pw, r.ss_port, r.due_date, r.price, r.country].every((v) => !v.trim());

/** 提交前的本地校验（不依赖后端）：每条错误带出错的字段，单台表单把它标在字段旁 */
export function localIssues(r: Row): { key: keyof Row; msg: string }[] {
  const e: { key: keyof Row; msg: string }[] = [];
  const add = (key: keyof Row, msg: string) => e.push({ key, msg });
  const col = (k: Col['key']) => COLS.find((c) => c.key === k)!;
  // 不适用的格子（灰格：类型不对、同机 IP 节点沿用宿主机的）不提交，里面残留的内容不报错
  for (const [k, v] of Object.entries(r.unresolved)) { const c = col(k as Col['key']); if (applies(c, r)) add(k as keyof Row, `${c.label}「${v}」不存在`); }
  if (!r.ip.trim()) add('ip', '请填写 IP');
  if (isIpNode(r) && !r.host_id && !r.unresolved.host_id) add('host_id', '请选择宿主机');
  if (r.kind === 'relay' && !r.label.trim()) add('label', '请填写线路机名');
  if (r.kind !== 'relay') { if (!r.team_id && !r.unresolved.team_id) add('team_id', '请选择团队'); if (!r.relay_id && !r.unresolved.relay_id) add('relay_id', '请选择挂载的线路机'); }
  if (r.number.trim() !== '' && (!/^\d+$/.test(r.number.trim()) || Number(r.number) < 1 || Number(r.number) > 999)) add('number', '编号应为 1–999');
  for (const k of ['ssh_port', 'ss_port', 'up_port'] as const) if (applies(col(k), r) && r[k].trim() !== '' && !/^\d+$/.test(r[k].trim())) add(k, `${col(k).label}应为整数`);
  if (r.kind === 'proxy' && (!r.up_host.trim() || !r.up_port.trim())) add(r.up_host.trim() ? 'up_port' : 'up_host', '请填上游主机和端口');
  if (r.due_date && !/^\d{4}-\d{2}-\d{2}$/.test(r.due_date)) add('due_date', '到期日格式应为 YYYY-MM-DD');
  if (r.price.trim() && !isFinite(Number(r.price))) add('price', '费用应为数字');
  // 费用只挂在订阅上：没有到期日就没有订阅，填了的费用会被悄悄丢掉——在这里拦下
  if (r.price.trim() && !r.due_date) add('due_date', '填了费用，也需要填到期日');
  // 选了套餐就建订阅（费用、流量跟随套餐，只要到期日）；没有套餐（含同机 IP 节点）时费用没有可跟随的（同服务端校验）
  const plan = !isIpNode(r) && (r.plan_id || r.unresolved.plan_id);
  if (plan && !r.due_date) add('due_date', '选了套餐要填到期日');
  if (!plan && r.due_date && !r.price.trim()) add('price', '请填费用（免费填 0）');
  return e;
}
export const localErrors = (r: Row): string[] => localIssues(r).map((x) => x.msg);

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
