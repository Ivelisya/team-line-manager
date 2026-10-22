// 操作记录：全站的审计日志（登录与登录失败、API 密钥、远程执行、机器与入口变更、目录与告警设置），新→旧。
// 一次取 100 条，「加载更早的记录」按 before_id 往前翻；分类与搜索只筛已经加载的记录。涉及机器的一行点开该机器的抽屉。
import * as React from 'react';
import { History, SearchX } from 'lucide-react';
import { useAuditLog } from '@/lib/queries';
import { useDrawer } from '@/lib/drawer';
import type { AuditEntry } from '@/lib/types';
import { Button } from '@/components/ui/button';
import { SearchInput } from '@/components/ui/form';
import { Segmented } from '@/components/ui/overlays';
import { SkeletonRows } from '@/components/ui/skeleton';
import { Empty, ErrorNote, PageHead, Panel } from '@/components/app/primitives';
import { AuditList } from '@/components/app/AuditList';

type Cat = 'all' | 'auth' | 'exec' | 'machine' | 'config';
const CATS: { value: Cat; label: string }[] = [
  { value: 'all', label: '全部' }, { value: 'auth', label: '登录' }, { value: 'exec', label: '远程执行' },
  { value: 'machine', label: '机器与入口' }, { value: 'config', label: '设置与告警' },
];
/** 动作名归类：登录（auth.*）、远程执行（在机器上跑命令、取日志、恢复预检登录机器）、机器与入口（生命周期、改名、费用续费、入口、作业、升级 agent），其余是目录、密钥、通知、静默、告警、访问规则 */
function catOf(action: string): Exclude<Cat, 'all'> {
  if (action.startsWith('auth.')) return 'auth';
  if (action === 'exec' || action === 'machine.logs' || action === 'restore.probe') return 'exec';
  if (/^(machine|entry|relay|subscription|agent|job)\./.test(action) || action === 'renewal') return 'machine';
  return 'config';
}
const EMPTY: AuditEntry[] = [];

export function AuditPage() {
  const q = useAuditLog();
  const { open } = useDrawer();
  const [cat, setCat] = React.useState<Cat>('all');
  const [text, setText] = React.useState('');
  const all = React.useMemo(() => q.data?.pages.flat() ?? EMPTY, [q.data]);
  const rows = React.useMemo(() => {
    const t = text.trim().toLowerCase();
    return all.filter((a) => (cat === 'all' || catOf(a.action) === cat) && (!t || `${a.title} ${a.summary} ${a.actor}`.toLowerCase().includes(t)));
  }, [all, cat, text]);
  const filtered = cat !== 'all' || !!text.trim();

  return (
    <div className="stack">
      <PageHead title="操作记录" note="登录、API 密钥、远程执行与各项变更" tools={<Segmented label="分类" value={cat} onChange={setCat} items={CATS} />} />
      <div className="row" role="search">
        <SearchInput value={text} onChange={(e) => setText(e.target.value)} placeholder="搜索动作、对象、操作人…" aria-label="搜索" className="w-full sm:w-[300px]" />
      </div>
      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
      <Panel title="记录" count={q.data ? (filtered ? `${rows.length} / 已加载 ${all.length}` : `已加载 ${all.length}`) : undefined}
        sub={filtered ? '分类与搜索只在已加载的记录里筛' : '新的在前；涉及机器的一行点开看这台机器'}>
        {q.isLoading ? <SkeletonRows rows={6} />
          : rows.length ? <AuditList items={rows} onOpen={open} />
          : <Empty icon={filtered ? <SearchX size={22} /> : <History size={22} />} text={filtered ? '已加载的记录里没有符合条件的' : '还没有操作记录'} hint={filtered && q.hasNextPage ? '可以加载更早的记录再找' : undefined} />}
        {q.hasNextPage && (
          <div className="audit-more"><Button variant="quiet" size="md" loading={q.isFetchingNextPage} onClick={() => void q.fetchNextPage()}>加载更早的记录</Button></div>
        )}
      </Panel>
    </div>
  );
}
