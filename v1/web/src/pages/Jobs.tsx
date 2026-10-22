// 作业：列表 + 详情（总进度 + 横向步骤条 + 步骤时间线 + 实时日志，进行中每 1.5s 轮询 /jobs/{id}/logs?after=）。
import * as React from 'react';
import { Link, useLocation, useNavigate, useParams, useSearchParams } from 'react-router-dom';
import { ArrowLeft, ArrowDownToLine, Check, CircleDashed, Copy, ListChecks, Loader2, Minus, Pause, Play, RotateCw, Square, Undo2, X } from 'lucide-react';
import { fetchAllLogs, fetchTailLogs, useCancelJob, useJob, useJobs, useMachine, useRetryJob } from '@/lib/queries';
import type { Job, JobLog } from '@/lib/types';
import { errorKindLabel, jobKindLabel, jobLive, jobProgress, jobText, outputLines, stepKind, type StepKind } from '@/lib/jobs';
import { jobStateLabel } from '@/lib/derive';
import { CheckRow, Field, InfoTip, Input } from '@/components/ui/form';
import { useNow } from '@/lib/useNow';
import { copyText } from '@/lib/clipboard';
import * as F from '@/lib/format';
import { Button, IconButton } from '@/components/ui/button';
import { Segmented } from '@/components/ui/overlays';
import { Select } from '@/components/ui/select';
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

/** 作业状态的色调（界面样式）；中文名在 /meta（jobStateLabel） */
const STATE: Record<string, { tone: Tone; dot?: 'pulse' }> = {
  queued: { tone: 'neutral' }, running: { tone: 'info', dot: 'pulse' }, waiting: { tone: 'info' },
  cancelling: { tone: 'warn', dot: 'pulse' }, succeeded: { tone: 'ok' },
  failed: { tone: 'bad' }, cancelled: { tone: 'neutral' },
};
/** 作业列表每页条数：列表在窄栏里，固定一档，不给每页条数下拉 */
const JOB_PAGE = [20];
/** 页签记在 URL（?view=running / done，缺省全部；首页「上架中 · 退役中」链到 ?view=running），对应列表接口的 state */
type View = 'all' | 'running' | 'done';
const VIEWS: { value: View; label: string }[] = [{ value: 'all', label: '全部' }, { value: 'running', label: '进行中' }, { value: 'done', label: '已结束' }];
const STATE_OF: Record<View, 'all' | 'active' | 'done'> = { all: 'all', running: 'active', done: 'done' };
/** 类型筛选（?kind=，与页签组合）：上架、恢复各含四类机器，按大类筛（同列表接口的 kind）；其余是作业类型原值，名字取 /meta */
const KIND_VALUES = ['provision', 'restore', 'attach_entry', 'move_entry', 'upgrade_agent', 'retire', 'purge'];
const KIND_GROUP: Record<string, string> = { provision: '上架', restore: '恢复' };

const JobPill = ({ state }: { state: string }) => { const s = STATE[state] ?? { tone: 'neutral' as Tone }; return <Pill tone={s.tone} dot={s.dot}>{jobStateLabel(state)}</Pill>; };

