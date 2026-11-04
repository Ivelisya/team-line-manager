// mock 的计费台账（同服务端规格：计费名额、增减记录、每一期已支付 / 待支付）与客户页（公开 /public/customer/{token}）。
// 口径同 batches.rs：每个节点从开始收费日起每满一个月收一次（一期，月底对齐）；按月看 = 每个节点起点在这个月的那一期；已支付按 (名额, 期起点) 记。
// 模拟数据覆盖同日起算、月末起算、替换和新增节点等场景，不模拟每分钟的对账。
// 「今天」固定为 2026-11-03；分享链接与密码按需随机生成。
import type { BillingEvent, BillingEventKind, BillingMonth, BillingMonthNode, CustomerPage, MachineView, PublicCustomer, Team } from '@/lib/types';
import { addDays, addMonths } from '@/lib/format';
import { compareBillingEffectiveDate } from '@/lib/sort';

type Reg = (method: string, path: string, h: (c: { params: string[]; query: URLSearchParams; body: any }) => unknown) => void; // eslint-disable-line @typescript-eslint/no-explicit-any
type Bad = (msg: string, status?: number, code?: string) => Error;

const TODAY = '2026-11-03';
interface MSlot { id: string; team_id: string; region: string; start_date: string; holder: { id: string; name: string } | null; last_holder_name: string | null; end_date: string | null; manual: boolean; note: string }
interface MEvent extends BillingEvent { team_id: string }
interface MPage { token: string; password?: string; enabled: boolean; created_at: string; token_at: string; last_view_at: string | null }

let seq = 0;
const nid = (p: string) => `${p}${(++seq).toString(16).padStart(8, '0')}`;
const p2 = (n: number) => String(n).padStart(2, '0');
const DATE = /^\d{4}-\d{2}-\d{2}$/;
const MONTH = /^\d{4}-\d{2}$/;
const validDate = (s: unknown): s is string => typeof s === 'string' && DATE.test(s) && !Number.isNaN(Date.parse(`${s}T00:00:00Z`)) && new Date(`${s}T00:00:00Z`).toISOString().slice(0, 10) === s;
/** 「10 月 30 日」 */
const cn = (s: string) => `${Number(s.slice(5, 7))} 月 ${Number(s.slice(8, 10))} 日`;
const token32 = () => btoa(String.fromCharCode(...crypto.getRandomValues(new Uint8Array(32)))).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
const cmp = (a: string, b: string) => (a < b ? -1 : a > b ? 1 : 0);
const monthNo = (s: string) => Number(s.slice(0, 4)) * 12 + Number(s.slice(5, 7));

