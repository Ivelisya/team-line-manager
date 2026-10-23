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

/** ended：作业已结束（取消、失败），没走到的步骤不再「等待」，写「未执行」 */
function stepSub(s: JobStep, k: StepKind, ended: boolean, now?: number): string {
  if (k === 'running') return s.started_at ? `进行中 · ${F.duration(s.started_at, null, now)}` : '进行中';
  if (k === 'done') return s.started_at ? F.duration(s.started_at, s.finished_at) : stepLabel(k);
  if (k === 'pending' && ended) return '未执行';
  return stepLabel(k);
}
/** 步骤多到每格放不下标签时（7 步起）收紧：只给当前（进行中 / 失败）那一步写标签，其余悬停节点看，不出横向滚动 */
const COMPACT_AT = 7;

/** 横向步骤条：完成 / 进行中（节点有呼吸光圈）/ 失败 / 跳过 / 已补偿 / 等待，各自不同的节点与连线。窄屏横向滚动。
 *  now：调用方每秒走表时传进来，进行中那一步的用时跟着走 */
export function StepBar({ job, now }: { job: Job; now?: number }) {
  if (!job.steps.length) return null;
  const ended = !jobLive(job.state);
  const compact = job.steps.length >= COMPACT_AT;
  let focus = job.steps.findIndex((s) => { const k = stepKind(s); return k === 'running' || k === 'failed'; });
  // 没有进行中、失败的（全部做完、或取消了）：标最后一个走过的步骤，不让一排圆点一个字都没有
  if (focus < 0) { const kinds = job.steps.map(stepKind); focus = Math.max(kinds.lastIndexOf('done'), kinds.lastIndexOf('compensated')); }
  return (
    <div className="stepbar-wrap">
      <ol className="stepbar" aria-label="作业步骤" data-compact={compact ? '' : undefined}>
        {job.steps.map((s, i) => {
          const k = stepKind(s);
          const prev = i > 0 ? stepKind(job.steps[i - 1]) : null;
          const tip = k === 'failed' && (s.error || s.error_kind)
            ? <div style={{ display: 'grid', gap: 2 }}><b>{s.title}</b>{s.error_kind && <span>{errorKindLabel(s.error_kind)}</span>}{s.error && <span>{s.error}</span>}</div>
            : s.title;
          return (
            <li key={s.idx} className="stepbar-item" data-kind={k} data-link={prev === 'done' ? 'done' : prev === 'compensated' ? 'comp' : undefined} data-focus={i === focus ? '' : undefined} aria-current={k === 'running' ? 'step' : undefined}>
              <Tip text={tip}><span className="stepbar-node" tabIndex={0} aria-label={`${s.title}：${stepLabel(k)}`}>{ICON[k]}</span></Tip>
              <span className="stepbar-title">{s.title}</span>
              <span className="stepbar-sub">{stepSub(s, k, ended, now)}</span>
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
        {/* 用时从开始执行算：进行中写「已用时」，结束了写「用时」；排队时、排队中就被取消的没有用时 */}
        <span className="t-sub ellipsis">{cur ? `${cur} · ` : ''}{job.started_at ? `${live ? '已用时' : '用时'} ${F.duration(job.started_at, job.finished_at, now)}` : live ? '排队中' : '未执行'}</span>
      </div>
      <Progress value={p.pct} tone={tone} live={live} label="作业总进度" />
    </div>
  );
}
