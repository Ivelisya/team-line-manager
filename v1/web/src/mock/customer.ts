// mock 的计费台账（同服务端规格：计费名额、待定节点、增减记录、每一期已收 / 没收）与客户页（公开 /public/customer/{token}）。
// 口径同 batches.rs：每个节点从开始收费日起每满一个月收一次（一期，月底对齐）；按月看 = 每个节点起点在这个月的那一期；已收按 (名额, 期起点) 记。
// 静态数据 + 写操作改内存，不模拟每分钟的对账。数据是生产的真实形状：团队1 九个 10-01 开始（团队1-01..09）+ 团队1-10 10-30 开始；
// 团队2 十九个 10-01 开始，10-28 有两次替换，另有一个刚加入、还在待定的节点。10 月除团队1-10 外都已收，11 月团队2 收了 5 个。
// 「今天」固定 2026-11-03；公开页的演示令牌是 demo。
import type { BillingEvent, BillingEventKind, BillingMonth, BillingMonthNode, CustomerPage, MachineView, PublicCustomer, Team } from '@/lib/types';
import { addMonths } from '@/lib/format';

type Reg = (method: string, path: string, h: (c: { params: string[]; query: URLSearchParams; body: any }) => unknown) => void; // eslint-disable-line @typescript-eslint/no-explicit-any
type Bad = (msg: string, status?: number, code?: string) => Error;

const TODAY = '2026-11-03';
const GRACE = 86_400_000;
interface MSlot { id: string; team_id: string; region: string; start_date: string; holder: { id: string; name: string } | null; last_holder_name: string | null; vacated_at: string | null; end_date: string | null; manual: boolean; note: string }
interface MPending { team_id: string; machine: { id: string; name: string }; seen_at: string }
interface MEvent extends BillingEvent { team_id: string }
interface MPage { token: string; enabled: boolean; created_at: string; token_at: string; last_view_at: string | null }

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
const pending: MPending[] = [];
const events: MEvent[] = [];
/** 已收：`名额 id|期起点` → 标记时刻 */
const paid = new Map<string, string>();
const ev = (team_id: string, slot_id: string | null, at: string, kind: BillingEventKind, machine_name: string | null, other_name: string | null, text: string) => {
  events.push({ id: nid('ev'), team_id, slot_id, at, kind, machine_name, other_name, text });
};
const holder = (team: 't1' | 't2', n: number) => ({ id: `m-${team}-${p2(n)}`, name: `${team === 't1' ? '团队1' : '团队2'}-${p2(n)}` });
const blank = { holder: null, last_holder_name: null, vacated_at: null, end_date: null, manual: false, note: '' };
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
  pending.push({ team_id: 't2', machine: { id: 'm-t2-20', name: '团队2-20' }, seen_at: '2026-11-03T09:12:00+08:00' });
}
// 各种状态的样例（只为界面覆盖，不是生产形状）：手动添加的、刚离开等补位的、10 月中停止收费的、未标地区、31 号开始收费
{
  const add = (team_id: 't1' | 't2', region: string, start_date: string, o: Partial<MSlot>) => slots.push({ ...blank, id: nid('sl'), team_id, region, start_date, ...o });
  add('t1', '墨西哥', '2026-10-15', { manual: true, note: '墨西哥家宽' });
  add('t1', '美区', '2026-10-01', { last_holder_name: '团队1-11', end_date: '2026-10-20' });
  add('t1', '美区', '2026-10-01', { last_holder_name: '团队1-12', vacated_at: '2026-11-02T22:00:00Z' });
  add('t2', '未知', '2026-10-12', { holder: { id: 'm-t2-21', name: '团队2-21' } });
  add('t2', '美区', '2026-08-31', { holder: { id: 'm-t2-22', name: '团队2-22' } });
}
// 10 月：除团队1-10 外都已收；11 月：团队2 收了前 5 个
for (const s of slots) {
  const p = periodIn(s, '2026-10');
  if (p && s.holder?.name !== '团队1-10') paid.set(`${s.id}|${p[0]}`, '2026-10-31T12:00:00+08:00');
}
for (const s of slots.filter((x) => x.team_id === 't2').slice(0, 5)) paid.set(`${s.id}|${periodIn(s, '2026-11')![0]}`, '2026-11-02T10:30:00+08:00');
const pages = new Map<string, MPage>([['t2', { token: 'demo', enabled: true, created_at: '2026-10-20T10:12:00Z', token_at: '2026-10-20T10:12:00Z', last_view_at: '2026-11-02T01:40:00Z' }]]);

