// 作业：列表 + 详情（总进度 + 横向步骤条 + 步骤时间线 + 实时日志，进行中每 1.5s 轮询 /jobs/{id}/logs?after=）。
import * as React from 'react';
import { Link, useLocation, useNavigate, useParams } from 'react-router-dom';
import { ArrowLeft, ArrowDownToLine, Check, CircleDashed, Copy, ListChecks, Loader2, Minus, Pause, Play, RotateCw, Square, Undo2, X } from 'lucide-react';
import { fetchAllLogs, useCancelJob, useJob, useJobs, useRetryJob } from '@/lib/queries';
import type { Job, JobLog } from '@/lib/types';
import { errorKindLabel, jobKindLabel, jobLive, jobProgress, outputLines, stepKind, type StepKind } from '@/lib/jobs';
import { CheckRow, Field, Input } from '@/components/ui/form';
import { useNow } from '@/lib/useNow';
import { copyText } from '@/lib/clipboard';
import * as F from '@/lib/format';
import { Button, IconButton } from '@/components/ui/button';
import { Segmented } from '@/components/ui/overlays';
import { Progress } from '@/components/ui/progress';
import { SkeletonRows } from '@/components/ui/skeleton';
import { ConfirmDialog } from '@/components/app/dialogs';
import { JobProgressBar, StepBar } from '@/components/app/JobProgress';
import { NodeHover } from '@/components/app/NodeHover';
import { toast } from '@/components/ui/toast';
import { copyNewShareLink } from '@/lib/moveShare';
import { Pagination } from '@/components/app/Pagination';
import { usePage } from '@/lib/usePage';
import { Empty, ErrorNote, LabelValue, LabelValueGrid, Note, PageHead, Panel, Pill, type Tone } from '@/components/app/primitives';

const STATE: Record<string, { label: string; tone: Tone; dot?: 'pulse' | 'ring' }> = {
  queued: { label: '排队中', tone: 'neutral', dot: 'ring' }, running: { label: '进行中', tone: 'info', dot: 'pulse' }, waiting: { label: '等待中', tone: 'info', dot: 'ring' },
  cancelling: { label: '取消中', tone: 'warn', dot: 'pulse' }, succeeded: { label: '已完成', tone: 'ok' },
  failed: { label: '失败', tone: 'bad' }, cancelled: { label: '已取消', tone: 'neutral', dot: 'ring' },
};
/** 作业列表每页条数：列表在窄栏里，固定一档，不给每页条数下拉 */
const JOB_PAGE = [20];

const JobPill = ({ state }: { state: string }) => { const s = STATE[state] ?? { label: state, tone: 'neutral' as Tone }; return <Pill tone={s.tone} dot={s.dot}>{s.label}</Pill>; };

