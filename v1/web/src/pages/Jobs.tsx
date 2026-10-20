// 作业：列表 + 详情（步骤时间线 + 实时日志，进行中每 1.5s 轮询 /jobs/{id}/logs?after=）。
import * as React from 'react';
import { Link, useNavigate, useParams } from 'react-router-dom';
import { ArrowLeft, Check, CircleDashed, Loader2, Minus, RotateCw, Square, X } from 'lucide-react';
import { fetchLogs, useCancelJob, useJob, useJobs, useRetryJob } from '@/lib/queries';
import type { Job, JobLog, JobStep } from '@/lib/types';
import { jobKindLabel, jobLive, outputLines, stepDone, stepFailed, stepRunning, stepSkipped, stepsDone } from '@/lib/jobs';
import * as F from '@/lib/format';
import { Button } from '@/components/ui/button';
import { Segmented } from '@/components/ui/overlays';
import { ConfirmDialog } from '@/components/app/dialogs';
import { toast } from '@/components/ui/toast';
import { Empty, LabelValue, LabelValueGrid, Note, PageHead, Panel, Pill, type Tone } from '@/components/app/primitives';

const STATE: Record<string, { label: string; tone: Tone; dot?: 'pulse' | 'ring' }> = {
  queued: { label: '排队中', tone: 'neutral', dot: 'ring' }, running: { label: '进行中', tone: 'info', dot: 'pulse' }, waiting: { label: '等待中', tone: 'info', dot: 'ring' },
  cancelling: { label: '取消中', tone: 'warn', dot: 'pulse' }, succeeded: { label: '已完成', tone: 'ok' },
  failed: { label: '失败', tone: 'bad' }, cancelled: { label: '已取消', tone: 'neutral', dot: 'ring' },
};
export const JobPill = ({ state }: { state: string }) => { const s = STATE[state] ?? { label: state, tone: 'neutral' as Tone }; return <Pill tone={s.tone} dot={s.dot}>{s.label}</Pill>; };
const running = (j: Job) => jobLive(j.state);

export function JobsPage() {
  const { id } = useParams();
  const nav = useNavigate();
  const [filter, setFilter] = React.useState<'all' | 'active' | 'done'>('all');
  const jobs = useJobs(filter);
  const list = jobs.data ?? [];
  const selected = id ?? null;
  return (
    <div className="stack">
      <PageHead title="作业" note={jobs.data ? `${list.length} 个` : undefined}
        tools={<Segmented label="筛选" value={filter} onChange={setFilter} items={[{ value: 'all', label: '全部' }, { value: 'active', label: '进行中' }, { value: 'done', label: '已结束' }]} />} />
      <div className="jobs-layout" data-detail={selected ? '1' : '0'}>
        <Panel className="jobs-list" flush>
          {jobs.isLoading && <div style={{ padding: 16 }}><span className="skel" style={{ width: '70%', height: 14 }} /></div>}
          {list.map((j) => (
            <button key={j.id} className="job-item" aria-current={j.id === selected} onClick={() => nav(`/jobs/${j.id}`)}>
              <span className="l1"><b className="ellipsis" style={{ fontWeight: 600 }}>{jobKindLabel(j.kind)} · {j.target?.name ?? '—'}</b><span style={{ marginLeft: 'auto' }}><JobPill state={j.state} /></span></span>
              <span className="l2"><span>{F.dt(j.created_at)}</span><span>{j.created_by}</span>{j.error && <span className="ellipsis" style={{ color: 'var(--bad-text)' }}>{j.error}</span>}</span>
            </button>
          ))}
          {jobs.data && list.length === 0 && <Empty text="没有作业" />}
        </Panel>
        <div className="jobs-detail">
          {selected ? <JobDetail id={selected} /> : <Panel><Empty text="选择左侧的作业查看步骤与日志" /></Panel>}
        </div>
      </div>
    </div>
  );
}

function StepIcon({ s }: { s: JobStep }) {
  if (stepDone(s)) return <Check />;
  if (stepRunning(s)) return <Loader2 className="spin" />;
  if (stepFailed(s)) return <X />;
  if (stepSkipped(s)) return <Minus />;
  return <CircleDashed />;
}