// ---------- 初始数据 ----------
const slots: MSlot[] = [];
const events: MEvent[] = [];
/** 已支付：`名额 id|期起点` → 标记时刻 */
const paid = new Map<string, string>();
const ev = (team_id: string, slot_id: string | null, at: string, kind: BillingEventKind, machine_name: string | null, other_name: string | null, text: string) => {
  events.push({ id: nid('ev'), team_id, slot_id, at, kind, machine_name, other_name, text });
};
const holder = (team: 't1' | 't2', n: number) => ({ id: `m-${team}-${p2(n)}`, name: `${team === 't1' ? '团队1' : '团队2'}-${p2(n)}` });
const blank = { holder: null, last_holder_name: null, end_date: null, manual: false, note: '' };
for (const [tid, n] of [['t1', 9], ['t2', 19]] as const) {
  for (let i = 1; i <= n; i++) slots.push({ ...blank, id: nid('sl'), team_id: tid, region: '美区', start_date: '2026-10-01', holder: holder(tid, i) });
  ev(tid, null, '2026-10-01T00:00:00+08:00', 'init', null, null, `开始记账：${n} 个节点从 10 月 1 日开始收费`);
}
{
  const h = holder('t1', 10);
  const s: MSlot = { ...blank, id: nid('sl'), team_id: 't1', region: '美区', start_date: '2026-10-30', holder: h };
  slots.push(s);
  ev('t1', s.id, '2026-10-30T14:26:00+08:00', 'add', h.name, null, `${h.name} 加入，从 10 月 30 日开始收费`);
  // 10-28 团队2 的两次替换：换下来的旧节点进了备用（改名 备用-NN），新节点接过编号，开始收费日不变
  for (const [n, spare, at] of [[7, '备用-01', '2026-10-28T10:05:00+08:00'], [12, '备用-02', '2026-10-28T16:40:00+08:00']] as const) {
    const sl = slots.find((x) => x.team_id === 't2' && x.holder?.name === `团队2-${p2(n)}`)!;
    ev('t2', sl.id, at, 'swap', sl.holder!.name, spare, `${spare} → ${sl.holder!.name} 替换，收费不变`);
  }
  // 今天上架的团队2-20：上架即记账，和别的节点在同一张清单里（没有订阅，从今天开始收费）
  const n20: MSlot = { ...blank, id: nid('sl'), team_id: 't2', region: '美区', start_date: TODAY, holder: { id: 'm-t2-20', name: '团队2-20' } };
  slots.push(n20);
  ev('t2', n20.id, '2026-11-03T09:12:00+08:00', 'add', '团队2-20', null, `团队2-20 加入，从 ${cn(TODAY)}开始收费`);
}
// 各种状态的样例（只为界面覆盖，不是生产形状）：手动添加的、10 月中停止收费的、今天离开的（本期照收）、未标地区、31 号开始收费
{
  const add = (team_id: 't1' | 't2', region: string, start_date: string, o: Partial<MSlot>) => slots.push({ ...blank, id: nid('sl'), team_id, region, start_date, ...o });
  add('t1', '墨西哥', '2026-10-15', { manual: true, note: '墨西哥家宽' });
  add('t1', '美区', '2026-10-01', { last_holder_name: '团队1-11', end_date: '2026-10-20' });
  add('t1', '美区', '2026-10-01', { last_holder_name: '团队1-12', end_date: TODAY });
  add('t2', '未知', '2026-10-12', { holder: { id: 'm-t2-21', name: '团队2-21' } });
  add('t2', '美区', '2026-08-31', { holder: { id: 'm-t2-22', name: '团队2-22' } });
}
// 10 月：除团队1-10 外都已支付；11 月：团队2 收了前 5 个
for (const s of slots) {
  const p = periodIn(s, '2026-10');
  if (p && s.holder?.name !== '团队1-10') paid.set(`${s.id}|${p[0]}`, '2026-10-31T12:00:00+08:00');
}
for (const s of slots.filter((x) => x.team_id === 't2').slice(0, 5)) paid.set(`${s.id}|${periodIn(s, '2026-11')![0]}`, '2026-11-02T10:30:00+08:00');
const pages = new Map<string, MPage>();

// 客户页通过 noopener 新标签页打开，不能共享管理端的模块内存。
// 仅将这组模拟台账持久化；每次请求先读再写，让新链接、换链、停用和付款状态跨页一致。
const STORAGE_KEY = 'fleet:mock:customer:v1';
interface StoredCustomer {
  seq: number;
  slots: MSlot[];
  events: MEvent[];
  paid: [string, string][];
  pages: [string, MPage][];
}
function readCustomerState() {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return;
    const state: StoredCustomer = JSON.parse(raw);
    if (!Number.isSafeInteger(state.seq) || state.seq < 0 || !Array.isArray(state.slots)
      || !Array.isArray(state.events) || !Array.isArray(state.paid) || !Array.isArray(state.pages)) return;
    const savedPaid = new Map(state.paid), savedPages = new Map(state.pages);
    seq = state.seq;
    slots.splice(0, slots.length, ...state.slots);
    events.splice(0, events.length, ...state.events);
    paid.clear(); savedPaid.forEach((value, key) => paid.set(key, value));
    pages.clear(); savedPages.forEach((value, key) => pages.set(key, value));
  } catch { /* 浏览器禁用存储或旧数据损坏时，仍可用内存中的演示数据 */ }
}
function writeCustomerState() {
  try {
    const state: StoredCustomer = { seq, slots, events, paid: [...paid], pages: [...pages] };
    const raw = JSON.stringify(state);
    if (localStorage.getItem(STORAGE_KEY) !== raw) localStorage.setItem(STORAGE_KEY, raw);
  } catch { /* 存储不可用时继续使用当前标签页的模拟数据 */ }
}