export function JobsPage() {
  const { id } = useParams();
  const nav = useNavigate();
  const { search } = useLocation(); // 列表 ↔ 详情切换时带着 ?p=，回到列表还在原来那页
  const [filter, setFilter] = React.useState<'all' | 'active' | 'done'>('all');
  // 页码要先于查询算出来（偏移量进查询）；总数来自上一次的结果，用来夹页和显示分页器。渲染中直接跟上（不等 effect 提交一遍再重渲染）
  const [total, setTotal] = React.useState<number>();
  const pg = usePage({ total, sizes: JOB_PAGE, reset: [filter] });
  const jobs = useJobs(filter, pg.offset, pg.size);
  if (jobs.data && jobs.data.total !== total) setTotal(jobs.data.total);
  const list = jobs.data?.items ?? [];
  const selected = id ?? null;
  const go = (jid: string) => nav({ pathname: `/jobs/${jid}`, search });
  return (
    <div className="stack">
      <PageHead title="作业" note={jobs.data ? `${jobs.data.total} 个` : undefined}
        tools={<Segmented label="筛选" value={filter} onChange={setFilter} items={[{ value: 'all', label: '全部' }, { value: 'active', label: '进行中' }, { value: 'done', label: '已结束' }]} />} />
      <div className="jobs-layout" data-detail={selected ? '1' : '0'}>
        <Panel ref={pg.anchor} className="jobs-list" flush>
          {jobs.isLoading && <SkeletonRows rows={5} list />}
          {jobs.isError && <div style={{ padding: 'var(--sp-12)' }}><ErrorNote error={jobs.error} onRetry={() => void jobs.refetch()} /></div>}
          {list.map((j) => (
            <div key={j.id} className="job-item" role="button" tabIndex={0} aria-current={j.id === selected} onClick={() => go(j.id)}
              onKeyDown={(e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); go(j.id); } }}>
              <span className="l1">
                <b className="ellipsis" style={{ fontWeight: 600 }}>{jobKindLabel(j.kind)} · {j.target ? <NodeHover id={j.target.id}><span>{j.target.name}</span></NodeHover> : '—'}</b>
                <span style={{ marginLeft: 'auto' }}><JobPill state={j.state} /></span>
              </span>
              {jobLive(j.state) && <span className="l-prog"><Progress value={jobProgress(j).pct} live label="总进度" /><span className="num">{jobProgress(j).done}/{jobProgress(j).total}</span></span>}
              <span className="l2"><span>{F.dt(j.created_at)}</span><span>{j.created_by}</span>{j.error && <span className="ellipsis" style={{ color: 'var(--bad-text)' }} title={j.error}>{j.error}</span>}</span>
            </div>
          ))}
          {jobs.data && list.length === 0 && <Empty icon={<ListChecks size={22} />} text="没有作业" hint="上架、退役、升级 agent 都会在这里留下一条" />}
          <Pagination pg={pg} unit="个" compact />
        </Panel>
        <div className="jobs-detail">
          {selected ? <JobDetail id={selected} /> : <Panel className="jobs-empty"><Empty icon={<ListChecks size={22} />} text="选择左侧的作业查看步骤与日志" /></Panel>}
        </div>
      </div>
    </div>
  );
}

const STEP_ICON: Record<StepKind, React.ReactNode> = { done: <Check />, running: <Loader2 className="spin" />, failed: <X />, skipped: <Minus />, compensated: <Undo2 />, pending: <CircleDashed /> };

/** 走表的“用时”：useNow 放在这个小组件里，每秒只重渲它自己，不拖着整个详情页（含日志）一起刷 */
function Dur({ a, b, live }: { a: string | null; b: string | null; live: boolean }) {
  const now = useNow(live);
  return <>{F.duration(a, b, now)}</>;
}
function LiveProgressBar({ job, live }: { job: Job; live: boolean }) {
  const now = useNow(live);
  return <JobProgressBar job={job} now={now} />;
}

