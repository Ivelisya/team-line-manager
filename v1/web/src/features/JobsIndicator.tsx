// 顶栏的“进行中作业”小徽标：有作业在跑时显示数量，点开浮层看每个作业的进度；作业结束时弹通知（带“查看”）。
import * as React from 'react';
import { Link, useNavigate } from 'react-router-dom';
import { ListChecks } from 'lucide-react';
import { useJobs } from '@/lib/queries';
import { currentStepTitle, jobKindLabel, jobLive, jobProgress } from '@/lib/jobs';
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/overlays';
import { Progress } from '@/components/ui/progress';
import { toast } from '@/components/ui/toast';
import { Tip } from '@/components/ui/overlays';
import { copyNewShareLink } from '@/lib/moveShare';
import type { Job } from '@/lib/types';

/** 作业从“进行中”变成终态时弹一条通知；首次加载只记基线，不补发历史 */
function useJobNotifier(jobs: Job[] | undefined) {
  const nav = useNavigate();
  const prev = React.useRef<Map<string, string> | null>(null);
  React.useEffect(() => {
    if (!jobs) return;
    const before = prev.current;
    if (before) {
      for (const j of jobs) {
        const was = before.get(j.id);
        if (!was || !jobLive(was) || jobLive(j.state)) continue;
        const label = `${jobKindLabel(j.kind)} · ${j.target?.name ?? '—'}`;
        const action = { label: '查看', onClick: () => nav(`/jobs/${j.id}`) };
        if (j.state === 'succeeded' && j.kind === 'move_entry') {
          toast.show({ tone: 'ok', title: '换线路机完成', text: `${label}：分享链接已变化，客户端需要换成新的分享链接`, duration: 15000, action: { label: '复制新分享链接', onClick: () => void copyNewShareLink(j.id) } });
        } else if (j.state === 'succeeded') toast.show({ tone: 'ok', title: '作业完成', text: label, action });
        else if (j.state === 'failed') toast.show({ tone: 'bad', title: '作业失败', text: j.error ? `${label}：${j.error}` : label, action });
        else toast.show({ tone: 'info', title: '作业已取消', text: label, action });
      }
    }
    prev.current = new Map(jobs.map((j) => [j.id, j.state]));
  }, [jobs, nav]);
}

export function JobsIndicator() {
  const q = useJobs('all');
  useJobNotifier(q.data?.items);
  const [open, setOpen] = React.useState(false);
  const live = (q.data?.items ?? []).filter((j) => jobLive(j.state));
  if (!live.length) return null;
  return (
    <Popover open={open} onOpenChange={setOpen}>
      <Tip text={`${live.length} 个作业进行中`}>
        <PopoverTrigger asChild>
          <button type="button" className="top-btn jobs-ind" aria-label={`${live.length} 个作业进行中`}>
            <ListChecks size={16} />
            <span className="jobs-ind-n num">{live.length}</span>
          </button>
        </PopoverTrigger>
      </Tip>
      <PopoverContent align="end" className="jobs-pop">
        <div className="jobs-pop-head">进行中的作业<span className="t-sub">{live.length}</span></div>
        <ul className="jobs-pop-list">
          {live.slice(0, 6).map((j) => {
            const p = jobProgress(j);
            return (
              <li key={j.id}>
                <Link to={`/jobs/${j.id}`} onClick={() => setOpen(false)} className="jobs-pop-item">
                  <span className="jobs-pop-t"><b className="ellipsis">{jobKindLabel(j.kind)} · {j.target?.name ?? '—'}</b><span className="num t-sub">{p.done}/{p.total}</span></span>
                  <Progress value={p.pct} live label={`${jobKindLabel(j.kind)}进度`} />
                  <span className="hint ellipsis">{currentStepTitle(j) ?? '排队中'}</span>
                </Link>
              </li>
            );
          })}
        </ul>
        <Link to="/jobs" onClick={() => setOpen(false)} className="jobs-pop-all">查看全部作业{live.length > 6 ? `（还有 ${live.length - 6} 个）` : ''}</Link>
      </PopoverContent>
    </Popover>
  );
}
