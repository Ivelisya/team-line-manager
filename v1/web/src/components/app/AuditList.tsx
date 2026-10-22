// 操作记录列表（抽屉「最近操作」与「操作记录」页共用）：动作 + 对象，右边是谁、什么时候；下一行是改了什么。
// 有更多内容的（远程执行的命令与结果、改动前后）点一下原地展开；对象名单独是链接，点了打开那台机器的抽屉。
// 服务端只给了动作名的（修改套餐、修改资料、升级 agent…）在这里按 detail 里的字段拼出对象和改动。
import * as React from 'react';
import { ChevronRight } from 'lucide-react';
import type { AuditEntry } from '@/lib/types';
import { useMachineBriefs, usePlans, useProviders, useTeams } from '@/lib/queries';
import { cycleLabel, trafficResetLabel, trafficRuleLabel } from '@/lib/derive';
import { countryName } from '@/lib/flags';
import { tzLabel } from '@/lib/tz';
import * as F from '@/lib/format';
import { cn } from '@/lib/utils';
import { Pill } from './primitives';

type Obj = Record<string, unknown>;
type Names = { plan: (id: string) => { name: string; currency: string } | undefined; team: (id: string) => string | undefined; provider: (id: string) => string | undefined; machine: (id: string) => string | undefined };
/** 一条记录在界面上的样子。more：展开后的明细（标签、内容）；res：执行结果（失败、非 0 退出码着色） */
interface Desc { title: string; target: string; rest: string; res?: { text: string; tone?: 'warn' | 'bad' | 'info' }; more: [string, string][]; mono?: boolean }

const obj = (v: unknown): Obj => (v && typeof v === 'object' ? (v as Obj) : {});
const str = (v: unknown) => (typeof v === 'string' ? v : '');
/** 远程执行超过这么久还没补上结果（中控中途重启、连接断了），就当结果未知：单次最长 600 秒，排队 30 秒 */
const EXEC_STALE_MS = 15 * 60_000;
const secs = (ms: number) => (ms < 60_000 ? `${(ms / 1000).toFixed(ms < 10_000 ? 1 : 0)} 秒` : `${Math.floor(ms / 60_000)} 分 ${Math.round((ms % 60_000) / 1000)} 秒`);

/** 改动前后：只列变了的字段，「价格 $5.00 → $6.00」 */
type Fields = Record<string, [string, (v: unknown) => string]>;
const show = (v: unknown) => (v == null || v === '' ? '—' : String(v));
function diff(d: Obj, fields: Fields): [string, string][] {
  const b = obj(d.before); const a = obj(d.after);
  return Object.entries(fields).flatMap(([k, [label, fmt]]) => (k in a && JSON.stringify(a[k]) !== JSON.stringify(b[k]) ? [[label, `${fmt(b[k])} → ${fmt(a[k])}`] as [string, string]] : []));
}
const restOf = (lines: [string, string][]) => lines.map(([k, v]) => `${k} ${v}`).join('；');