function JobDetail({ id }: { id: string }) {
  const q = useJob(id);
  const retry = useRetryJob(); const cancel = useCancelJob();
  const [cancelAsk, setCancelAsk] = React.useState(false);
  const j = q.data;
  if (q.isError) return <Panel><Note tone="bad">{(q.error as Error).message}</Note></Panel>;
  if (!j) return <Panel><div className="skel" style={{ height: 160, margin: 16 }} /></Panel>;
  return (
    <div className="stack">
      <Panel title={<span className="row" style={{ gap: 8 }}><Link to="/jobs" className="icon-btn icon-btn-sm job-back" aria-label="返回列表"><ArrowLeft size={16} /></Link>{jobKindLabel(j.kind)} · {j.target?.name ?? '—'}</span>} tools={<>
        <JobPill state={j.state} />
        {j.state === 'failed' && <Button size="sm" variant="primary" loading={retry.isPending} onClick={() => retry.mutate(j.id, { onSuccess: () => toast.ok('已从失败的步骤重试'), onError: toast.err })}><RotateCw />重试</Button>}
        {j.cancellable && <Button size="sm" onClick={() => setCancelAsk(true)}><Square />取消</Button>}
      </>}>
        <LabelValueGrid>
          <LabelValue label="创建人">{j.created_by}</LabelValue>
          <LabelValue label="创建时间">{F.dtFull(j.created_at)}</LabelValue>
          <LabelValue label="用时">{F.duration(j.started_at, j.finished_at)}</LabelValue>
          <LabelValue label="作业 ID"><span className="mono">{j.id}</span></LabelValue>
        </LabelValueGrid>
        {j.error && <div style={{ marginTop: 12 }}><Note tone="bad">{j.error}</Note></div>}
      </Panel>

      <Panel title="步骤" sub={`${stepsDone(j)} / ${j.step_count || j.steps.length}`}>
        <ol className="steps">
          {j.steps.map((s) => (
            <li key={s.idx} className="step" data-state={s.state}>
              <span className="step-ic"><StepIcon s={s} /></span>
              <div style={{ minWidth: 0 }}>
                <div className="step-title"><b>{s.title}</b><span className="step-time">{s.started_at ? `${F.clock(s.started_at)} · ${F.duration(s.started_at, s.finished_at)}` : '等待'}</span></div>
                {outputLines(s.output).length > 0 && <pre className="step-out">{outputLines(s.output).join('\n')}</pre>}
                {s.error && <p className="step-err">{s.error}</p>}
              </div>
            </li>
          ))}
        </ol>
      </Panel>

      <LogPanel job={j} />
      <ConfirmDialog open={cancelAsk} onOpenChange={setCancelAsk} title="取消这个作业？" confirmLabel="取消作业" danger description="已完成的步骤会被补偿（回滚），然后停下。"
        onConfirm={async () => { await cancel.mutateAsync(j.id); toast.ok('作业已取消'); }} />
    </div>
  );
}

/** 实时日志：进行中每 1.5 秒按 after=<最后一条 id> 增量拉取；结束后再补拉一次 */
function LogPanel({ job }: { job: Job }) {
  const [logs, setLogs] = React.useState<JobLog[]>([]);
  const last = React.useRef(0);
  const box = React.useRef<HTMLDivElement>(null);
  const stick = React.useRef(true);
  const live = running(job);

  React.useEffect(() => { setLogs([]); last.current = 0; stick.current = true; }, [job.id]);
  React.useEffect(() => {
    let stop = false; let timer: ReturnType<typeof setTimeout>;
    const pull = async () => {
      try {
        const more = await fetchLogs(job.id, last.current);
        if (stop) return;
        if (more.length) { last.current = more[more.length - 1].id; setLogs((l) => [...l, ...more]); }
      } catch { /* 下一轮重试 */ }
      if (!stop && live) timer = setTimeout(pull, 1500);
    };
    void pull();
    return () => { stop = true; clearTimeout(timer); };
  }, [job.id, live, job.state]);

  React.useEffect(() => { const el = box.current; if (el && stick.current) el.scrollTop = el.scrollHeight; }, [logs]);
  return (
    <Panel title="日志" sub={live ? '实时' : undefined} tools={live ? <Pill tone="info" dot="pulse">连接中</Pill> : undefined}>
      <div className="logbox" ref={box} onScroll={(e) => { const el = e.currentTarget; stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 24; }} role="log" aria-live="off">
        {logs.length === 0 && <span className="t-muted">暂无日志</span>}
        {logs.map((l) => (
          <div key={l.id} className="logline"><span className="ts">{F.clock(l.ts)}</span><span className={`lv-${l.level}`}>{l.level === 'error' ? '错误' : l.level === 'warn' ? '警告' : '信息'}</span><span className="msg">{l.msg}</span></div>
        ))}
      </div>
    </Panel>
  );
}