export function JobsPage() {
  const { id } = useParams();
  const nav = useNavigate();
  const { search } = useLocation(); // 列表 ↔ 详情切换时带着 ?p=、?view=，回到列表还在原来那页
  const [sp] = useSearchParams();
  const view: View = VIEWS.some((v) => v.value === sp.get('view')) ? (sp.get('view') as View) : 'all';
  const filter = STATE_OF[view];
  const kind = KIND_VALUES.includes(sp.get('kind') ?? '') ? sp.get('kind')! : '';
  const kindOpts = React.useMemo(() => KIND_VALUES.map((k) => ({ value: k, label: KIND_GROUP[k] ?? jobKindLabel(k) })), []);
  // 换页签、类型回到列表（不再停在之前选中的作业：进行中为 0 时右边还挂着之前的已结束作业）
  const setParam = (k: 'view' | 'kind', v: string) => {
    const n = new URLSearchParams(search); n.delete('p');
    if (v) n.set(k, v); else n.delete(k);
    const q = n.toString();
    nav({ pathname: '/jobs', search: q ? `?${q}` : '' }, { replace: true });
  };
  const setView = (v: View) => setParam('view', v === 'all' ? '' : v);
  // 页码要先于查询算出来（偏移量进查询）；总数来自上一次的结果，用来夹页和显示分页器。渲染中直接跟上（不等 effect 提交一遍再重渲染）
  const [total, setTotal] = React.useState<number>();
  const pg = usePage({ total, sizes: JOB_PAGE, reset: [filter, kind] });
  const jobs = useJobs(filter, pg.offset, pg.size, kind);
  if (jobs.data && jobs.data.total !== total) setTotal(jobs.data.total);
  const list = jobs.data?.items ?? [];
  const selected = id ?? null;
  // 列表为空、也没打开作业：整页只留一个与筛选相符的空状态（右栏不再同时写「选择左侧作业」）
  const none = !!jobs.data && list.length === 0 && !selected;
  const noneText = `没有${view === 'all' ? '' : `${VIEWS.find((v) => v.value === view)!.label}的`}${kind ? `「${kindOpts.find((o) => o.value === kind)!.label}」` : ''}作业`;
  const go = (jid: string) => nav({ pathname: `/jobs/${jid}`, search });
  // 详情跟着页面滚（只有日志框自己滚）：在列表下面点了作业、详情顶部已滚出视口时，滚回详情顶部
  const detail = React.useRef<HTMLDivElement>(null);
  React.useLayoutEffect(() => { const el = detail.current; if (el && selected && el.getBoundingClientRect().top < 0) el.scrollIntoView({ block: 'start' }); }, [selected]);
  // 按地址打开的作业：在列表里滚到看得见（每个作业只滚一次；点列表里的本来就看得见）
  const scrolled = React.useRef<string | null>(null);
  React.useEffect(() => {
    if (!selected || scrolled.current === selected) return;
    const el = document.querySelector(`.job-item[data-id="${CSS.escape(selected)}"]`);
    if (!el) return;
    scrolled.current = selected;
    el.scrollIntoView({ block: 'nearest' });
  }, [selected, list]);
  return (
    <div className="stack">
      <PageHead title="作业" note={jobs.data ? `${jobs.data.total} 个` : undefined}
        tools={<>
          <div className="w-[140px]"><Select value={kind} onChange={(v) => setParam('kind', v)} allowEmpty="全部类型" placeholder="作业类型" options={kindOpts} aria-label="作业类型" /></div>
          <Segmented label="筛选" value={view} onChange={setView} items={VIEWS} />
        </>} />
      <div className="jobs-layout" data-detail={selected ? '1' : '0'} data-empty={none ? '' : undefined}>
        <Panel ref={pg.anchor} className="jobs-list" flush>
          {jobs.isLoading && <SkeletonRows rows={5} list />}
          {jobs.isError && <div style={{ padding: 'var(--sp-12)' }}><ErrorNote error={jobs.error} onRetry={() => void jobs.refetch()} /></div>}
          {list.map((j) => (
            <div key={j.id} data-id={j.id} className="job-item" role="button" tabIndex={0} aria-current={j.id === selected} onClick={() => go(j.id)}
              onKeyDown={(e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); go(j.id); } }}>
              <span className="l1">
                <b className="ellipsis" style={{ fontWeight: 600 }}>{jobKindLabel(j.kind)} · {j.target ? <NodeHover id={j.target.id} noFocus><span>{j.target.name}</span></NodeHover> : '—'}</b>
                <span style={{ marginLeft: 'auto' }}><JobPill state={j.state} /></span>
              </span>
              {jobLive(j.state) && <span className="l-prog"><Progress value={jobProgress(j).pct} live label="总进度" /><span className="num">{jobProgress(j).done}/{jobProgress(j).total}</span></span>}
              <span className="l2"><span>{F.dt(j.created_at)}</span><span>{F.actor(j.created_by)}</span>{j.error && <span className="ellipsis" style={{ color: j.state === 'cancelled' ? 'var(--fg-2)' : 'var(--bad-text)' }} data-tip={jobText(j.error)}>{jobText(j.error)}</span>}</span>
            </div>
          ))}
          {jobs.data && list.length === 0 && <div className={none ? 'jobs-empty' : undefined}><Empty icon={<ListChecks size={22} />} text={noneText}>
            {(view !== 'all' || kind) && <Button size="sm" variant="quiet" onClick={() => nav({ pathname: '/jobs' }, { replace: true })}>清除筛选</Button>}
          </Empty></div>}
          <Pagination pg={pg} unit="个" compact />
        </Panel>
        <div className="jobs-detail" ref={detail}>
          {selected ? <JobDetail id={selected} /> : !none && <Panel className="jobs-empty"><Empty icon={<ListChecks size={22} />} text="选择左侧作业" /></Panel>}
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
/** 总进度与横向步骤条：进行中（含取消中）每秒走表，步骤条上「进行中 · N 秒」与下面的步骤时间线一起走 */
function LiveProgress({ job, live }: { job: Job; live: boolean }) {
  const now = useNow(live);
  return <><JobProgressBar job={job} now={now} /><StepBar job={job} now={now} /></>;
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
          {/* 用时写在进度条右上（进行中「已用时」、结束后「用时」），信息格不再重复 */}
          <LiveProgress job={j} live={live} />
          <LabelValueGrid>
            <LabelValue label="创建人">{F.actor(j.created_by)}</LabelValue>
            <LabelValue label="创建时间">{F.dtFull(j.created_at)}</LabelValue>
            <LabelValue label="作业 ID"><span className="mono">{j.id}</span></LabelValue>
          </LabelValueGrid>
        </div>
        {/* 取消（含「全部升级」里前一台失败后跟着取消的）不是出错：原因用中性色 */}
        {j.error && <div style={{ marginTop: 12 }}><Note tone={j.state === 'cancelled' ? undefined : 'bad'}>{jobText(j.error)}</Note></div>}
        {/* 只有换成功了链接才变：失败、取消的回滚到旧入口，旧链接照常可用 */}
        {j.kind === 'move_entry' && j.state === 'succeeded' && (
          <div style={{ marginTop: 12 }}>
            <Note tone="warn">
              <span className="row row-between" style={{ gap: 12 }}>
                <span>分享链接已变 <InfoTip text="旧链接已失效，客户端要换成新的分享链接。" /></span>
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
              <li key={s.idx} className="step" data-kind={k}>
                <span className="step-ic">{STEP_ICON[k]}</span>
                <div style={{ minWidth: 0 }}>
                  <div className="step-title"><b>{s.title}</b><span className="step-time">{s.started_at ? <>{F.clock(s.started_at)} · <Dur a={s.started_at} b={s.finished_at ?? (k === 'failed' ? j.finished_at : null)} live={live && !s.finished_at && k !== 'failed'} /></> : k === 'skipped' || k === 'compensated' || !live ? '未执行' : '等待'}</span>{k === 'compensated' && <Pill tone="warn" dot="none">已补偿</Pill>}</div>
                  {outputLines(s.output).length > 0 && <pre className="step-out">{outputLines(s.output).join('\n')}</pre>}
                  {(s.error || s.error_kind) && (
                    <div className="step-err-box">
                      {s.error_kind && <span className="err-kind">{errorKindLabel(s.error_kind, s.error)}</span>}
                      {s.error && <p className="step-err">{jobText(s.error)}</p>}
                    </div>
                  )}
                </div>
              </li>
            );
          })}
        </ol>
      </Panel>

      {/* key：换作业时整个日志面板重新挂载，日志、暂停状态从空开始，不先闪一帧上一个作业的日志 */}
      <LogPanel key={j.id} jobId={j.id} state={j.state} />
      <ConfirmDialog open={cancelAsk} onOpenChange={setCancelAsk} title="取消这个作业？" confirmLabel="取消作业" cancelLabel="返回" danger description={cancelDesc(j)}
        onConfirm={async () => { await cancel.mutateAsync(j.id); toast.info('正在取消…'); }} /> {/* 要等回滚做完才结束，结束时状态胶囊变成「已取消」 */}
    </div>
  );
}