function describe(a: AuditEntry, n: Names): Desc {
  const d = obj(a.detail);
  const id = a.target_id ?? '';
  // 摘要以记录里的显示名开头时（上架、执行命令、换线路机…），名字接在动作后面，其余是详情。名字保持记录时的样子，之后改过名也不跟着变
  const name = str(d.name);
  const base: Desc = name && a.summary === name ? { title: a.title, target: name, rest: '', more: [] }
    : name && a.summary.startsWith(`${name} · `) ? { title: a.title, target: name, rest: a.summary.slice(name.length + 3), more: [] }
      : { title: a.title, target: '', rest: a.summary, more: [] };
  switch (a.action) {
    case 'plan.update': {
      const p = n.plan(id); const after = obj(d.after);
      const target = p?.name ?? str(after.name);
      // 只改了归档：写成「归档套餐 X」「取消归档套餐 X」
      if (Object.keys(after).length === 1 && 'archived_at' in after) return { ...base, title: after.archived_at ? '归档套餐' : '取消归档套餐', target, rest: '' };
      const cur = str(after.currency) || p?.currency || 'USD';
      const lines = diff(d, {
        name: ['套餐名', show], provider_id: ['供应商', (v) => n.provider(str(v)) ?? '—'], cpu_cores: ['CPU', (v) => (v == null ? '—' : `${v} 核`)],
        mem_mb: ['内存', (v) => (typeof v === 'number' ? F.str(F.mem(v * 1024 * 1024)) : '—')], disk_gb: ['磁盘', (v) => (v == null ? '—' : `${v} GiB`)],
        bandwidth_mbps: ['带宽', (v) => (typeof v === 'number' ? F.str(F.mbps(v)) : '—')], traffic_gb: ['流量', (v) => (v ? `${v} GB` : '不限')],
        price_cents: ['价格', (v) => (typeof v === 'number' ? F.money(v, cur) : '—')], currency: ['币种', show], cycle: ['计费周期', (v) => cycleLabel(str(v))],
        notes: ['备注', show], archived_at: ['状态', (v) => (v ? '已归档' : '在用')],
      });
      return { ...base, target, rest: restOf(lines), more: lines };
    }
    case 'team.update': {
      const lines = diff(d, { name: ['团队名', show], sort_order: ['排序', show] });
      return { ...base, title: '修改团队', target: str(obj(d.after).name) || n.team(id) || '', rest: restOf(lines), more: lines };
    }
    case 'provider.update': {
      const lines = diff(d, {
        name: ['名称', show], code: ['代码', show], panel_url: ['面板地址', show], timezone: ['账单时区', (v) => (v ? tzLabel(str(v)) : '跟随组织')],
        traffic_rule: ['流量计量', (v) => trafficRuleLabel(str(v))], traffic_reset: ['流量重置', (v) => (v ? trafficResetLabel(str(v)) : '—')],
        remind_days: ['到期提醒', (v) => (v == null ? '—' : `提前 ${v} 天`)],
      });
      return { ...base, target: str(obj(d.after).name) || base.rest, rest: restOf(lines), more: lines };
    }
    case 'machine.update': case 'machine.rename': {
      const lines = diff(d, {
        team_id: ['团队', (v) => (v ? n.team(str(v)) ?? '—' : '—')], number: ['编号', (v) => (typeof v === 'number' ? F.pad2(v) : '—')],
        label: ['线路机名称', show], country: ['国家', (v) => (v ? countryName(str(v)) : '自动')],
      });
      return { ...base, title: a.action === 'machine.rename' ? '改名' : '修改资料', target: n.machine(id) ?? base.target, rest: restOf(lines) || base.rest, more: lines };
    }
    case 'subscription.set': {
      const lines: [string, string][] = [['到期日', show(d.due_date)]];
      if (typeof d.auto_renew === 'boolean') lines.push(['自动续费', d.auto_renew ? '开' : '关']);
      if ('traffic_used_gb' in d) lines.push(['按面板校准已用', d.traffic_used_gb == null ? '清除' : `${d.traffic_used_gb} GB`]);
      return { ...base, target: n.machine(id) ?? '', rest: restOf(lines), more: lines };
    }
    case 'agent.upgrade': {
      const skipped = Number(d.skipped) || 0;
      return { ...base, rest: `${Number(d.count) || 0} 台${skipped ? `，跳过 ${skipped} 台` : ''}` };
    }
    case 'exec': case 'machine.logs': {
      if (a.action === 'machine.logs') return base;
      const cmd = str(d.command);
      const code = typeof d.exit_code === 'number' ? d.exit_code : null;
      const res: Desc['res'] = d.started
        ? (Date.now() - Date.parse(a.ts) > EXEC_STALE_MS ? { text: '结果未知', tone: 'warn' } : { text: '执行中', tone: 'info' })
        : d.timed_out ? { text: '超时', tone: 'warn' } : code != null ? (code === 0 ? { text: '退出码 0' } : { text: `退出码 ${code}`, tone: 'warn' }) : { text: '失败', tone: 'bad' };
      const more: [string, string][] = [['命令', cmd], ['结果', d.error ? `${res.text}：${str(d.error)}` : res.text]];
      if (typeof d.duration_ms === 'number') more.push(['耗时', secs(d.duration_ms)]);
      return { ...base, rest: cmd.split('\n')[0], res, more, mono: true };
    }
    default:
      return base;
  }
}

