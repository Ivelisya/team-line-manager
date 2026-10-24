// 告警的两个小件（告警页、顶栏告警预览、抽屉「最近告警」共用）：建议命令（中控给的 suggested，可复制）、近 7 天第几次。
import type { Incident } from '@/lib/types';
import { cn } from '@/lib/utils';
import { CopyButton } from './primitives';

/** 要列出的建议命令：已有快捷按钮的不重复——临时切换（quick_switch，「临时切到…」）、出口参数不一致的同步（「同步出口参数…」） */
const shownCmds = (i: Incident) => (i.suggested ?? []).filter((c) =>
  !(i.quick_switch?.length && c.startsWith('fleetctl router switch ')) && !(i.rule === 'router_exit_drift' && c.startsWith('fleetctl router sync ')));

/** 建议命令：等宽小字一行一条，后面一个复制按钮；没有就不渲染 */
export function SuggestedCmds({ i, className }: { i: Incident; className?: string }) {
  const cmds = shownCmds(i);
  if (!cmds.length) return null;
  return (
    <span className={cn('inc-cmds', className)}>
      {cmds.map((c) => <span key={c} className="inc-cmd"><code className="mono" data-tip={`建议执行：${c}`}>{c}</code><CopyButton text={c} label="命令" size={12} /></span>)}
    </span>
  );
}

/** 次数标签：进行中的（同组最多一条，就是最新那次）写「近 7 天第 N 次」，已恢复的写「近 7 天共 N 次」（recent_count 是整组的总数）。只在中控判定反复（chronic，次数含 30 分钟内复发合并进来的）时标；合并过的次数写在悬停里 */
export function RecentCount({ i, className }: { i: Incident; className?: string }) {
  if (!i.chronic) return null;
  const tip = [
    i.chronic && '同一对象同一规则近 7 天反复出现，考虑换线或替换',
    i.reopen_count > 0 && `这一条恢复后 30 分钟内又出现过 ${i.reopen_count} 次，已合并成这一条`,
  ].filter(Boolean).join('\n');
  return <span className={className} data-tip={tip}>{i.state === 'firing' ? `近 7 天第 ${i.recent_count ?? 1} 次` : `近 7 天共 ${i.recent_count ?? 1} 次`}</span>;
}