/** 取消的后果：失败的上架 / 恢复作业取消后机器不再停在「上架失败」；升级 agent 只有排队中的能取消 */
function cancelDesc(j: Job): React.ReactNode {
  if (j.state === 'failed' && j.kind.startsWith('provision_')) return <>机器会被退役并清理 <InfoTip text="不再重试这次上架：已完成的步骤会被补偿（回滚），机器会被退役并清理，释放 IP 与编号。" /></>;
  if (j.state === 'failed' && j.kind.startsWith('restore_')) return <>机器退回「已退役」 <InfoTip text="不再重试这次恢复：已完成的步骤会被补偿（回滚），机器退回「已退役」。" /></>;
  if (j.kind === 'upgrade_agent') return <>不升了，排在后面的也取消 <InfoTip text="这台还没开始升级，取消后不升了；「全部升级」里排在它后面、等它的机器也会跟着取消。" /></>;
  return '已完成的步骤会回滚';
}

/** 重试：失败原因是主机指纹不符 / SSH 认证失败时先问一句，其余直接重试。
 *  重置主机指纹只在指纹不符、且目标不是同机 IP 节点时给（服务端同样只在这时允许，否则 409）。
 *  同机 IP 节点（上架 / 恢复 IP 节点的作业，或目标挂在宿主机上）经宿主机登录：指纹是宿主机的，要先在宿主机那边重置（说明里写步骤）；
 *  它也没有「引导密钥」这一步，密码用不上（服务端只在那一步读）：不给填，认证失败时说明要先在宿主机上把中控公钥加回去 */
