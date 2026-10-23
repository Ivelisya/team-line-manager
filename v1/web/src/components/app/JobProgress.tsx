// 作业进度可视化：总进度（JobProgressBar）。作业详情、顶栏进度浮层共用（步骤状态在作业详情的步骤时间线里）。
import type { JobSummary } from '@/lib/types';
import { currentStepTitle, jobLive, jobProgress } from '@/lib/jobs';
import * as F from '@/lib/format';
import { Progress } from '@/components/ui/progress';

/** 总进度条 + 文字：done/total 步 · 百分比，右侧当前步骤或已用时 */
export function JobProgressBar({ job, now }: { job: JobSummary; now?: number }) {
  const p = jobProgress(job);
  const live = jobLive(job.state);
  const tone = job.state === 'failed' ? 'bad' : job.state === 'succeeded' ? 'ok' : job.state === 'cancelled' ? 'warn' : undefined;
  const cur = live ? currentStepTitle(job) : null;
  return (
    <div className="job-prog">
      <div className="job-prog-head">
        <span className="num">{p.done} / {p.total} 步 · {Math.round(p.pct)}%</span>
        {/* 用时从开始执行算：进行中写「已用时」，结束了写「用时」；排队时、排队中就被取消的没有用时 */}
        <span className="t-sub ellipsis">{cur ? `${cur} · ` : ''}{job.started_at ? `${live ? '已用时' : '用时'} ${F.duration(job.started_at, job.finished_at, now)}` : live ? '排队中' : '未执行'}</span>
      </div>
      <Progress value={p.pct} tone={tone} live={live} label="作业总进度" />
    </div>
  );
}