// ---------- 口径 ----------
/** 起点落在 month（YYYY-MM）那个月、要收费的那一期 [起点, 终点)；没有为 null */
function periodIn(s: MSlot, month: string): [string, string] | null {
  const k = monthNo(month) - monthNo(s.start_date);
  if (k < 0) return null;
  const ps = addMonths(s.start_date, k);
  return !s.end_date || ps < s.end_date ? [ps, addMonths(s.start_date, k + 1)] : null;
}
const slotName = (s: MSlot) => (s.manual ? s.note || '手动添加的节点' : s.holder?.name ?? s.last_holder_name ?? '（已删除的节点）');
const vacated = (s: MSlot) => !s.manual && !s.holder && (!s.end_date || s.end_date > TODAY);
const ended = (s: MSlot) => !!s.end_date && s.end_date <= TODAY;
/** 可以翻看的月份：最早的开始收费月 → 下个月 */
function monthsOf(tid: string) {
  const last = addMonths(`${TODAY.slice(0, 7)}-01`, 1).slice(0, 7);
  let m = slots.filter((s) => s.team_id === tid).reduce((a, s) => (s.start_date.slice(0, 7) < a ? s.start_date.slice(0, 7) : a), TODAY.slice(0, 7));
  const out: string[] = [];
  while (m <= last) { out.push(m); m = addMonths(`${m}-01`, 1).slice(0, 7); }
  return out;
}
/** 某月清单：没收在前、再按名字 */
function rows(tid: string, month: string): BillingMonthNode[] {
  return slots.filter((s) => s.team_id === tid).flatMap((s) => {
    const p = periodIn(s, month);
    if (!p) return [];
    const at = paid.get(`${s.id}|${p[0]}`) ?? null;
    return [{
      slot_id: s.id, name: slotName(s), region: s.region, start_date: s.start_date, end_date: s.end_date, period_start: p[0], period_end: p[1], upcoming: p[0] > TODAY,
      manual: s.manual, note: s.note, holder_id: s.holder?.id ?? null, vacated: vacated(s), ended: ended(s), paid: !!at, paid_at: at,
    }];
  }).sort((a, b) => Number(a.paid) - Number(b.paid) || a.name.localeCompare(b.name, 'zh-CN', { numeric: true }));
}
const eventView = ({ team_id: _t, ...e }: MEvent): BillingEvent => e; // eslint-disable-line @typescript-eslint/no-unused-vars
const teamEvents = (tid: string) => events.filter((e) => e.team_id === tid).sort((a, b) => Date.parse(b.at) - Date.parse(a.at) || cmp(b.id, a.id));