// ---------- 口径 ----------
/** 起点落在 month（YYYY-MM）那个月、要收费的那一期 [起点, 终点)；没有为 null */
function periodIn(s: MSlot, month: string): [string, string] | null {
  const k = monthNo(month) - monthNo(s.start_date);
  if (k < 0) return null;
  const ps = addMonths(s.start_date, k);
  return !s.end_date || ps < s.end_date ? [ps, addMonths(s.start_date, k + 1)] : null;
}
const slotName = (s: MSlot) => (s.manual ? s.note || '手动添加的节点' : s.holder?.name ?? s.last_holder_name ?? '（已删除的节点）');
/** 停止收费的名额收到哪天为止（不含）：起点早于结束日的最后一期的终点（同服务端 billed_until 的后一天） */
const paidTo = (start: string, end: string) => { let k = 0; while (addMonths(start, k + 1) < end) k++; return addMonths(start, k + 1); };
const ended = (s: MSlot) => !!s.end_date && s.end_date <= TODAY;
/** 可以翻看的月份：最早的开始收费月 → 下个月 */
function monthsOf(tid: string) {
  const last = addMonths(`${TODAY.slice(0, 7)}-01`, 1).slice(0, 7);
  let m = slots.filter((s) => s.team_id === tid).reduce((a, s) => (s.start_date.slice(0, 7) < a ? s.start_date.slice(0, 7) : a), TODAY.slice(0, 7));
  const out: string[] = [];
  while (m <= last) { out.push(m); m = addMonths(`${m}-01`, 1).slice(0, 7); }
  return out;
}
/** 某月清单：按本期生效日期、节点编号排列，支付状态不影响顺序。 */
function rows(tid: string, month: string): BillingMonthNode[] {
  return slots.filter((s) => s.team_id === tid).flatMap((s) => {
    const p = periodIn(s, month);
    if (!p) return [];
    const at = paid.get(`${s.id}|${p[0]}`) ?? null;
    return [{
      slot_id: s.id, name: slotName(s), region: s.region, start_date: s.start_date, period_start: p[0], period_end: p[1], upcoming: p[0] > TODAY,
      manual: s.manual, note: s.note, holder_id: s.holder?.id ?? null, ended: ended(s), paid: !!at, paid_at: at,
    }];
  }).sort(compareBillingEffectiveDate);
}
const eventView = ({ team_id: _t, ...e }: MEvent): BillingEvent => e; // eslint-disable-line @typescript-eslint/no-unused-vars
const teamEvents = (tid: string) => events.filter((e) => e.team_id === tid).sort((a, b) => Date.parse(b.at) - Date.parse(a.at) || cmp(b.id, a.id));