function RetryButton({ j }: { j: Job }) {
  const retry = useRetryJob();
  const kind = j.steps.find((s) => s.state === 'failed')?.error_kind;
  const ask = kind === 'host_key' || kind === 'auth';
  const target = useMachine(ask ? j.target?.id ?? null : null);
  const ip = j.kind === 'provision_ip' || j.kind === 'restore_ip' || !!target.data?.host_machine;
  // 目标机器还没取回时不知道是不是同机 IP 节点：先不给重置（没有目标的作业不用等）
  const canReset = kind === 'host_key' && !ip && (!j.target || !!target.data);
  const [short, tip] = kind === 'host_key'
    ? (ip ? ['宿主机的主机指纹变了', '同机 IP 节点经宿主机登录，指纹不符说明宿主机的主机指纹变了（比如宿主机重装过系统）。先确认这是预期的变化，再对宿主机「升级 agent」：它也会因为指纹不符失败，在作业页重试它并勾选「重置主机指纹」；宿主机升级成功后，回来重试这个作业。']
      : ['主机指纹和记录的不一致', '确认机器重装过、IP 没有换给别的机器，再重置指纹重试。'])
    : ip ? ['中控密钥登录不了宿主机', '同机 IP 节点没有引导密钥这一步，填密码也用不上：先把中控公钥（中控上的 /etc/fleet/ssh/id_ed25519.pub）加回宿主机 root 的 authorized_keys，再重试。']
    : ['中控密钥登录不上', '可以填 SSH 密码，重试时用它重新推送中控公钥。'];
  const desc = <>{short} <InfoTip text={tip} /></>;
  const [open, setOpen] = React.useState(false);
  const [reset, setReset] = React.useState(false);
  const [pw, setPw] = React.useState('');
  const ok = () => toast.ok('已从失败的步骤重试');
  const click = () => {
    if (!ask) { retry.mutate({ id: j.id }, { onSuccess: ok, onError: toast.err }); return; }
    setReset(canReset); setPw(''); setOpen(true);
  };
  return (
    <>
      <Button size="sm" variant="primary" loading={retry.isPending && !open} onClick={click}><RotateCw />重试</Button>
      {ask && (
        <ConfirmDialog open={open} onOpenChange={setOpen} title="重试作业" confirmLabel="重试" description={desc}
          onConfirm={async () => { await retry.mutateAsync({ id: j.id, ...(canReset && reset ? { reset_host_key: true } : {}), ...(pw ? { ssh_password: pw } : {}) }); ok(); }}>
          <div className="stack-sm">
            {canReset && <CheckRow checked={reset} onChange={setReset}>重置主机指纹</CheckRow>}
            {!ip && (
              <Field label={kind === 'auth' ? 'SSH 密码' : 'SSH 密码（选填）'}>
                <Input type="password" value={pw} onChange={(e) => setPw(e.target.value)} autoComplete="new-password" placeholder="留空用中控密钥" />
              </Field>
            )}
          </div>
        </ConfirmDialog>
      )}
    </>
  );
}