export function installCustomerMock(R: Reg, o: { teams: () => Team[]; machines: () => MachineView[]; bad: Bad; emit: (e: { type: string; id?: string }) => void }) {
  const { bad } = o;
  const billing = (tid: string) => o.emit({ type: 'billing', id: tid });
  const team = (id: string) => { const t = o.teams().find((x) => x.id === id); if (!t) throw bad('没有这个团队', 404, 'not_found'); return t; };
  const slot = (id: string) => { const s = slots.find((x) => x.id === id); if (!s) throw bad('没有这个计费节点', 404, 'not_found'); return s; };
  const pageView = (tid: string): CustomerPage | null => {
    const p = pages.get(tid);
    return p ? { enabled: p.enabled, link: `${location.origin}/c/${p.token}`, created_at: p.created_at, token_at: p.token_at, last_view_at: p.last_view_at } : null;
  };
  const region = (v: unknown) => { const s = String(v ?? '').trim(); if (!s || [...s].length > 20) throw bad('地区要 1–20 个字，如 美区、墨西哥'); return s; };
  const note = (v: unknown) => { const s = String(v ?? '').trim(); if ([...s].length > 60) throw bad('备注最多 60 个字'); return s; };
  const date = (v: unknown, what: string) => { if (!validDate(v)) throw bad(`${what}要写成 2026-10-31 这样的日期`); return v; };
  const monthQ = (v: string | null) => { if (v == null || v === '') return TODAY.slice(0, 7); if (!MONTH.test(v)) throw bad('月份要写成 2026-11 这样'); return v; };
  const view = (tid: string, month: string): BillingMonth => {
    const t = team(tid);
    const nodes = rows(tid, month);
    const pend = month === TODAY.slice(0, 7) ? pending.filter((p) => p.team_id === tid) : [];
    return {
      team: { id: t.id, name: t.name }, today: TODAY, initialized: true, page: pageView(tid), month, months: monthsOf(tid), nodes,
      paid_count: nodes.filter((n) => n.paid).length, unpaid_count: nodes.filter((n) => !n.paid).length,
      pending: pend.map((p) => ({ machine: p.machine, seen_at: p.seen_at, decide_at: new Date(Date.parse(p.seen_at) + GRACE).toISOString() })),
    };
  };

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
      ev(t.id, null, new Date().toISOString(), body.paid ? 'paid' : 'unpaid', null, null, `${Number(body.month.slice(5))} 月${body.paid ? '已收' : '改回没收'}：${names.slice(0, 10).join('、')}${names.length > 10 ? ' 等' : ''}（共 ${names.length} 个）`);
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
    const start = date(body.start_date, '开始收费日'); const r = region(body.region); const n = note(body.note);
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
    if (body.start_date !== undefined) { const v = date(body.start_date, '开始收费日'); if (v !== s.start_date) { changes.push(`开始收费日 ${cn(s.start_date)} → ${cn(v)}`); s.start_date = v; } }
    if (body.region !== undefined) { const v = region(body.region); if (v !== s.region) { changes.push(`地区 ${s.region} → ${v}`); s.region = v; } }
    if (body.note !== undefined) { const v = note(body.note); if (v !== s.note) { changes.push('改了备注'); s.note = v; } }
    if (body.end_date !== undefined) {
      if (body.end_date === null) {
        if (!s.manual) throw bad('只有手动添加的计费节点能恢复；系统节点回到团队后会自动计费', 409, 'conflict');
        s.end_date = null; changes.push('恢复收费');
      } else {
        if (s.holder) throw bad('节点还在团队里；不收这个节点的钱请把节点设为不计费', 409, 'conflict');
        const v = date(body.end_date, '停止收费日');
        if (v <= s.start_date) throw bad(`停止收费日 ${v} 要晚于开始收费日 ${s.start_date}`);
        if (!s.manual && v > TODAY) throw bad('等补位的节点停止收费日不能晚于今天');
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
    // 设不计费后服务端马上对账：名额立刻空出（之后照样走 24 小时配对）
    if (body.exempt && held) { held.last_holder_name = held.holder!.name; held.holder = null; held.vacated_at = now; }
    const pi = pending.findIndex((p) => p.machine.id === m.id);
    if (body.exempt && pi >= 0) pending.splice(pi, 1);
    if (!body.exempt && m.team && !held && pi < 0) pending.push({ team_id: m.team.id, machine: { id: m.id, name: m.name }, seen_at: now });
    if (tid) { ev(tid, held?.id ?? null, now, 'exempt', m.name, null, `${m.name} ${body.exempt ? '设为不收费' : '恢复收费'}`); billing(tid); }
    return { machine_id: m.id, exempt: body.exempt };
  });

  R('POST', '/teams/([^/]+)/customer-page', ({ params }) => {
    const t = team(params[0]); const now = new Date().toISOString();
    const old = pages.get(t.id);
    pages.set(t.id, { token: token32(), enabled: old?.enabled ?? true, created_at: old?.created_at ?? now, token_at: now, last_view_at: null });
    return pageView(t.id);
  });
  R('PATCH', '/teams/([^/]+)/customer-page', ({ params, body }) => {
    const p = pages.get(team(params[0]).id); if (!p) throw bad('这个团队还没有客户页链接', 404, 'not_found');
    if (typeof body.enabled !== 'boolean') throw bad('enabled 要是 true / false');
    p.enabled = body.enabled;
    return pageView(params[0]);
  });
  R('DELETE', '/teams/([^/]+)/customer-page', ({ params }) => { const t = team(params[0]); pages.delete(t.id); return { team_id: t.id }; });

  R('GET', '/public/customer/([^/]+)', ({ params, query }) => {
    const month = monthQ(query.get('month'));
    const hit = [...pages.entries()].find(([, p]) => p.token === params[0] && p.enabled);
    if (!hit) throw bad('链接不对或已停用', 404, 'not_found');
    const [tid, p] = hit;
    if (!p.last_view_at || Date.now() - Date.parse(p.last_view_at) > 600_000) p.last_view_at = new Date().toISOString();
    const nodes = rows(tid, month);
    const out: PublicCustomer = {
      team: team(tid).name, today: TODAY, month, months: monthsOf(tid),
      nodes: nodes.map((n) => ({ name: n.name, region: n.region, period_start: n.period_start, period_end: n.period_end, paid: n.paid })),
      paid_count: nodes.filter((n) => n.paid).length, unpaid_count: nodes.filter((n) => !n.paid).length,
    };
    return out;
  });
}
