// 告警：事件列表（确认 / 查看机器，服务端分页）+ 静默与维护窗口（可预约开始时间）。列表都保持服务端顺序（告警严重度优先，静默按开始时间），前端不重排。
import * as React from 'react';
import { useSearchParams } from 'react-router-dom';
import { BellOff, CalendarPlus, Check, CheckCircle2, Plus, Trash2, Wrench } from 'lucide-react';
import { RenewDialog } from '@/features/BillingTab';
import { INCIDENT_PAGE, useAck, useAddSilence, useDelSilence, useIncidents, useMachineBriefs, useSilences } from '@/lib/queries';
import { usePage } from '@/lib/usePage';
import { Pagination } from '@/components/app/Pagination';
import { useDrawer } from '@/lib/drawer';
import * as F from '@/lib/format';
import { severityOf } from '@/lib/derive';
import type { Incident, Silence } from '@/lib/types';
import { resetOnOpen, useHeld } from '@/lib/useHeld';
import { useNow } from '@/lib/useNow';
import { Button, IconButton } from '@/components/ui/button';
import { CheckRow, Field, Input } from '@/components/ui/form';
import { Select } from '@/components/ui/select';
import { Segmented, Trunc } from '@/components/ui/overlays';
import { SkeletonRows } from '@/components/ui/skeleton';
import { toast } from '@/components/ui/toast';
import { ConfirmDialog, FormDialog } from '@/components/app/dialogs';
import { NodeHover } from '@/components/app/NodeHover';
import { Empty, ErrorNote, FilterChip, Note, PageHead, Panel, Pill } from '@/components/app/primitives';

const PAGE_SIZES = [20, INCIDENT_PAGE, 100];
const SEV = (i: Incident) => { const s = severityOf(i.severity); return i.state === 'resolved' ? <Pill tone="ok">已恢复</Pill> : <Pill tone={s.tone}>{s.label}</Pill>; };

export function IncidentsPage() {
  const [state, setState] = React.useState<'firing' | 'resolved' | 'all'>('firing');
  // 页码先于查询算出（偏移量进查询）；总数取上一次的结果。渲染中直接跟上（不等 effect 提交一遍再重渲染）
  const [total, setTotal] = React.useState<number>();
  const pg = usePage({ total, sizes: PAGE_SIZES, size: INCIDENT_PAGE, reset: [state] });
  const q = useIncidents(state, pg.offset, pg.size);
  if (q.data && q.data.total !== total) setTotal(q.data.total);
  const ack = useAck();
  const { open } = useDrawer();
  const list = q.data?.items ?? [];
  // 续费类告警的「登记续费」要用机器的订阅信息；续费后下一轮评估（≤30 秒）自动恢复
  const machines = useMachineBriefs().data;
  const [renewId, setRenewId] = React.useState<string | null>(null);
  // 关闭后沿用最近的机器，对话框常驻挂载、播完退场动画
  const renewM = useHeld(renewId, machines?.find((m) => m.id === renewId));
  return (
    <div className="stack" ref={pg.anchor}>
      <PageHead title="告警" note={q.data ? `${q.data.total} 条` : undefined}
        tools={<Segmented label="状态" value={state} onChange={setState} items={[{ value: 'firing', label: '进行中' }, { value: 'resolved', label: '已恢复' }, { value: 'all', label: '全部' }]} />} />
      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
      <Panel flush>
        <div className="table-wrap">
          <table className="dt" style={{ minWidth: 820 }}>
            {/* 末列 120：「登记续费」小按钮 90（边框 2 + 内边距 20 + 图标 14 + 间距 6 + 四个字 48）+ 单元格内边距 12 + 16 = 118，留 2px 余量；多出的从说明列（自适应）让出 */}
            <colgroup><col style={{ width: 100 }} /><col style={{ width: 150 }} /><col style={{ width: 150 }} /><col /><col style={{ width: 110 }} /><col style={{ width: 120 }} /></colgroup>
            <thead><tr><th>级别</th><th>对象</th><th>规则</th><th>说明</th><th>开始</th><th /></tr></thead>
            <tbody>
              {list.map((i) => {
                const sid = i.subject?.id ?? i.subject_id;
                const canRenew = i.state === 'firing' && i.rule.startsWith('renewal_') && !!machines?.find((m) => m.id === sid)?.subscription;
                return (
                  <tr key={i.id}>
                    <td>{SEV(i)}</td>
                    {/* 中控自身的告警（central_ 开头）没有机器可点开：只写对象名「中控」 */}
                    <td>{sid ? <NodeHover id={sid}><button className="btn btn-link" style={{ maxWidth: '100%' }} onClick={() => open(sid)}><span className="ellipsis">{i.subject?.name ?? i.subject_name}</span></button></NodeHover> : (i.subject_name ?? '—')}</td>
                    <td>{i.rule_name}{i.reopen_count > 0 && <span className="sub" data-tip={`恢复后 30 分钟内又复发 ${i.reopen_count} 次，合并为这一条`}>反复 {i.reopen_count + 1} 次</span>}</td>
                    <td><Trunc className="cell-main">{i.summary}</Trunc><span className="sub">{i.resolved_at ? `恢复于 ${F.dt(i.resolved_at)}` : i.ack_at ? `${i.ack_by ?? ''} 已确认 · ${F.ago(i.ack_at)}` : '未确认'}</span></td>
                    <td>{F.dt(i.opened_at)}<span className="sub">{F.ago(i.opened_at)}</span></td>
                    <td className="r">{canRenew && <Button size="sm" onClick={() => setRenewId(sid)}><CalendarPlus size={13} />登记续费</Button>}{!canRenew && i.state === 'firing' && !i.ack_at && <Button size="sm" loading={ack.isPending && ack.variables === i.id} disabled={ack.isPending} onClick={() => ack.mutate(i.id, { onSuccess: () => toast.ok('已确认'), onError: toast.err })}><Check size={13} />确认</Button>}</td>
                  </tr>
                );
              })}
            </tbody>
          </table>
          {q.isLoading && <SkeletonRows rows={4} />}
          {q.data && list.length === 0 && <Empty icon={state === 'firing' ? <CheckCircle2 size={22} /> : undefined} text={state === 'firing' ? '没有进行中的告警' : '没有记录'} hint={state === 'firing' ? '所有机器都在正常范围内' : undefined} />}
        </div>
        <Pagination pg={pg} unit="条" />
      </Panel>
      {renewM?.subscription && <RenewDialog m={renewM} s={renewM.subscription} open={!!renewId} onOpenChange={(o) => !o && setRenewId(null)} />}
      <Silences />
    </div>
  );
}