function JobDetail({ id }: { id: string }) {
  const { search } = useLocation();
  const q = useJob(id);
  const cancel = useCancelJob();
  const [cancelAsk, setCancelAsk] = React.useState(false);
  const j = q.data;
  const live = !!j && jobLive(j.state);
  if (q.isError && !j) return <Panel><div style={{ padding: 'var(--sp-12)' }}><ErrorNote error={q.error} onRetry={() => void q.refetch()} /></div></Panel>;
  if (!j) return <Panel><div className="skel" style={{ height: 160, margin: 16 }} /></Panel>;
  return (
    <div className="stack">
      <Panel title={<span className="row" style={{ gap: 8 }}><Link to={{ pathname: '/jobs', search }} className="icon-btn icon-btn-sm job-back" aria-label="返回列表"><ArrowLeft size={16} /></Link><span>{jobKindLabel(j.kind)} · {j.target ? <NodeHover id={j.target.id}><span>{j.target.name}</span></NodeHover> : '—'}</span></span>} tools={<>
        <JobPill state={j.state} />
        {j.state === 'failed' && <RetryButton j={j} />}
        {j.cancellable && <Button size="sm" onClick={() => setCancelAsk(true)}><Square />取消</Button>}
      </>}>
        <div className="stack">
          <LiveProgressBar job={j} live={live} />
          <StepBar job={j} />
          <LabelValueGrid>
            <LabelValue label="创建人">{j.created_by}</LabelValue>
            <LabelValue label="创建时间">{F.dtFull(j.created_at)}</LabelValue>
            <LabelValue label="用时"><Dur a={j.started_at} b={j.finished_at} live={live} /></LabelValue>
            <LabelValue label="作业 ID"><span className="mono">{j.id}</span></LabelValue>
          </LabelValueGrid>
        </div>
        {j.error && <div style={{ marginTop: 12 }}><Note tone="bad">{j.error}</Note></div>}
        {j.kind === 'move_entry' && (j.result?.new_entry_id || j.state === 'succeeded') && (
          <div style={{ marginTop: 12 }}>
            <Note tone={j.state === 'succeeded' ? 'warn' : 'info'}>
              <span className="row row-between" style={{ gap: 12 }}>
                <span>{j.result?.note ?? '客户端需要换成新的分享链接'}：{j.state === 'succeeded' ? '旧链接已失效。' : '新入口验收通过、旧入口摘除后，旧链接会失效。'}</span>
                {j.result?.new_entry_id && <Button size="sm" onClick={() => void copyNewShareLink(j.id)}><Copy size={14} />复制新分享链接</Button>}
              </span>
            </Note>
          </div>
        )}
      </Panel>

      <Panel title="步骤" sub={`${jobProgress(j).done} / ${jobProgress(j).total}`}>
        <ol className="steps">
          {j.steps.map((s) => {
            const k = stepKind(s);
            return (
              <li key={s.idx} className="step" data-state={s.state} data-kind={k}>
                <span className="step-ic">{STEP_ICON[k]}</span>
                <div style={{ minWidth: 0 }}>
                  <div className="step-title"><b>{s.title}</b><span className="step-time">{s.started_at ? <>{F.clock(s.started_at)} · <Dur a={s.started_at} b={s.finished_at ?? (k === 'failed' ? j.finished_at : null)} live={live && !s.finished_at && k !== 'failed'} /></> : k === 'skipped' || k === 'compensated' ? '未执行' : '等待'}</span>{k === 'compensated' && <Pill tone="warn" dot="none">已补偿</Pill>}</div>
                  {outputLines(s.output).length > 0 && <pre className="step-out">{outputLines(s.output).join('\n')}</pre>}
                  {(s.error || s.error_kind) && (
                    <div className="step-err-box">
                      {s.error_kind && <span className="err-kind" title={s.error_kind}>{errorKindLabel(s.error_kind)}</span>}
                      {s.error && <p className="step-err">{s.error}</p>}
                    </div>
                  )}
                </div>
              </li>
            );
          })}
        </ol>
      </Panel>

      <LogPanel jobId={j.id} state={j.state} />
      <ConfirmDialog open={cancelAsk} onOpenChange={setCancelAsk} title="取消这个作业？" confirmLabel="取消作业" danger description={cancelDesc(j)}
        onConfirm={async () => { await cancel.mutateAsync(j.id); toast.ok('作业已取消'); }} />
    </div>
  );
}

/** 取消的后果：失败的上架 / 恢复作业取消后机器不再停在「上架失败」 */
function cancelDesc(j: Job) {
  if (j.state === 'failed' && j.kind.startsWith('provision_')) return '不再重试这次上架：已完成的步骤会被补偿（回滚），机器会被退役并清理，释放 IP 与编号。';
  if (j.state === 'failed' && j.kind.startsWith('restore_')) return '不再重试这次恢复：已完成的步骤会被补偿（回滚），机器退回「已退役」。';
  return '已完成的步骤会被补偿（回滚），然后停下。';
}

