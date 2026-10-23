// 操作记录列表（抽屉「最近操作」与「操作记录」页共用）：动作 + 对象，右边是谁、什么时候；下一行是改了什么（标题、摘要由中控按 detail 拼好）。
// 远程执行点一下原地展开命令、结果、耗时；对象名单独是链接，点了打开那台机器的抽屉。
import * as React from 'react';
import { ChevronRight } from 'lucide-react';
import type { AuditEntry } from '@/lib/types';
import * as F from '@/lib/format';
import { cn } from '@/lib/utils';
import { Pill } from './primitives';

type Obj = Record<string, unknown>;
/** 一条记录在界面上的样子。more：展开后的明细（标签、内容）；res：执行结果（失败、非 0 退出码着色）；machine：对象名对应的机器 id（可点开抽屉） */
interface Desc { title: string; target: string; rest: string; res?: { text: string; tone?: 'warn' | 'bad' | 'info' }; more: [string, string][]; machine?: string }

const obj = (v: unknown): Obj => (v && typeof v === 'object' ? (v as Obj) : {});
const str = (v: unknown) => (typeof v === 'string' ? v : '');
/** 远程执行超过这么久还没补上结果（中控中途重启、连接断了），就当结果未知：单次最长 600 秒，排队 30 秒 */
const EXEC_STALE_MS = 15 * 60_000;
/** 对象是机器的记录：多数写 machine，SSH 登录写的是机器类型 */
const MACHINE_KINDS = new Set(['machine', 'vps', 'relay', 'proxy']);
const secs = (ms: number) => (ms < 60_000 ? `${(ms / 1000).toFixed(ms < 10_000 ? 1 : 0)} 秒` : `${Math.floor(ms / 60_000)} 分 ${Math.round((ms % 60_000) / 1000)} 秒`);

function describe(a: AuditEntry): Desc {
  const d = obj(a.detail);
  // 记录里的显示名接在动作后面：摘要以它开头时（上架、执行命令、改套餐、换线路机…）从摘要里拿掉，其余是详情；
  // 对象是机器、摘要不以名字开头（改名的摘要是「旧 → 新」）时名字照样接上，摘要整句留作详情。名字保持记录时的样子，之后改过名也不跟着变。
  // 静默 / 维护窗口的对象是 silence，名字与机器 id 在 subject_name / subject_id（全部机器的没有 id，不可点）
  const silence = a.target_kind === 'silence';
  const name = str(silence ? d.subject_name : d.name);
  const machine = silence ? str(d.subject_id) || undefined : MACHINE_KINDS.has(a.target_kind ?? '') && a.target_id ? a.target_id : undefined;
  const base: Desc = name && a.summary === name ? { title: a.title, target: name, rest: '', more: [], machine }
    : name && a.summary.startsWith(`${name} · `) ? { title: a.title, target: name, rest: a.summary.slice(name.length + 3), more: [], machine }
      : { title: a.title, target: name && machine ? name : '', rest: a.summary, more: [], machine };
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

const isToday = (iso: string) => new Date(iso).toDateString() === new Date().toDateString();

/** onOpen：给了就让涉及机器的记录里的对象名可点（打开这台机器的抽屉）；抽屉里自己的记录不需要。
 *  byDay：列表已按天分段（操作记录页）：写几点几分，今天的再加「多久前」；哪天在段标题里 */
export function AuditList({ items, onOpen, byDay }: { items: AuditEntry[]; onOpen?: (id: string) => void; byDay?: boolean }) {
  const [open, setOpen] = React.useState<Set<string>>(new Set());
  const toggle = (id: string) => setOpen((s) => { const n = new Set(s); if (n.has(id)) n.delete(id); else n.add(id); return n; });
  return (
    <ul className="audit-list is-bare">
      {items.map((a) => {
        const p = describe(a);
        const link = onOpen && p.machine && p.target ? p.machine : null;
        const can = p.more.length > 0;
        const on = can && open.has(a.id);
        // 审计要看具体时刻：按天分段的页面写「14:52」，今天的再跟「4 小时前」（日期在段标题里）；抽屉里不分段，只写多久前（悬停看完整时间）
        const when = byDay ? (isToday(a.ts) ? `${F.hm(a.ts)} · ${F.ago(a.ts)}` : F.hm(a.ts)) : F.ago(a.ts);
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