const ALL = '__all__';
const MAX_HOURS = 720; // 30 天
const AHEAD_DAYS = 90; // 最多提前 90 天预约（同中控）
const MIN = 60_000;
type StartMode = 'now' | 'at';
const START_ITEMS: { value: StartMode; label: string }[] = [{ value: 'now', label: '现在' }, { value: 'at', label: '指定时间' }];
/** 常用时长（分钟），默认 4 小时；别的时长用「自定义」填小时数 */
const DURATIONS: { min: number; label: string }[] = [
  { min: 15, label: '15 分钟' }, { min: 30, label: '30 分钟' }, { min: 60, label: '1 小时' }, { min: 120, label: '2 小时' },
  { min: 240, label: '4 小时' }, { min: 480, label: '8 小时' }, { min: 1440, label: '24 小时' },
];
/** datetime-local 的值：本地时间，到分钟 */
const localInput = (t: number) => { const d = new Date(t); return `${d.getFullYear()}-${F.pad2(d.getMonth() + 1)}-${F.pad2(d.getDate())}T${F.pad2(d.getHours())}:${F.pad2(d.getMinutes())}`; };
const nextHour = () => { const d = new Date(); d.setMinutes(60, 0, 0); return localInput(d.getTime()); };
const dtMs = (t: number) => (Number.isNaN(t) ? '—' : F.dt(new Date(t).toISOString()));

