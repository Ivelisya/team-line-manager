// 操作记录：全站的审计日志（登录与登录失败、API 密钥、远程执行、机器与入口变更、目录与告警设置），新→旧。
// 一次取 100 条，「加载更早的记录」按 before_id 往前翻；分类与搜索由服务端筛（分类规则见中控 api/misc.rs audit_cat），两者写进 URL。
// 有明细的记录点开原地展开，对象名点开那台机器的抽屉。
import * as React from 'react';
import { History, SearchX } from 'lucide-react';
import { useAuditLog } from '@/lib/queries';
import { useDrawer } from '@/lib/drawer';
import { useUrlState } from '@/lib/urlState';
import type { AuditEntry } from '@/lib/types';
import { Button } from '@/components/ui/button';
import { SearchInput } from '@/components/ui/form';
import { Segmented } from '@/components/ui/overlays';
import { SkeletonRows } from '@/components/ui/skeleton';
import { Empty, ErrorNote, PageHead, Panel } from '@/components/app/primitives';
import { AuditList } from '@/components/app/AuditList';

type Cat = 'all' | 'auth' | 'exec' | 'machine' | 'config';
const CATS: { value: Cat; label: string }[] = [
  { value: 'all', label: '全部' }, { value: 'auth', label: '登录与账户' }, { value: 'exec', label: '远程执行' },
  { value: 'machine', label: '机器与入口' }, { value: 'config', label: '设置与告警' },
];
const EMPTY: AuditEntry[] = [];
const WEEK = '日一二三四五六';
const dayKey = (d: Date) => `${d.getFullYear()}-${d.getMonth()}-${d.getDate()}`;
/** 按本地日期分段（记录本来就是新→旧），段标题：今天 / 昨天 / 10月20日 周一 */
function byDay(rows: AuditEntry[]) {
  const now = new Date(); const today = dayKey(now); const yday = dayKey(new Date(now.getTime() - 86400_000));
  const out: { key: string; label: string; items: AuditEntry[] }[] = [];
  for (const a of rows) {
    const d = new Date(a.ts); const k = dayKey(d);
    if (out[out.length - 1]?.key !== k) out.push({ key: k, label: k === today ? '今天' : k === yday ? '昨天' : `${d.getMonth() + 1}月${d.getDate()}日 周${WEEK[d.getDay()]}`, items: [] });
    out[out.length - 1].items.push(a);
  }
  return out;
}

export function AuditPage() {
  const { open } = useDrawer();
  // 分类（?cat=）与搜索（?q=）写进 URL：刷新、分享后保持
  const [sp, upd] = useUrlState();
  const cat: Cat = CATS.find((c) => c.value === sp.get('cat'))?.value ?? 'all';
  const search = sp.get('q') ?? '';
  const setCat = (v: Cat) => upd({ cat: v === 'all' ? '' : v });
  // 搜索框：停手 250ms 后写进 URL、交给服务端（不每敲一个字查一次）
  const [text, setText] = React.useState(search);
  React.useEffect(() => { const t = setTimeout(() => upd({ q: text.trim() }), 250); return () => clearTimeout(t); }, [text, upd]);
  // 地址里的 q 不是自己写的那样变了（后退 / 前进、带参链接）：输入框跟上。自己写进去的是 text.trim()，打字时末尾的空格不被吃掉
  const [seenQ, setSeenQ] = React.useState(search);
  if (search !== seenQ) { setSeenQ(search); if (search !== text.trim()) setText(search); }
  const q = useAuditLog(cat === 'all' ? '' : cat, search.trim());
  const rows = React.useMemo(() => q.data?.pages.flat() ?? EMPTY, [q.data]);
  const filtered = cat !== 'all' || !!search.trim();

  return (
    <div className="stack">
      <PageHead title="操作记录" tools={<Segmented label="分类" value={cat} onChange={setCat} items={CATS} />} />
      <div className="row" role="search">
        <SearchInput value={text} onChange={(e) => setText(e.target.value)} placeholder="搜索动作、对象、操作人…" aria-label="搜索" className="w-full sm:w-[300px]" />
      </div>
      {/* 第一页取不到：整页重取；往前翻的那一页取不到：只重试那一页 */}
      {q.isError && <ErrorNote error={q.error} onRetry={() => void (q.isFetchNextPageError ? q.fetchNextPage() : q.refetch())} />}
      {/* 换分类、搜索后新结果未到时，沿用的旧结果调淡（components.css [data-stale]） */}
      <Panel title="记录" count={q.data ? (q.hasNextPage ? `已加载 ${rows.length} 条` : `${rows.length} 条`) : undefined}>
        <div data-stale={q.isPlaceholderData}>
          {q.isLoading ? <SkeletonRows rows={6} />
            : rows.length ? byDay(rows).map((g) => <React.Fragment key={g.key}><div className="audit-day">{g.label}</div><AuditList items={g.items} onOpen={open} byDay /></React.Fragment>)
            : !q.isError && <Empty icon={filtered ? <SearchX size={22} /> : <History size={22} />} text={filtered ? '没有符合条件的记录' : '还没有操作记录'} />}
        </div>
        {q.hasNextPage && (
          <div className="audit-more"><Button variant="quiet" size="md" loading={q.isFetchingNextPage} onClick={() => void q.fetchNextPage()}>加载更早的记录</Button></div>
        )}
      </Panel>
    </div>
  );
}
