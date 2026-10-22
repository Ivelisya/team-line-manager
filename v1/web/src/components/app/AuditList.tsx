// 操作记录列表（抽屉「最近操作」与「操作记录」页共用）：动作 + 对象，右边是谁、什么时候；下一行是改了什么（标题、摘要由中控按 detail 拼好）。
// 远程执行点一下原地展开命令、结果、耗时；对象名单独是链接，点了打开那台机器的抽屉。
import * as React from 'react';
import { ChevronRight } from 'lucide-react';
import type { AuditEntry } from '@/lib/types';
import * as F from '@/lib/format';
import { cn } from '@/lib/utils';
import { Pill } from './primitives';

type Obj = Record<string, unknown>;
/** 一条记录在界面上的样子。more：展开后的明细（标签、内容）；res：执行结果（失败、非 0 退出码着色） */
interface Desc { title: string; target: string; rest: string; res?: { text: string; tone?: 'warn' | 'bad' | 'info' }; more: [string, string][] }

const obj = (v: unknown): Obj => (v && typeof v === 'object' ? (v as Obj) : {});
const str = (v: unknown) => (typeof v === 'string' ? v : '');
/** 远程执行超过这么久还没补上结果（中控中途重启、连接断了），就当结果未知：单次最长 600 秒，排队 30 秒 */
const EXEC_STALE_MS = 15 * 60_000;
const secs = (ms: number) => (ms < 60_000 ? `${(ms / 1000).toFixed(ms < 10_000 ? 1 : 0)} 秒` : `${Math.floor(ms / 60_000)} 分 ${Math.round((ms % 60_000) / 1000)} 秒`);

function describe(a: AuditEntry): Desc {
  const d = obj(a.detail);
  // 摘要以记录里的显示名开头时（上架、执行命令、改套餐、换线路机…），名字接在动作后面，其余是详情。名字保持记录时的样子，之后改过名也不跟着变
  const name = str(d.name);
  const base: Desc = name && a.summary === name ? { title: a.title, target: name, rest: '', more: [] }
    : name && a.summary.startsWith(`${name} · `) ? { title: a.title, target: name, rest: a.summary.slice(name.length + 3), more: [] }
      : { title: a.title, target: '', rest: a.summary, more: [] };
  if (a.action !== 'exec') return base;
  // 远程执行：结果自己判断（中控摘要里的「执行中或中断」放久了就是结果未知），命令、结果、耗时进展开的明细
  const cmd = str(d.command);
  const code = typeof d.exit_code === 'number' ? d.exit_code : null;
  const res: Desc['res'] = d.started
    ? (Date.now() - Date.parse(a.ts) > EXEC_STALE_MS ? { text: '结果未知', tone: 'warn' } : { text: '执行中', tone: 'info' })
    : d.timed_out ? { text: '超时', tone: 'warn' } : code != null ? (code === 0 ? { text: '退出码 0' } : { text: `退出码 ${code}`, tone: 'warn' }) : { text: '失败', tone: 'bad' };
  const more: [string, string][] = [['命令', cmd], ['结果', d.error ? `${res.text}：${str(d.error)}` : res.text]];
  if (typeof d.duration_ms === 'number') more.push(['耗时', secs(d.duration_ms)]);
  return { ...base, rest: cmd.split('\n')[0], res, more };
}

const p2 = F.pad2;
/** 对象是机器的记录：多数写 machine，SSH 登录写的是机器类型 */
const MACHINE_KINDS = new Set(['machine', 'vps', 'relay', 'proxy']);
const isToday = (iso: string) => new Date(iso).toDateString() === new Date().toDateString();

/** onOpen：给了就让涉及机器的记录里的对象名可点（打开这台机器的抽屉）；抽屉里自己的记录不需要。
 *  byDay：列表已按天分段（操作记录页）：今天的写「多久前」，更早的段标题已经写了哪天，这里只写几点几分 */
export function AuditList({ items, onOpen, byDay }: { items: AuditEntry[]; onOpen?: (id: string) => void; byDay?: boolean }) {
  const [open, setOpen] = React.useState<Set<string>>(new Set());
  const toggle = (id: string) => setOpen((s) => { const n = new Set(s); if (n.has(id)) n.delete(id); else n.add(id); return n; });
  return (
    <ul className="audit-list is-bare">
      {items.map((a) => {
        const p = describe(a);
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
              {p.rest && !on && <div className={cn('audit-detail', a.action === 'exec' && 'mono')} data-tip={p.rest}>{p.rest}</div>}
              {on && (
                <dl className="audit-ex" onClick={(e) => e.stopPropagation()}>
                  {p.more.map(([k, v]) => <React.Fragment key={k}><dt>{k}</dt><dd className={cn(k === '命令' && 'mono')}>{v}</dd></React.Fragment>)}
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
