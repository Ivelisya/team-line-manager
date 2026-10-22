// 作业进度可视化：横向步骤条（StepBar）、总进度（JobProgressBar）。作业页、作业详情、顶栏进度浮层共用。
import { Check, CircleDashed, Loader2, Minus, Undo2, X } from 'lucide-react';
import type { Job, JobStep, JobSummary } from '@/lib/types';
import { currentStepTitle, errorKindLabel, jobLive, jobProgress, stepKind, type StepKind } from '@/lib/jobs';
import * as F from '@/lib/format';
import { Tip } from '@/components/ui/overlays';
import { stepLabel } from '@/lib/derive';
import { Progress } from '@/components/ui/progress';

const ICON: Record<StepKind, React.ReactNode> = {
  done: <Check />, running: <Loader2 className="spin" />, failed: <X />, skipped: <Minus />, compensated: <Undo2 />, pending: <CircleDashed />,
};

function stepSub(s: JobStep, k: StepKind): string {
  if (k === 'running') return s.started_at ? `进行中 · ${F.duration(s.started_at, null)}` : '进行中';
  if (k === 'done') return s.started_at ? F.duration(s.started_at, s.finished_at) : stepLabel(k);
  return stepLabel(k);
}

/** 横向步骤条：完成 / 进行中（节点有呼吸光圈）/ 失败 / 跳过 / 已补偿 / 等待，各自不同的节点与连线。窄屏横向滚动 */
export function StepBar({ job }: { job: Job }) {
  if (!job.steps.length) return null;
  return (
    <div className="stepbar-wrap">
      <ol className="stepbar" aria-label="作业步骤">
        {job.steps.map((s, i) => {
          const k = stepKind(s);
          const prev = i > 0 ? stepKind(job.steps[i - 1]) : null;
          const tip = k === 'failed' && (s.error || s.error_kind)
            ? <div style={{ display: 'grid', gap: 2 }}><b>{s.title}</b>{s.error_kind && <span>{errorKindLabel(s.error_kind)}</span>}{s.error && <span>{s.error}</span>}</div>
            : s.title;
          return (
            <li key={s.idx} className="stepbar-item" data-kind={k} data-link={prev === 'done' ? 'done' : prev === 'compensated' ? 'comp' : undefined} aria-current={k === 'running' ? 'step' : undefined}>
              <Tip text={tip}><span className="stepbar-node" tabIndex={0} aria-label={`${s.title}：${stepLabel(k)}`}>{ICON[k]}</span></Tip>
              <span className="stepbar-title">{s.title}</span>
              <span className="stepbar-sub">{stepSub(s, k)}</span>
            </li>
          );
        })}
      </ol>
    </div>
  );
}

/** 总进度条 + 文字：done/total 步 · 百分比，右侧当前步骤或已用时 */
export function JobProgressBar({ job, now, showStep = true }: { job: JobSummary; now?: number; showStep?: boolean }) {
  const p = jobProgress(job);
  const live = jobLive(job.state);
  const tone = job.state === 'failed' ? 'bad' : job.state === 'succeeded' ? 'ok' : job.state === 'cancelled' ? 'warn' : undefined;
  const cur = showStep && live ? currentStepTitle(job) : null;
  return (
    <div className="job-prog">
      <div className="job-prog-head">
        <span className="num">{p.done} / {p.total} 步 · {Math.round(p.pct)}%</span>
        <span className="t-sub ellipsis">{cur ? `${cur} · ` : ''}已用时 {F.duration(job.started_at ?? job.created_at, job.finished_at, now)}</span>
      </div>
      <Progress value={p.pct} tone={tone} live={live} label="作业总进度" />
    </div>
  );
}
