// 操作记录列表（抽屉「最近操作」与「操作记录」页共用）：动作 + 对象，右边是谁、多久以前；下一行是详情（等宽，悬停看全文）。
// 登录失败的圆点标红，远程执行还没补上结果的（执行中或中控中途重启）标「执行中」。
import type { AuditEntry } from '@/lib/types';
import * as F from '@/lib/format';
import { cn } from '@/lib/utils';
import { Pill } from './primitives';

/** 一条操作记录拆成对象与详情：摘要以记录里的显示名开头时（上架、执行命令、换线路机…），名字接在动作后面，其余是详情。
 *  名字保持记录时的样子，之后改过名也不跟着变 */
function auditParts(a: AuditEntry): { target: string; rest: string } {
  const name = (a.detail as { name?: unknown } | null)?.name;
  if (typeof name !== 'string' || !name) return { target: '', rest: a.summary };
  if (a.summary === name) return { target: name, rest: '' };
  if (a.summary.startsWith(`${name} · `)) return { target: name, rest: a.summary.slice(name.length + 3) };
  return { target: '', rest: a.summary };
}
const started = (a: AuditEntry) => !!(a.detail as { started?: unknown } | null)?.started;

/** onOpen：给了就让涉及机器的一行可点（打开这台机器的抽屉）；抽屉里自己的记录不需要 */
export function AuditList({ items, onOpen }: { items: AuditEntry[]; onOpen?: (id: string) => void }) {
  return (
    <ul className="audit-list">
      {items.map((a) => {
        const p = auditParts(a);
        const open = onOpen && a.target_kind === 'machine' && a.target_id ? () => onOpen(a.target_id!) : undefined;
        return (
          <li key={a.id} className={cn(open && 'is-link')} role={open ? 'button' : undefined} tabIndex={open ? 0 : undefined}
            onClick={open} onKeyDown={open && ((e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); open(); } })}>
            <span className="dot" data-tone={a.action === 'auth.login_failed' ? 'bad' : 'neutral'} />
            <div className="audit-body">
              <div className="audit-head">
                <span className="audit-what"><b>{a.title}</b>{p.target && <> {p.target}</>}{started(a) && <> <Pill tone="info" dot="pulse">执行中</Pill></>}</span>
                <span className="audit-meta">{F.actor(a.actor)} · <span data-tip={F.dtFull(a.ts)}>{F.ago(a.ts)}</span></span>
              </div>
              {p.rest && <div className="audit-detail mono" data-tip={p.rest}>{p.rest}</div>}
            </div>
          </li>
        );
      })}
    </ul>
  );
}