/** 记录里对象的现名：套餐、团队、供应商、机器（各一份缓存的列表，抽屉里用也不多发请求） */
function useNames(): Names {
  const plans = usePlans().data; const teams = useTeams().data; const providers = useProviders().data; const machines = useMachineBriefs().data;
  return React.useMemo(() => ({
    plan: (id) => plans?.find((p) => p.id === id),
    team: (id) => teams?.find((t) => t.id === id)?.name,
    provider: (id) => providers?.find((p) => p.id === id)?.name,
    machine: (id) => machines?.find((m) => m.id === id)?.name,
  }), [plans, teams, providers, machines]);
}

/** 一条记录显示出来的文字（动作、对象、改了什么、操作人），操作记录页的搜索按它匹配 */
export function useAuditText() {
  const names = useNames();
  return React.useCallback((a: AuditEntry) => { const p = describe(a, names); return `${p.title} ${p.target} ${p.rest} ${p.res?.text ?? ''} ${F.actor(a.actor)}`; }, [names]);
}

const p2 = F.pad2;
/** 对象是机器的记录：多数写 machine，SSH 登录写的是机器类型 */
const MACHINE_KINDS = new Set(['machine', 'vps', 'relay', 'proxy']);
const isToday = (iso: string) => new Date(iso).toDateString() === new Date().toDateString();

/** onOpen：给了就让涉及机器的记录里的对象名可点（打开这台机器的抽屉）；抽屉里自己的记录不需要。
 *  byDay：列表已按天分段（操作记录页）：今天的写「多久前」，更早的段标题已经写了哪天，这里只写几点几分 */
export function AuditList({ items, onOpen, byDay }: { items: AuditEntry[]; onOpen?: (id: string) => void; byDay?: boolean }) {
  const names = useNames();
  const [open, setOpen] = React.useState<Set<string>>(new Set());
  const toggle = (id: string) => setOpen((s) => { const n = new Set(s); if (n.has(id)) n.delete(id); else n.add(id); return n; });
  return (
    <ul className="audit-list is-bare">
      {items.map((a) => {
        const p = describe(a, names);
        const link = onOpen && MACHINE_KINDS.has(a.target_kind ?? '') && a.target_id && p.target ? a.target_id : null;
        const can = p.more.length > 0;
        const on = can && open.has(a.id);
        const d = new Date(a.ts);
        const when = byDay && !isToday(a.ts) ? `${p2(d.getHours())}:${p2(d.getMinutes())}` : F.ago(a.ts);
        return (
          <li key={a.id} className={cn(can && 'is-link')} data-open={on ? '' : undefined} role={can ? 'button' : undefined} tabIndex={can ? 0 : undefined} aria-expanded={can ? on : undefined}
            onClick={can ? () => toggle(a.id) : undefined} onKeyDown={can ? (e) => { if (e.target === e.currentTarget && (e.key === 'Enter' || e.key === ' ')) { e.preventDefault(); toggle(a.id); } } : undefined}>
            <div className="audit-body">
              <div className="audit-head">
                <span className="audit-what" data-tone={a.action === 'auth.login_failed' ? 'bad' : undefined}>
                  {/* 展开箭头的位置每行都留着：可展开、不可展开的行文字对齐 */}
                  <span className="audit-chev" aria-hidden>{can && <ChevronRight />}</span>
                  <b>{p.title}</b>
                  {p.target && <> {link ? <button type="button" className="btn btn-link audit-target" onClick={(e) => { e.stopPropagation(); onOpen!(link); }}>{p.target}</button> : p.target}</>}
                  {p.res && (p.res.tone === 'info' ? <> <Pill tone="info" dot="pulse">{p.res.text}</Pill></> : <span className="audit-res" data-tone={p.res.tone}>{p.res.text}</span>)}
                </span>
                <span className="audit-meta">{F.actor(a.actor)} · <span data-tip={F.dtFull(a.ts)}>{when}</span></span>
              </div>
              {p.rest && !on && <div className={cn('audit-detail', p.mono && 'mono')} data-tip={p.rest}>{p.rest}</div>}
              {on && (
                <dl className="audit-ex" onClick={(e) => e.stopPropagation()}>
                  {p.more.map(([k, v]) => <React.Fragment key={k}><dt>{k}</dt><dd className={cn(p.mono && k === '命令' && 'mono')}>{v}</dd></React.Fragment>)}
                  <dt>时间</dt><dd>{F.dtFull(a.ts)}</dd>
                </dl>
              )}
            </div>
          </li>
        );
      })}
    </ul>
  );
}
