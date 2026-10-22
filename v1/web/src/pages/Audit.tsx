// 操作记录：全站的审计日志（登录与登录失败、API 密钥、远程执行、机器与入口变更、目录与告警设置），新→旧。
// 一次取 100 条，「加载更早的记录」按 before_id 往前翻；分类与搜索只筛已经加载的记录（接口只能按对象、按 id 往前翻），两者写进 URL。
// 有明细的记录点开原地展开，对象名点开那台机器的抽屉。
import * as React from 'react';
import { useSearchParams } from 'react-router-dom';
import { History, SearchX } from 'lucide-react';
import { useAuditLog } from '@/lib/queries';
import { useDrawer } from '@/lib/drawer';
import type { AuditEntry } from '@/lib/types';
import { Button } from '@/components/ui/button';
import { InfoTip, SearchInput } from '@/components/ui/form';
import { Segmented } from '@/components/ui/overlays';
import { SkeletonRows } from '@/components/ui/skeleton';
import { Empty, ErrorNote, PageHead, Panel } from '@/components/app/primitives';
import { AuditList, useAuditText } from '@/components/app/AuditList';

type Cat = 'all' | 'auth' | 'exec' | 'machine' | 'config';
const CATS: { value: Cat; label: string }[] = [
  { value: 'all', label: '全部' }, { value: 'auth', label: '登录与账户' }, { value: 'exec', label: '远程执行' },
  { value: 'machine', label: '机器与入口' }, { value: 'config', label: '设置与告警' },
];
/** 动作名归类：登录与账户（auth.*：登录、退出、两步验证、改密码、撤销会话）、远程执行（在机器上跑命令、SSH 登录、网页终端、取日志、恢复预检登录机器）、
 *  机器与入口（生命周期、改名、费用续费、入口、作业、升级 agent），其余是目录、密钥、通知、静默、告警、访问规则 */
const EXEC = new Set(['exec', 'ssh.login', 'terminal.open', 'terminal.close', 'machine.logs', 'server.logs', 'restore.probe']);
function catOf(action: string): Exclude<Cat, 'all'> {
  if (action.startsWith('auth.')) return 'auth';
  if (EXEC.has(action)) return 'exec';
  if (/^(machine|entry|relay|subscription|agent|job|renewal)(\.|$)/.test(action)) return 'machine';
  return 'config';
}
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
  const q = useAuditLog();
  const { open } = useDrawer();
  // 分类（?cat=）与搜索（?q=）写进 URL：刷新、分享后保持
  const [sp, setSp] = useSearchParams();
  const cat: Cat = CATS.find((c) => c.value === sp.get('cat'))?.value ?? 'all';
  const text = sp.get('q') ?? '';
  const set = (k: string, v: string) => setSp((p) => { const n = new URLSearchParams(p); if (v) n.set(k, v); else n.delete(k); return n; }, { replace: true });
  const setCat = (v: Cat) => set('cat', v === 'all' ? '' : v);
  const all = React.useMemo(() => q.data?.pages.flat() ?? EMPTY, [q.data]);
  const textOf = useAuditText();
  const rows = React.useMemo(() => {
    const t = text.trim().toLowerCase();
    return all.filter((a) => (cat === 'all' || catOf(a.action) === cat) && (!t || textOf(a).toLowerCase().includes(t)));
  }, [all, cat, text, textOf]);
  const filtered = cat !== 'all' || !!text.trim();

  return (
    <div className="stack">
      <PageHead title="操作记录" tools={<Segmented label="分类" value={cat} onChange={setCat} items={CATS} />} />
      <div className="row" role="search">
        <SearchInput value={text} onChange={(e) => set('q', e.target.value)} placeholder="搜索动作、对象、操作人…" aria-label="搜索" className="w-full sm:w-[300px]" />
      </div>
      {/* 第一页取不到：整页重取；往前翻的那一页取不到：只重试那一页 */}
      {q.isError && <ErrorNote error={q.error} onRetry={() => void (q.isFetchNextPageError ? q.fetchNextPage() : q.refetch())} />}
      <Panel title="记录" count={q.data ? (filtered ? `${rows.length} / 已加载 ${all.length}` : `已加载 ${all.length}`) : undefined}
        sub={filtered ? <InfoTip text="只在已加载的记录里筛；要找更早的，先点下面的「加载更早的记录」" /> : undefined}>
        {q.isLoading ? <SkeletonRows rows={6} />
          : rows.length ? byDay(rows).map((g) => <React.Fragment key={g.key}><div className="audit-day">{g.label}</div><AuditList items={g.items} onOpen={open} byDay /></React.Fragment>)
          : !q.isError && <Empty icon={filtered ? <SearchX size={22} /> : <History size={22} />} text={filtered ? '没有符合条件的记录' : '还没有操作记录'} />}
        {q.hasNextPage && (
          <div className="audit-more"><Button variant="quiet" size="md" loading={q.isFetchingNextPage} onClick={() => void q.fetchNextPage()}>加载更早的记录</Button></div>
        )}
      </Panel>
    </div>
  );
}