function Silences() {
  const q = useSilences(); const del = useDelSilence();
  const [sp, setSp] = useSearchParams();
  const [open, setOpen] = React.useState(false);
  const [preset, setPreset] = React.useState(''); // 新建静默预选的对象（'' = 不预选）
  const [askDel, setAskDel] = React.useState<Silence | null>(null);
  const delHeld = useHeld(askDel?.id, askDel);
  // 命令面板 / 右键菜单：/incidents?silence=<机器id> 直接打开“新建静默”并选好对象；silence=new 不预选
  React.useEffect(() => {
    const want = sp.get('silence');
    if (!want) return;
    setPreset(want === 'new' ? '' : want); setOpen(true);
    setSp((p) => { const n = new URLSearchParams(p); n.delete('silence'); return n; }, { replace: true });
  }, [sp, setSp]);
  // 走表（有静默时每 10 秒重渲染）：到点「已预约」变「进行中」、到期的不再列出——静默开始、到期都没有事件，查询仍按时间过期，下次重取时中控也不再给它。
  // 时刻取渲染这一刻的，不用表上的：刚建的「从现在开始」不会因为表慢了几秒先显示成「已预约」
  useNow(!!q.data?.length, 10_000);
  const now = Date.now();
  const list = (q.data ?? []).filter((s) => Date.parse(s.ends_at) > now);
  return (
    <>
      <Panel title="静默与维护窗口" count={list.length} sub="窗口内不会产生告警" tools={<Button onClick={() => { setPreset(''); setOpen(true); }}><Plus />新建静默</Button>} flush>
        {q.isError && <div style={{ padding: 'var(--sp-12) var(--card-pad)' }}><ErrorNote error={q.error} onRetry={() => void q.refetch()} /></div>}
        {q.isLoading ? <SkeletonRows rows={2} head /> : list.length === 0 && !q.isError ? <Empty icon={<BellOff size={22} />} text="没有进行中或已预约的静默" hint="维护机器前先建一个（可以预约开始时间），期间不会发告警" /> : (
          <div className="table-wrap">
            {/* 服务端按开始时间排：进行中的在前，已预约的随后 */}
            <table className="dt" style={{ minWidth: 760 }}>
              <colgroup><col style={{ width: 150 }} /><col style={{ width: 90 }} /><col style={{ width: 90 }} /><col style={{ width: 230 }} /><col /><col style={{ width: 64 }} /></colgroup>
              <thead><tr><th>对象</th><th>类型</th><th>状态</th><th>时间</th><th>备注</th><th /></tr></thead>
              <tbody>{list.map((s) => {
                const start = Date.parse(s.starts_at); const on = start <= now;
                return (
                  <tr key={s.id}>
                    <td><span className="cell-main strong">{s.subject_name ?? '全部机器'}</span>{s.rule && <span className="sub">只静默「{s.rule_name ?? s.rule}」</span>}</td>
                    <td>{s.is_maintenance ? <Pill tone="info" dot="none"><Wrench size={11} />维护</Pill> : <Pill>静默</Pill>}</td>
                    <td>{on ? <Pill tone="ok">进行中</Pill> : <Pill>已预约</Pill>}</td>
                    <td>{F.dt(s.starts_at)} → {F.dt(s.ends_at)}<span className="sub">{on ? `还剩 ${F.span(Date.parse(s.ends_at) - now)}` : `${F.span(start - now)}后开始`}</span></td>
                    <td><Trunc className="cell-main">{s.comment}</Trunc></td>
                    <td className="c"><IconButton label="删除静默" size="sm" onClick={() => setAskDel(s)}><Trash2 size={14} /></IconButton></td>
                  </tr>
                );
              })}</tbody>
            </table>
          </div>
        )}
      </Panel>
      <SilenceDialog open={open} onOpenChange={setOpen} preset={preset} />
      <ConfirmDialog open={!!askDel} onOpenChange={(o) => !o && setAskDel(null)} title={`删除「${delHeld?.subject_name ?? '全部机器'}」的${delHeld?.is_maintenance ? '维护窗口' : '静默'}？`} confirmLabel="删除" danger
        description={delHeld && Date.parse(delHeld.starts_at) > now
          ? `它还没开始（${F.dt(delHeld.starts_at)} 起），删除即取消这次预约，不影响现在的告警。`
          : `删除后立即恢复告警：${delHeld?.subject_name ?? '所有机器'}若仍异常，会马上发出通知。`}
        onConfirm={async () => { await del.mutateAsync(askDel!.id); toast.ok('已删除静默'); }} />
    </>
  );
}