/** 重试：失败原因是主机指纹不符 / SSH 认证失败时先问要不要重置指纹、换密码，其余直接重试 */
function RetryButton({ j }: { j: Job }) {
  const retry = useRetryJob();
  const kind = j.steps.find((s) => s.state === 'failed')?.error_kind;
  const ask = kind === 'host_key' || kind === 'auth';
  const [open, setOpen] = React.useState(false);
  const [reset, setReset] = React.useState(false);
  const [pw, setPw] = React.useState('');
  const ok = () => toast.ok('已从失败的步骤重试');
  const click = () => {
    if (!ask) { retry.mutate({ id: j.id }, { onSuccess: ok, onError: toast.err }); return; }
    setReset(kind === 'host_key'); setPw(''); setOpen(true);
  };
  return (
    <>
      <Button size="sm" variant="primary" loading={retry.isPending && !open} onClick={click}><RotateCw />重试</Button>
      {ask && (
        <ConfirmDialog open={open} onOpenChange={setOpen} title="重试作业" confirmLabel="重试"
          description={kind === 'host_key' ? '机器的主机指纹和记录的不一致。确认机器重装过、IP 没有换给别的机器，再重置指纹重试。' : 'SSH 认证失败：中控密钥登录不上。可以填 SSH 密码，重试时用它重新推送中控公钥。'}
          onConfirm={async () => { await retry.mutateAsync({ id: j.id, reset_host_key: reset, ...(pw ? { ssh_password: pw } : {}) }); ok(); }}>
          <div className="stack-sm">
            <CheckRow checked={reset} onChange={setReset}>重置主机指纹</CheckRow>
            <Field label={kind === 'auth' ? 'SSH 密码' : 'SSH 密码（选填）'} hint="留空则继续用中控密钥登录">
              <Input type="password" value={pw} onChange={(e) => setPw(e.target.value)} autoComplete="new-password" />
            </Field>
          </div>
        </ConfirmDialog>
      )}
    </>
  );
}

const SHOW_MAX = 2000; // 页面里最多渲染的行数（更早的行仍在服务端）
const KEEP_MAX = 5000; // 内存里最多保留的行数：超出的从头部丢弃，并在日志顶部提示
const LEVEL: Record<string, string> = { error: '错误', warn: '警告', info: '信息', debug: '调试' };