const SHOW_MAX = 2000; // 页面里最多渲染的行数（更早的行仍在服务端）
const KEEP_MAX = 5000; // 内存里最多保留的行数：超出的从头部丢弃，并在日志顶部提示
const LEVEL: Record<string, string> = { error: '错误', warn: '警告', info: '信息', debug: '调试' };

/** 实时日志：首屏只取最后 KEEP_MAX 行；进行中每 1.5 秒按 after=<最后一条 id> 增量拉取（满 1000 条就接着拉）；结束后再补拉一次。可暂停、自动滚到底、按级别着色 */
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

  // 只随“是否进行中”重启（换作业时整个面板重新挂载）：进行中的状态之间切换（running ↔ waiting）不必重来；离开时中止在途的请求
  React.useEffect(() => {
    let stop = false; let timer: ReturnType<typeof setTimeout>;
    const ac = new AbortController();
    const pull = async () => {
      // 进行中且页面在后台：跳过这一轮（下一轮再拉）；已结束的最后一次补拉不跳过
      if (!live || !document.hidden) try {
        // 首屏只取最后 KEEP_MAX 行，更早的直接计入丢弃数（不从头一页页拉完再丢掉）；之后按 after 增量拉
        const { logs: more, skipped } = last.current === 0
          ? await fetchTailLogs(jobId, KEEP_MAX, ac.signal)
          : { logs: await fetchAllLogs(jobId, last.current, ac.signal), skipped: 0 };
        if (stop) return;
        setErr(null);
        if (more.length) {
          last.current = more[more.length - 1].id;
          const merged = all.current.concat(more);
          const over = Math.max(0, merged.length - KEEP_MAX);
          all.current = over ? merged.slice(over) : merged;
          if (over + skipped) setDropped((d) => d + over + skipped);
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
        {/* 已结束的作业不会再有新日志，不给暂停；结束前暂停着的仍可点“继续”看到最后几行 */}
        {(live || frozen != null) && <Button size="sm" variant="secondary" aria-pressed={frozen != null} onClick={() => { if (frozen == null) setFrozen(logs.length ? logs[logs.length - 1].id : 0); else { setFrozen(null); setStuck(true); } }}>
          {frozen == null ? <><Pause />暂停滚动</> : <><Play />继续</>}
        </Button>}
        <IconButton label="复制全部日志" size="sm" onClick={copy} disabled={!logs.length}><Copy size={14} /></IconButton>
      </>}>
      {err != null && logs.length === 0 && <div style={{ marginBottom: 8 }}><ErrorNote error={err} /></div>}
      <div className="logbox" ref={box} role="log" aria-live="off" tabIndex={0} aria-label="作业日志"
        onScroll={(e) => { const el = e.currentTarget; setStuck(el.scrollHeight - el.scrollTop - el.clientHeight < 24); }}>
        {logs.length === 0 && <span className="t-muted">暂无日志</span>}
        {(dropped > 0 || shown.length > SHOW_MAX) && <div className="logline logline-note"><span /><span /><span className="msg t-sub">仅显示最近 {Math.min(SHOW_MAX, shown.length)} 行，共 {shown.length + dropped} 行</span></div>}
        {view.map((l) => <LogLine key={l.id} l={l} />)}
      </div>
    </Panel>
  );
});

/** 一行日志：已拉到的行对象不变，memo 挡住增量拉取、滚动时的整表重比对 */
const LogLine = React.memo(function LogLine({ l }: { l: JobLog }) {
  return <div className="logline" data-level={l.level}><span className="ts">{F.clock(l.ts)}</span><span className={`lv-${l.level}`}>{LEVEL[l.level] ?? l.level}</span><span className="msg">{jobText(l.msg)}</span></div>;
});