/** 新建静默。每次打开重新挂载（resetOnOpen）：对象按 preset 预选，其余回到默认（现在开始、4 小时、标记为维护）。选“全部机器”时提交前再确认一次（写明起止） */
const SilenceDialog = resetOnOpen(function SilenceDialog({ open, onOpenChange, preset }: { open: boolean; onOpenChange: (o: boolean) => void; preset: string }) {
  const add = useAddSilence();
  const machines = useMachineBriefs().data;
  // 选项随机器列表变（结构共享：只改读数的推送不换引用），不随每次输入重建：几百台时 Select 不必每敲一个字重渲全部选项
  const options = React.useMemo(() => [{ value: ALL, label: '全部机器（影响所有告警）' }, ...(machines ?? []).map((m) => ({ value: m.id, label: m.name }))], [machines]);
  const [subject, setSubject] = React.useState(preset);
  const [mode, setMode] = React.useState<StartMode>('now'); const [start, setStart] = React.useState(nextHour);
  const [dur, setDur] = React.useState<number | 'custom'>(240); const [hours, setHours] = React.useState('');
  const [maint, setMaint] = React.useState(true); const [comment, setComment] = React.useState('');
  const [askAll, setAskAll] = React.useState(false);
  // 指定时间时走表：对话框开着不动，选好的时间过去了也要提示
  useNow(open && mode === 'at', 10_000);
  const now = Date.now();
  const startMs = Date.parse(start); // datetime-local 的值不带时区，按本地时间解析；没填完时为 NaN
  // 当前这一分钟还算数（同 fleetctl）
  const startErr = mode === 'now' ? undefined : Number.isNaN(startMs) ? '请选择开始时间' : startMs + MIN <= now ? '这个时间已经过去了；要马上开始请选「现在」'
    : startMs > now + AHEAD_DAYS * 1440 * MIN ? `最多提前 ${AHEAD_DAYS} 天预约` : undefined;
  const h = Number(hours);
  const minutes = dur === 'custom' ? h * 60 : dur;
  const hoursErr = dur === 'custom' && hours !== '' && (h < 1 || h > MAX_HOURS) ? `时长应在 1–${MAX_HOURS} 小时之间` : undefined;
  const valid = !!subject && !startErr && (dur !== 'custom' || (h >= 1 && h <= MAX_HOURS));
  const durText = dur === 'custom' ? `${hours || 0} 小时` : DURATIONS.find((d) => d.min === dur)?.label;
  // 改成自定义时带上刚才选的时长（不足 1 小时按 1 小时）
  const pickCustom = () => { if (dur !== 'custom') setHours(String(Math.max(1, Math.round(dur / 60)))); setDur('custom'); };
  const startAt = () => (mode === 'now' ? Date.now() : startMs);
  const body = () => {
    const s = startAt();
    return { subject_id: subject === ALL ? null : subject, starts_at: new Date(s).toISOString(), ends_at: new Date(s + minutes * MIN).toISOString(), is_maintenance: maint, comment: comment.trim() };
  };
  const submit = () => add.mutate(body(), { onSuccess: () => { toast.ok('已创建静默'); onOpenChange(false); }, onError: toast.err });
  const desc = mode === 'now' ? `从现在开始，${durText}内所选对象不会触发告警。` : `从 ${dtMs(startMs)} 开始，${durText}内（到 ${dtMs(startMs + minutes * MIN)}）所选对象不会触发告警。`;
  return (
    <>
      <FormDialog open={open} onOpenChange={onOpenChange} title="新建静默" desc={desc} submitLabel="创建" busy={add.isPending} valid={valid}
        dirty={subject !== preset || mode !== 'now' || dur !== 240 || !maint || !!comment.trim()}
        onSubmit={() => (subject === ALL ? setAskAll(true) : submit())}>
        <Field label="对象" hint="必须选择：是哪台机器要静默"><Select value={subject} onChange={setSubject} placeholder="选择机器" options={options} /></Field>
        {subject === ALL && <Note tone="warn">静默「全部机器」会让所有机器在这段时间内都不发告警。提交时需要再确认一次。</Note>}
        <div className="field">
          <span className="label">开始</span>
          <div className="row">
            <Segmented label="开始" value={mode} onChange={setMode} items={START_ITEMS} />
            {mode === 'at' && <Input type="datetime-local" aria-label="开始时间" value={start} min={localInput(now)} max={localInput(now + AHEAD_DAYS * 1440 * MIN)} onChange={(e) => setStart(e.target.value)} aria-invalid={!!startErr} style={{ flex: '1 1 200px', width: 'auto' }} />}
          </div>
          {startErr ? <div className="hint is-error">{startErr}</div> : mode === 'at' && <div className="hint">按本机时间，最多提前 {AHEAD_DAYS} 天</div>}
        </div>
        <div className="field">
          <span className="label">时长</span>
          <div className="row" role="group" aria-label="时长">
            {DURATIONS.map((d) => <FilterChip key={d.min} pressed={dur === d.min} onClick={() => setDur(d.min)}>{d.label}</FilterChip>)}
            <FilterChip pressed={dur === 'custom'} onClick={pickCustom}>自定义</FilterChip>
          </div>
          {dur === 'custom' && <Input inputMode="numeric" aria-label="自定义时长（小时）" placeholder="小时数" autoFocus value={hours} onChange={(e) => setHours(e.target.value.replace(/\D/g, ''))} aria-invalid={!!hoursErr} />}
          {hoursErr ? <div className="hint is-error">{hoursErr}</div> : dur === 'custom' && <div className="hint">填小时数，最长 {MAX_HOURS} 小时（30 天）</div>}
        </div>
        <CheckRow checked={maint} onChange={setMaint}>标记为维护（卡片显示「维护中」）</CheckRow>
        <Field label="备注"><Input value={comment} onChange={(e) => setComment(e.target.value)} placeholder="例如：换内核" /></Field>
      </FormDialog>
      <ConfirmDialog open={askAll} onOpenChange={setAskAll} title="静默全部机器？" confirmLabel="静默全部机器" danger
        description={`从${mode === 'now' ? '现在' : ` ${dtMs(startMs)} `}到 ${dtMs(startAt() + minutes * MIN)}，所有机器的告警都不会发出${maint ? '（都显示「维护中」）' : ''}。确认是要这样做吗？`}
        onConfirm={async () => { await add.mutateAsync(body()); toast.ok('已创建静默'); onOpenChange(false); }} />
    </>
  );
});