export function installCustomerMock(register: Reg, o: { teams: () => Team[]; machines: () => MachineView[]; bad: Bad; emit: (e: { type: string; id?: string }) => void }) {
  const R: Reg = (method, path, handler) => register(method, path, (context) => {
    readCustomerState();
    const result = handler(context);
    writeCustomerState();
    return result;
  });
  const { bad } = o;
  const billing = (tid: string) => o.emit({ type: 'billing', id: tid });
  const team = (id: string) => { const t = o.teams().find((x) => x.id === id); if (!t) throw bad('没有这个团队', 404, 'not_found'); return t; };
  const slot = (id: string) => { const s = slots.find((x) => x.id === id); if (!s) throw bad('没有这个计费节点', 404, 'not_found'); return s; };
  const pageView = (tid: string): CustomerPage | null => {
    let p = pages.get(tid);
    if (!team(tid).spare && (!p || (p.enabled && (!p.password || Date.now() >= Date.parse(p.token_at) + 3600000)))) {
      const now = new Date().toISOString();
      p = { token: token32(), password: token32().slice(0, 16), enabled: true,
        created_at: p?.created_at ?? now, token_at: now, last_view_at: null };
      pages.set(tid, p);
    }
    return p ? { password: p.password ?? null, expires_at: new Date(Date.parse(p.token_at) + 3600000).toISOString(), enabled: p.enabled, link: `${location.origin}/c/${p.token}`, created_at: p.created_at, token_at: p.token_at, last_view_at: p.last_view_at } : null;
  };
  const region = (v: unknown) => { const s = String(v ?? '').trim(); if (!s || [...s].length > 20) throw bad('地区要 1–20 个字，如 美区、墨西哥'); return s; };
  const note = (v: unknown) => { const s = String(v ?? '').trim(); if ([...s].length > 60) throw bad('备注最多 60 个字'); return s; };
  const date = (v: unknown, what: string) => { if (!validDate(v)) throw bad(`${what}要写成 2026-10-31 这样的日期`); return v; };
  /** 开始收费日最晚今天加一个月（同服务端 check_start） */
  const startDate = (v: unknown) => { const d = date(v, '开始收费日'); const max = addMonths(TODAY, 1); if (d > max) throw bad(`开始收费日最晚填到 ${max}（今天起一个月内）`); return d; };
  const monthQ = (v: string | null) => { if (v == null || v === '') return TODAY.slice(0, 7); if (!MONTH.test(v)) throw bad('月份要写成 2026-11 这样'); return v; };
  const view = (tid: string, month: string): BillingMonth => {
    const t = team(tid);
    const nodes = rows(tid, month);
    return { team: { id: t.id, name: t.name }, today: TODAY, initialized: true, page: pageView(tid), month, months: monthsOf(tid), nodes };
  };

  R('GET', '/teams/([^/]+)/billing/outstanding', ({ params }) => {
    const t = team(params[0]);
    const nodes = monthsOf(t.id).flatMap(month => rows(t.id, month).filter(n => !n.paid && !n.upcoming));
    nodes.sort(compareBillingEffectiveDate);
    return { team: { id: t.id, name: t.name }, today: TODAY, nodes };
  });
  R('GET', '/teams/([^/]+)/billing/month', ({ params, query }) => view(team(params[0]).id, monthQ(query.get('month'))));
  R('POST', '/teams/([^/]+)/billing/paid', ({ params, body }) => {
    const t = team(params[0]);
    if (!MONTH.test(String(body.month ?? ''))) throw bad('月份要写成 2026-11 这样');
    const ids: string[] = Array.isArray(body.slot_ids) ? body.slot_ids : [];
    if (!ids.length) throw bad('一次标记 1–500 个节点');
    const list = rows(t.id, body.month);
    const wrong = ids.filter((id) => !list.some((n) => n.slot_id === id));
    if (wrong.length) throw bad(`${Number(body.month.slice(5))} 月没有要收这些节点的钱（不是这个团队的，或这个月不收费）：${wrong.map((x) => x.slice(0, 8)).join('、')}`);
    const changed = list.filter((n) => ids.includes(n.slot_id) && n.paid !== !!body.paid);
    for (const n of changed) {
      const k = `${n.slot_id}|${n.period_start}`;
      if (body.paid) paid.set(k, new Date().toISOString()); else paid.delete(k);
    }
    if (changed.length) {
      const names = changed.map((n) => n.name);
      ev(t.id, null, new Date().toISOString(), body.paid ? 'paid' : 'unpaid', null, null, `${Number(body.month.slice(5))} 月${body.paid ? '已支付' : '改回待支付'}：${names.slice(0, 10).join('、')}${names.length > 10 ? ' 等' : ''}（共 ${names.length} 个）`);
      billing(t.id);
    }
    return view(t.id, body.month);
  });
  R('GET', '/teams/([^/]+)/billing-events', ({ params, query }) => {
    const t = team(params[0]);
    const limit = Math.min(500, Number(query.get('limit') ?? 200) || 200);
    let list = teamEvents(t.id);
    const before = query.get('before');
    if (before) { const i = list.findIndex((e) => e.id === before); list = i < 0 ? [] : list.slice(i + 1); }
    return list.slice(0, limit).map(eventView);
  });
  R('POST', '/teams/([^/]+)/billing-slots', ({ params, body }) => {
    const t = team(params[0]);
    const start = startDate(body.start_date); const r = region(body.region); const n = note(body.note);
    const count = body.count == null ? 1 : Number(body.count);
    if (!Number.isInteger(count) || count < 1 || count > 50) throw bad('一次添加 1–50 个');
    for (let i = 0; i < count; i++) {
      const s: MSlot = { ...blank, id: nid('sl'), team_id: t.id, region: r, start_date: start, manual: true, note: n };
      slots.push(s);
      ev(t.id, s.id, new Date().toISOString(), 'manual_add', slotName(s), null, `手动添加 ${slotName(s)}（${r}），从 ${cn(start)}开始收费`);
    }
    billing(t.id);
    return { count };
  });
  R('PATCH', '/billing-slots/([^/]+)', ({ params, body }) => {
    const s = slot(params[0]); const name = slotName(s);
    const changes: string[] = [];
    if (body.start_date !== undefined) {
      const v = startDate(body.start_date);
      if (v !== s.start_date) {
        // 已支付的期按期序号 k 挪到新开始日的第 k 期，对不上旧期序列的丢掉（同服务端 shift_paid）
        for (const [key, at] of [...paid]) {
          if (!key.startsWith(`${s.id}|`)) continue;
          paid.delete(key);
          const ps = key.slice(s.id.length + 1); const k = monthNo(ps) - monthNo(s.start_date);
          if (k >= 0 && addMonths(s.start_date, k) === ps) paid.set(`${s.id}|${addMonths(v, k)}`, at);
        }
        changes.push(`开始收费日 ${cn(s.start_date)} → ${cn(v)}`); s.start_date = v;
      }
    }
    if (body.region !== undefined) { const v = region(body.region); if (v !== s.region) { changes.push(`地区 ${s.region} → ${v}`); s.region = v; } }
    if (body.note !== undefined) { const v = note(body.note); if (v !== s.note) { changes.push('改了备注'); s.note = v; } }
    if (body.end_date !== undefined) {
      if (body.end_date === null) {
        if (!s.manual) throw bad('只有手动添加的计费节点能恢复；系统节点回到团队后会自动计费', 409, 'conflict');
        s.end_date = null; changes.push('恢复收费');
      } else {
        if (s.holder || (!s.manual && !s.end_date)) throw bad('节点还在团队里；不收这个节点的钱请把节点设为不计费', 409, 'conflict');
        const v = date(body.end_date, '停止收费日');
        if (v <= s.start_date) throw bad(`停止收费日 ${v} 要晚于开始收费日 ${s.start_date}`);
        if (!s.manual && v > TODAY) throw bad('系统节点的停止收费日不能晚于今天');
        s.end_date = v;
        ev(s.team_id, s.id, new Date().toISOString(), 'manual_end', name, null, `${name} 停止收费`);
      }
    }
    if (changes.length) ev(s.team_id, s.id, new Date().toISOString(), 'edit', name, null, `${name}：${changes.join('，')}`);
    billing(s.team_id);
    return { id: s.id };
  });
  R('DELETE', '/billing-slots/([^/]+)', ({ params }) => {
    const s = slot(params[0]);
    if (!s.manual) throw bad('只有手动添加的计费节点能删除；系统节点离开团队后由对账自动结束，也可以把节点设为不计费', 409, 'conflict');
    slots.splice(slots.indexOf(s), 1);
    billing(s.team_id);
    return { id: s.id };
  });
  R('PUT', '/machines/([^/]+)/billing-exempt', ({ params, body }) => {
    const m = o.machines().find((x) => x.id === params[0]); if (!m) throw bad('没有这台机器', 404, 'not_found');
    if (typeof body.exempt !== 'boolean') throw bad('exempt 要是 true / false');
    if (m.billing_exempt === body.exempt) return { machine_id: m.id, exempt: body.exempt };
    m.billing_exempt = body.exempt;
    const now = new Date().toISOString();
    const held = slots.find((s) => s.holder?.id === m.id && !s.end_date);
    const tid = m.team?.id ?? held?.team_id;
    if (tid) ev(tid, held?.id ?? null, now, 'exempt', m.name, null, `${m.name} ${body.exempt ? '设为不收费' : '恢复收费'}`);
    // 设完服务端马上对账：不计费的名额今天结束（本期照收）；恢复的在本期有效期尚未结束时接回原名额，否则今天新开始收费（mock 没有订阅）
    if (body.exempt && held) {
      held.last_holder_name = held.holder!.name; held.holder = null; held.end_date = TODAY;
      ev(held.team_id, held.id, now, 'remove', m.name, null, `${m.name} 离开，${TODAY > held.start_date ? `收费到 ${cn(addDays(paidTo(held.start_date, TODAY), -1))}` : '没有产生收费期次'}`);
    }
    if (!body.exempt && m.team && !held) {
      const back = slots.find((s) => s.team_id === m.team!.id && !s.manual && !s.holder && s.last_holder_name === m.name && s.end_date && s.end_date > s.start_date && paidTo(s.start_date, s.end_date) > TODAY);
      if (back) { back.holder = { id: m.id, name: m.name }; back.end_date = null; } else {
        const s: MSlot = { ...blank, id: nid('sl'), team_id: m.team.id, region: '美区', start_date: TODAY, holder: { id: m.id, name: m.name } };
        slots.push(s);
        ev(s.team_id, s.id, now, 'add', m.name, null, `${m.name} 加入，从 ${cn(TODAY)}开始收费`);
      }
    }
    if (tid) billing(tid);
    return { machine_id: m.id, exempt: body.exempt };
  });

  R('POST', '/teams/([^/]+)/customer-page', ({ params }) => {
    const t = team(params[0]); const now = new Date().toISOString();
    const old = pages.get(t.id);
    pages.set(t.id, { token: token32(), password: token32().slice(0, 16), enabled: old?.enabled ?? true, created_at: old?.created_at ?? now, token_at: now, last_view_at: null });
    return pageView(t.id);
  });
  R('PATCH', '/teams/([^/]+)/customer-page', ({ params, body }) => {
    const p = pages.get(team(params[0]).id); if (!p) throw bad('这个团队还没有客户页链接', 404, 'not_found');
    if (typeof body.enabled !== 'boolean') throw bad('enabled 要是 true / false');
    p.enabled = body.enabled;
    return pageView(params[0]);
  });
  R('DELETE', '/teams/([^/]+)/customer-page', ({ params }) => { const t = team(params[0]); pages.delete(t.id); return { team_id: t.id }; });

  R('POST', '/public/customer/([^/]+)', ({ params, query, body }) => {
    const asked = monthQ(query.get('month'));
    const month = asked;
    const hit = [...pages.entries()].find(([, p]) => p.token === params[0] && p.enabled && p.password && Date.now() < Date.parse(p.token_at) + 3600000);
    if (!hit) throw bad('链接不对或已停用', 404, 'not_found');
    const [tid, p] = hit;
    if (body.password !== p.password) throw bad('查看密码不正确', 403, 'forbidden');
    if (!p.last_view_at || Date.now() - Date.parse(p.last_view_at) > 600_000) p.last_view_at = new Date().toISOString();
    const nodes = rows(tid, month);
    const out: PublicCustomer = {
      expires_at: new Date(Date.parse(p.token_at) + 3600000).toISOString(), team: team(tid).name, today: TODAY, month, months: monthsOf(tid),
      outstanding: monthsOf(tid).map(month => ({ month, count: rows(tid, month).filter(n => !n.paid && !n.upcoming).length })).filter(m => m.count > 0),
      nodes: nodes.map((n) => ({ name: n.name, region: n.region, period_start: n.period_start, period_end: n.period_end, paid: n.paid, manual: n.manual, ended: n.ended })),
    };
    return out;
  });
}