/** 实时日志：进行中每 1.5 秒按 after=<最后一条 id> 增量拉取（满 1000 条就接着拉）；结束后再补拉一次。可暂停、自动滚到底、按级别着色 */
const LogPanel = React.memo(function LogPanel({ jobId, state }: { jobId: string; state: string }) {
  const [logs, setLogs] = React.useState<JobLog[]>([]);
  const [dropped, setDropped] = React.useState(0); // 因超过 KEEP_MAX 丢掉的行数
  const [frozen, setFrozen] = React.useState<number | null>(null); // 暂停时冻结到的最后一条日志 id（按 id 而不是行数：头部被丢弃时位置不会错）
  const [stuck, setStuck] = React.useState(true);
  const [err, setErr] = React.useState<unknown>(null);
  const last = React.useRef(0);
  const all = React.useRef<JobLog[]>([]); // 已保留的日志（真值在这里，state 只用来触发渲染）
  const box = React.useRef<HTMLDivElement>(null);
  const live = jobLive(state);

  React.useEffect(() => { all.current = []; setLogs([]); setDropped(0); setFrozen(null); setStuck(true); setErr(null); last.current = 0; }, [jobId]);
  // 只随作业和“是否进行中”重启：进行中的状态之间切换（running ↔ waiting）不必重来；离开或换作业时中止在途的请求
  React.useEffect(() => {
    let stop = false; let timer: ReturnType<typeof setTimeout>;
    const ac = new AbortController();
    const pull = async () => {
      // 进行中且页面在后台：跳过这一轮（下一轮再拉）；已结束的最后一次补拉不跳过
      if (!live || !document.hidden) try {
        const more = await fetchAllLogs(jobId, last.current, ac.signal);
        if (stop) return;
        setErr(null);
        if (more.length) {
          last.current = more[more.length - 1].id;
          const merged = all.current.concat(more);
          const over = Math.max(0, merged.length - KEEP_MAX);
          all.current = over ? merged.slice(over) : merged;
          if (over) setDropped((d) => d + over);
          setLogs(all.current);
        }
      } catch (e) { if (!stop) setErr(e); /* 下一轮重试 */ }
      if (!stop && live) timer = setTimeout(pull, 1500);
    };
    void pull();
    return () => { stop = true; clearTimeout(timer); ac.abort(); };
  }, [jobId, live]);

  const shown = React.useMemo(() => {
    if (frozen == null) return logs;
    let n = logs.length;
    while (n > 0 && logs[n - 1].id > frozen) n--;
    return logs.slice(0, n);
  }, [logs, frozen]);
  const view = shown.length > SHOW_MAX ? shown.slice(-SHOW_MAX) : shown;
  React.useEffect(() => { const el = box.current; if (el && stuck && frozen == null) el.scrollTop = el.scrollHeight; }, [view.length, stuck, frozen]);
  const jump = () => { const el = box.current; if (el) { el.scrollTop = el.scrollHeight; setStuck(true); } };
  const copy = () => void copyText(logs.map((l) => `${F.clock(l.ts)} ${LEVEL[l.level] ?? l.level} ${l.msg}`).join('\n'), '日志');

  return (
    <Panel title="日志" sub={frozen != null ? `已暂停 · 另有 ${logs.length - shown.length} 行新日志` : live ? '实时' : `${logs.length + dropped} 行`}
      tools={<>
        {live && frozen == null && <Pill tone="info" dot="pulse">实时</Pill>}
        {!stuck && frozen == null && <Button size="sm" variant="quiet" onClick={jump}><ArrowDownToLine />跳到最新</Button>}
        <Button size="sm" variant="secondary" aria-pressed={frozen != null} onClick={() => { if (frozen == null) setFrozen(logs.length ? logs[logs.length - 1].id : 0); else { setFrozen(null); setStuck(true); } }}>
          {frozen == null ? <><Pause />暂停滚动</> : <><Play />继续</>}
        </Button>
        <IconButton label="复制全部日志" size="sm" onClick={copy} disabled={!logs.length}><Copy size={14} /></IconButton>
      </>}>
      {err != null && logs.length === 0 && <div style={{ marginBottom: 8 }}><ErrorNote error={err} /></div>}
      <div className="logbox" ref={box} role="log" aria-live="off" tabIndex={0} aria-label="作业日志"
        onScroll={(e) => { const el = e.currentTarget; setStuck(el.scrollHeight - el.scrollTop - el.clientHeight < 24); }}>
        {logs.length === 0 && <span className="t-muted">暂无日志</span>}
        {(dropped > 0 || shown.length > SHOW_MAX) && <div className="logline logline-note"><span /><span /><span className="msg t-sub">{dropped > 0 ? `页面只保留最近 ${KEEP_MAX} 行，更早的 ${dropped} 行已省略；` : ''}仅显示最近 {Math.min(SHOW_MAX, shown.length)} 行，共 {shown.length + dropped} 行</span></div>}
        {view.map((l) => <LogLine key={l.id} l={l} />)}
      </div>
    </Panel>
  );
});

/** 一行日志：已拉到的行对象不变，memo 挡住增量拉取、滚动时的整表重比对 */
const LogLine = React.memo(function LogLine({ l }: { l: JobLog }) {
  return <div className="logline" data-level={l.level}><span className="ts">{F.clock(l.ts)}</span><span className={`lv-${l.level}`}>{LEVEL[l.level] ?? l.level}</span><span className="msg">{l.msg}</span></div>;
});
