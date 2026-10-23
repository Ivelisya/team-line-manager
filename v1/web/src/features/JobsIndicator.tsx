// 顶栏的“进行中作业”小徽标：有作业在跑时显示数量，点开浮层看每个作业的进度；作业结束时弹通知（带“查看”）。
import * as React from 'react';
import { Link, useNavigate } from 'react-router-dom';
import { ListChecks } from 'lucide-react';
import { useJobs } from '@/lib/queries';
import { currentStepTitle, jobKindLabel, jobProgress } from '@/lib/jobs';
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/overlays';
import { Progress } from '@/components/ui/progress';
import { toast } from '@/components/ui/toast';
import { Tip } from '@/components/ui/overlays';
import { copyNewShareLink } from '@/lib/moveShare';
import type { JobSummary, Paged } from '@/lib/types';

/** 「最近结束」列表（前 20 个）里新出现的作业直接按列表字段弹通知，不再逐个取详情；首次加载只记基线，不补发历史。
 *  不要对比“进行中”列表：超出那一页的作业结束了也看不到。基线之前就结束的作业（例如别的作业被删后挤进前 20）按结束时间排除。
 *  一轮超过 3 个合并成一条（批量上架、批量升级时不刷屏），个数按列表 total 的增量算（一轮结束的可能比列出的 20 个多）；
 *  换线路机成功的照常单独弹：要提醒复制新分享链接 */
function useJobNotifier(page: Paged<JobSummary> | undefined) {
  const nav = useNavigate();
  const seen = React.useRef<{ ids: Set<string>; since: number; total: number } | null>(null);
  React.useEffect(() => {
    if (!page) return;
    const done = page.items;
    const at = (j: JobSummary) => Date.parse(j.finished_at ?? '') || 0;
    const s = seen.current;
    if (!s) { seen.current = { ids: new Set(done.map((j) => j.id)), since: Math.max(0, ...done.map(at)), total: page.total }; return; }
    const fresh = done.filter((j) => !s.ids.has(j.id) && at(j) > s.since);
    done.forEach((j) => s.ids.add(j.id));
    const moved = fresh.filter((j) => j.state === 'succeeded' && j.kind === 'move_entry');
    const rest = fresh.filter((j) => !moved.includes(j));
    const label = (j: JobSummary) => `${jobKindLabel(j.kind)} · ${j.target?.name ?? '—'}`;
    moved.forEach((j) => toast.show({ tone: 'ok', title: '换线路机完成', text: `${label(j)}：分享链接已更新`, duration: 15000, action: { label: '复制新分享链接', onClick: () => void copyNewShareLink(j.id) } }));
    const ended = Math.max(rest.length, page.total - s.total - moved.length);
    s.total = page.total;
    if (rest.length > 3) {
      const n = (st: string) => rest.filter((j) => j.state === st).length;
      const failed = n('failed');
      const tally = [`成功 ${n('succeeded')}`, failed && `失败 ${failed}`, n('cancelled') && `取消 ${n('cancelled')}`].filter(Boolean).join(' · ');
      toast.show({ tone: failed ? 'bad' : 'ok', title: `${ended} 个作业已结束`, text: ended > rest.length ? `最近 ${rest.length} 个：${tally}` : tally, action: { label: '查看', onClick: () => nav('/jobs') } });
      return;
    }
    rest.forEach((j) => {
      const action = { label: '查看', onClick: () => nav(`/jobs/${j.id}`) };
      if (j.state === 'succeeded') toast.show({ tone: 'ok', title: '作业完成', text: label(j), action });
      else if (j.state === 'failed') toast.show({ tone: 'bad', title: '作业失败', text: j.error ? `${label(j)}：${j.error}` : label(j), action });
      else toast.show({ tone: 'info', title: '作业已取消', text: label(j), action });
    });
  }, [page, nav]);
}

/** 浮层只列前 6 个，数量取服务端 total：活动列表取 6 条就够 */
const SHOW = 6;

export function JobsIndicator() {
  const q = useJobs('active', 0, SHOW);
  useJobNotifier(useJobs('done', 0, 20).data);
  const [open, setOpen] = React.useState(false);
  const live = q.data?.items ?? [];
  const n = q.data?.total ?? 0; // 数量取服务端 total，列表只是第一页
  // 作业都结束时弹层随徽标一起卸掉，开关也复位：不然下一个作业开始时弹层会自己打开
  if (!n) { if (open) setOpen(false); return null; }
  return (
    <Popover open={open} onOpenChange={setOpen}>
      <Tip text={`${n} 个作业进行中`}>
        <PopoverTrigger asChild>
          <button type="button" className="top-btn jobs-ind" aria-label={`${n} 个作业进行中`}>
            <ListChecks size={16} />
            <span className="count-badge" data-tone="accent">{n}</span>
          </button>
        </PopoverTrigger>
      </Tip>
      <PopoverContent align="end" className="jobs-pop">
        <div className="pop-head">进行中的作业<span className="t-sub">{n}</span></div>
        <ul className="pop-list">
          {live.map((j) => {
            const p = jobProgress(j);
            return (
              <li key={j.id}>
                <Link to={`/jobs/${j.id}`} onClick={() => setOpen(false)} className="pop-item">
                  <span className="pop-t"><b className="ellipsis">{jobKindLabel(j.kind)} · {j.target?.name ?? '—'}</b><span className="num t-sub">{p.done}/{p.total}</span></span>
                  <Progress value={p.pct} live label={`${jobKindLabel(j.kind)}进度`} />
                  <span className="hint ellipsis">{currentStepTitle(j) ?? '排队中'}</span>
                </Link>
              </li>
            );
          })}
        </ul>
        <Link to="/jobs" onClick={() => setOpen(false)} className="pop-all">查看全部作业{n > SHOW ? `（还有 ${n - SHOW} 个）` : ''}</Link>
      </PopoverContent>
    </Popover>
  );
}
