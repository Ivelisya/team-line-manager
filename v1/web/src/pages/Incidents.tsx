// 告警：事件列表（确认 / 查看机器，服务端分页）+ 静默与维护窗口（可预约开始时间）。列表都保持服务端顺序（告警严重度优先，静默按开始时间），前端不重排。
import * as React from 'react';
import { useSearchParams } from 'react-router-dom';
import { BellOff, CalendarPlus, Check, CheckCircle2, ChevronDown, ChevronRight, Plus, Trash2, Wrench } from 'lucide-react';
import { RenewDialog } from '@/features/BillingTab';
import { INCIDENT_PAGE, useAck, useAddSilence, useDelSilence, useIncidents, useMachineBriefs, useSilences } from '@/lib/queries';
import { usePage } from '@/lib/usePage';
import { Pagination } from '@/components/app/Pagination';
import { useDrawer } from '@/lib/drawer';
import { isRouterIncident, routerIncidentTab, useRouterDrawer } from '@/lib/routers';
import * as F from '@/lib/format';
import { errText, severityOf } from '@/lib/derive';
import type { Incident, Silence } from '@/lib/types';
import { resetOnOpen, useHeld } from '@/lib/useHeld';
import { useNow } from '@/lib/useNow';
import { Button, IconButton } from '@/components/ui/button';
import { CheckRow, Field, InfoTip, Input } from '@/components/ui/form';
import { Popover, PopoverContent, PopoverTrigger, Segmented, Trunc } from '@/components/ui/overlays';
import { Command, CommandEmpty, CommandGroup, CommandInput, CommandItem, CommandList } from '@/components/ui/command';
import { collator } from '@/lib/sort';
import { matchTier, sv } from '@/lib/match';
import { DurationField, SILENCE_MAX_MIN, useDuration, type DurationItem } from '@/components/app/DurationField';
import type { MachineBrief } from '@/lib/queries';
import { SkeletonRows } from '@/components/ui/skeleton';
import { toast } from '@/components/ui/toast';
import { ConfirmDialog, FormDialog } from '@/components/app/dialogs';
import { NodeHover } from '@/components/app/NodeHover';
import { Empty, ErrorNote, Note, PageHead, Panel, Pill, TableWrap } from '@/components/app/primitives';

const PAGE_SIZES = [20, INCIDENT_PAGE, 100];
/** 级别一直按原级别（严重 / 警告）显示；已恢复的调淡，状态写在说明下面 */
const SEV = (i: Incident) => { const s = severityOf(i.severity); return <Pill tone={s.tone} className={i.state === 'resolved' ? 'is-past' : undefined}>{s.label}</Pill>; };
/** 说明列：摘要以「对象名：」开头（通知要能单独看懂），对象列已有名字，去掉不重复；英文原始错误换成中文（同顶栏预览与抽屉） */
const summaryText = (i: Incident) => {
  const name = i.subject?.name ?? i.subject_name;
  return errText(name && i.summary.startsWith(`${name}：`) ? i.summary.slice(name.length + 1) : i.summary);
};
/** 同一状态（进行中 / 已恢复）里，同一分钟开始的同一条规则（如一批 agent 同时离线、一批线路机入口连不上）连着出现 3 条起折成一行；只合并相邻的，顺序仍是服务端的 */
const BATCH_MIN = 3;
const batchKey = (i: Incident) => `${i.rule}|${i.opened_at.slice(0, 16)}`;
function foldBatches(list: Incident[]): (Incident | Incident[])[] {
  const out: (Incident | Incident[])[] = [];
  for (let a = 0; a < list.length;) {
    let b = a + 1;
    while (b < list.length && list[b].state === list[a].state && batchKey(list[b]) === batchKey(list[a])) b++;
    if (b - a >= BATCH_MIN) out.push(list.slice(a, b)); else out.push(...list.slice(a, b));
    a = b;
  }
  return out;
}
type State = 'firing' | 'resolved' | 'all';
const STATES: { value: State; label: string }[] = [{ value: 'firing', label: '进行中' }, { value: 'resolved', label: '已恢复' }, { value: 'all', label: '全部' }];

export function IncidentsPage() {
  // 页签写进 URL 的 ?state=（进行中是默认，不写）；页码在 usePage 里（?p=）
  const [sp, setSp] = useSearchParams();
  const state: State = STATES.find((s) => s.value === sp.get('state'))?.value ?? 'firing';
  const setState = (v: State) => setSp((p) => { const n = new URLSearchParams(p); if (v === 'firing') n.delete('state'); else n.set('state', v); return n; }, { replace: true });
  // 页码先于查询算出（偏移量进查询）；总数取上一次的结果。渲染中直接跟上（不等 effect 提交一遍再重渲染）
  const [total, setTotal] = React.useState<number>();
  const pg = usePage({ total, sizes: PAGE_SIZES, size: INCIDENT_PAGE, reset: [state] });
  const q = useIncidents(state, pg.offset, pg.size);
  if (q.data && q.data.total !== total) setTotal(q.data.total);
  const ack = useAck();
  const { open } = useDrawer();
  const { open: openRouter } = useRouterDrawer();
  const list = q.data?.items ?? [];
  const rows = React.useMemo(() => foldBatches(list), [list]);
  const [opened, setOpened] = React.useState<ReadonlySet<string>>(new Set()); // 展开了的批（以第一条的 id 为键）
  const toggle = (k: string) => setOpened((o) => { const n = new Set(o); if (!n.delete(k)) n.add(k); return n; });
  // 续费类告警的「登记续费」要用机器的订阅信息；续费后下一轮评估（≤30 秒）自动恢复
  const machines = useMachineBriefs().data;
  const [renewId, setRenewId] = React.useState<string | null>(null);
  // 关闭后沿用最近的机器，对话框常驻挂载、播完退场动画
  const renewM = useHeld(renewId, machines?.find((m) => m.id === renewId));
  // 折叠行的对象列写「6 台线路机」：这一批的对象类型（软路由、线路机、节点；混着的写机器）
  const foldNoun = (r: Incident[]) => {
    if (isRouterIncident(r[0])) return '软路由';
    const kinds = new Set(r.map((i) => machines?.find((m) => m.id === (i.subject?.id ?? i.subject_id))?.kind));
    return kinds.size === 1 ? (kinds.has('relay') ? '线路机' : kinds.has(undefined) ? '机器' : '节点') : '机器';
  };
  const row = (i: Incident) => {
      const sid = i.subject?.id ?? i.subject_id;
      const canRenew = i.state === 'firing' && i.rule.startsWith('renewal_') && !!machines?.find((m) => m.id === sid)?.subscription;
      const sum = summaryText(i);
      return (
        <tr key={i.id}>
          <td>{SEV(i)}</td>
          {/* 中控自身的告警（central_ 开头）没有机器可点开：只写对象名「中控」；软路由的（router_ 开头）打开路由抽屉 */}
          <td>{sid && isRouterIncident(i) ? <button className="btn btn-link" style={{ maxWidth: '100%' }} onClick={() => openRouter(sid, routerIncidentTab(i))}><span className="ellipsis">{i.subject?.name ?? i.subject_name}</span></button>
            : sid ? <NodeHover id={sid}><button className="btn btn-link" style={{ maxWidth: '100%' }} onClick={() => open(sid)}><span className="ellipsis">{i.subject?.name ?? i.subject_name}</span></button></NodeHover> : (i.subject_name ?? '—')}</td>
          <td>{i.rule_name}{i.reopen_count > 0 && <span className="sub" data-tip={`恢复后 30 分钟内又复发 ${i.reopen_count} 次，合并为这一条`}>反复 {i.reopen_count + 1} 次</span>}</td>
          <td><span className="cell-main" data-tip={sum}>{sum}</span>{(i.resolved_at || i.ack_at) && <span className="sub">{i.resolved_at
            ? <span data-tip={`恢复于 ${F.dtFull(i.resolved_at)}`}>已恢复 · 持续 {F.span(Date.parse(i.resolved_at) - Date.parse(i.opened_at))}</span>
            : `${i.ack_by ? F.actor(i.ack_by) : ''} 已确认 · ${F.ago(i.ack_at!)}`}</span>}</td>
          <td>{F.dt(i.opened_at)}<span className="sub">{F.ago(i.opened_at)}</span></td>
          <td className="r">{canRenew && <Button size="sm" onClick={() => setRenewId(sid)}><CalendarPlus size={13} />登记续费</Button>}{!canRenew && i.state === 'firing' && !i.ack_at && <Button size="sm" loading={ack.isPending && ack.variables === i.id} disabled={ack.isPending} onClick={() => ack.mutate(i.id, { onSuccess: () => toast.ok('已确认'), onError: toast.err })}><Check size={13} />确认</Button>}</td>
        </tr>
      );
  };
  return (
    <div className="stack" ref={pg.anchor}>
      <PageHead title="告警" note={q.data ? `${q.data.total.toLocaleString('zh-CN')} 条` : undefined}
        tools={<>
          <Segmented label="状态" value={state} onChange={setState} items={STATES} />
          {/* 静默与维护窗口的列表在页面最下面：新建入口放在页头（走 ?silence=new，同命令面板） */}
          <Button onClick={() => setSp((p) => { const n = new URLSearchParams(p); n.set('silence', 'new'); return n; }, { replace: true })}><BellOff />新建静默</Button>
        </>} />
      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
      <Panel flush>
        <TableWrap>
          <table className="dt" style={{ minWidth: 820 }}>
            {/* 末列 120：「登记续费」小按钮 90（边框 2 + 内边距 20 + 图标 14 + 间距 6 + 四个字 48）+ 单元格内边距 12 + 16 = 118，留 2px 余量；多出的从说明列（自适应）让出 */}
            <colgroup><col style={{ width: 100 }} /><col style={{ width: 150 }} /><col style={{ width: 150 }} /><col /><col style={{ width: 110 }} /><col style={{ width: 120 }} /></colgroup>
            <thead><tr><th>级别</th><th>对象</th><th>规则</th><th>说明</th><th>开始</th><th /></tr></thead>
            <tbody>
              {rows.map((r) => {
                if (!Array.isArray(r)) return row(r);
                const k = r[0].id; const on = opened.has(k);
                return (
                  <React.Fragment key={k}>
                    <tr data-open={on ? '' : undefined}>
                      <td>{SEV(r[0])}</td>
                      <td><button type="button" className="btn btn-link inc-fold" aria-expanded={on} data-tip={on ? '收起' : '展开看每一台'} onClick={() => toggle(k)}>{r.length} 台{foldNoun(r)}<ChevronRight /></button></td>
                      <td>{r[0].rule_name}</td>
                      <td>同时出现{r[0].state === 'resolved' && <span className="sub">已恢复</span>}</td>
                      <td>{F.dt(r[0].opened_at)}<span className="sub">{F.ago(r[0].opened_at)}</span></td>
                      <td />
                    </tr>
                    {on && r.map(row)}
                  </React.Fragment>
                );
              })}
            </tbody>
          </table>
          {q.isLoading && <SkeletonRows rows={4} />}
          {q.data && list.length === 0 && <Empty icon={state === 'firing' ? <CheckCircle2 size={22} /> : undefined} text={state === 'firing' ? '没有进行中的告警' : '没有记录'} />}
        </TableWrap>
        <Pagination pg={pg} unit="条" />
      </Panel>
      {renewM?.subscription && <RenewDialog m={renewM} s={renewM.subscription} open={!!renewId} onOpenChange={(o) => !o && setRenewId(null)} />}
      <Silences />
    </div>
  );
}

const ALL = '__all__';
const AHEAD_DAYS = 90; // 最多提前 90 天预约（同中控）
const MIN = 60_000;
type StartMode = 'now' | 'at';
const START_ITEMS: { value: StartMode; label: string }[] = [{ value: 'now', label: '现在' }, { value: 'at', label: '指定时间' }];
/** 常用时长（分钟），默认 4 小时；别的时长用「自定义」填分钟数 */
const DURATIONS: DurationItem[] = [
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
      <Panel title="静默与维护窗口" count={list.length} sub={<InfoTip text="窗口内不会产生告警" />} tools={list.length === 0 && !q.isLoading ? <Button onClick={() => { setPreset(''); setOpen(true); }}><Plus />新建静默</Button> : undefined} flush>
        {q.isError && <div style={{ padding: 'var(--sp-12) var(--card-pad)' }}><ErrorNote error={q.error} onRetry={() => void q.refetch()} /></div>}
        {q.isLoading ? <SkeletonRows rows={2} head /> : list.length === 0 && !q.isError ? <Empty icon={<BellOff size={22} />} text="没有进行中或已预约的静默" /> : (
          <TableWrap>
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
                    <td className="c"><IconButton label={s.is_maintenance ? '删除维护窗口' : '删除静默'} size="sm" onClick={() => setAskDel(s)}><Trash2 size={14} /></IconButton></td>
                  </tr>
                );
              })}</tbody>
            </table>
          </TableWrap>
        )}
      </Panel>
      <SilenceDialog open={open} onOpenChange={setOpen} preset={preset} />
      <ConfirmDialog open={!!askDel} onOpenChange={(o) => !o && setAskDel(null)} title={`删除「${delHeld?.subject_name ?? '全部机器'}」的${delHeld?.is_maintenance ? '维护窗口' : '静默'}？`} confirmLabel="删除" danger
        description={delHeld && Date.parse(delHeld.starts_at) > now
          ? '删除即取消这次预约'
          : '仍异常的会马上发出告警'}
        onConfirm={async () => { await del.mutateAsync(askDel!.id); toast.ok(askDel!.is_maintenance ? '已删除维护窗口' : '已删除静默'); }} />
    </>
  );
}

/** 新建静默。每次打开重新挂载（resetOnOpen）：对象按 preset 预选，其余回到默认（现在开始、4 小时、不标记为维护——单台机器的维护走卡片菜单的「维护窗口…」）。
 *  选“全部机器”时提交前再确认一次（写明起止） */
const SilenceDialog = resetOnOpen(function SilenceDialog({ open, onOpenChange, preset }: { open: boolean; onOpenChange: (o: boolean) => void; preset: string }) {
  const add = useAddSilence();
  const [subject, setSubject] = React.useState(preset);
  const [mode, setMode] = React.useState<StartMode>('now'); const [start, setStart] = React.useState(nextHour);
  const d = useDuration(240, SILENCE_MAX_MIN);
  const [maint, setMaint] = React.useState(false); const [comment, setComment] = React.useState('');
  const what = maint ? '维护窗口' : '静默';
  const [askAll, setAskAll] = React.useState(false);
  // 指定时间时走表：对话框开着不动，选好的时间过去了也要提示
  useNow(open && mode === 'at', 10_000);
  const now = Date.now();
  const startMs = Date.parse(start); // datetime-local 的值不带时区，按本地时间解析；没填完时为 NaN
  // 当前这一分钟还算数（同 fleetctl）
  const startErr = mode === 'now' ? undefined : Number.isNaN(startMs) ? '请选择开始时间' : startMs + MIN <= now ? '这个时间已过，可选「现在」'
    : startMs > now + AHEAD_DAYS * 1440 * MIN ? `最多提前 ${AHEAD_DAYS} 天预约` : undefined;
  const { minutes } = d;
  const valid = !!subject && !startErr && d.valid;
  const startAt = () => (mode === 'now' ? Date.now() : startMs);
  const body = () => {
    const s = startAt();
    return { subject_id: subject === ALL ? null : subject, starts_at: new Date(s).toISOString(), ends_at: new Date(s + minutes * MIN).toISOString(), is_maintenance: maint, comment: comment.trim() };
  };
  const submit = () => add.mutate(body(), { onSuccess: () => { toast.ok(`已创建${what}`); onOpenChange(false); }, onError: toast.err });
  const desc = !d.valid ? (mode === 'now' ? '从现在起' : `${dtMs(startMs)} 起`) : mode === 'now' ? `从现在起 ${F.span(minutes * MIN)}` : `${dtMs(startMs)} → ${dtMs(startMs + minutes * MIN)}`;
  return (
    <>
      <FormDialog open={open} onOpenChange={onOpenChange} title={`新建${what}`} desc={desc} submitLabel="创建" busy={add.isPending} valid={valid}
        dirty={subject !== preset || mode !== 'now' || d.dur !== 240 || maint || !!comment.trim()}
        onSubmit={() => (subject === ALL ? setAskAll(true) : submit())}>
        <Field label="对象"><SubjectSelect value={subject} onChange={setSubject} /></Field>
        {subject === ALL && <Note tone="warn">全部机器不发告警</Note>}
        <div className="field">
          <span className="label">开始</span>
          <div className="row">
            <Segmented label="开始" value={mode} onChange={setMode} items={START_ITEMS} />
            {mode === 'at' && <Input type="datetime-local" aria-label="开始时间" value={start} min={localInput(now)} max={localInput(now + AHEAD_DAYS * 1440 * MIN)} onChange={(e) => setStart(e.target.value)} aria-invalid={!!startErr} style={{ flex: '1 1 200px', width: 'auto' }} />}
          </div>
          {startErr ? <div className="hint is-error">{startErr}</div> : mode === 'at' && <div className="hint">最多提前 {AHEAD_DAYS} 天 <InfoTip text="按这台电脑的本地时间" /></div>}
        </div>
        <DurationField d={d} items={DURATIONS} />
        <div className="row" style={{ gap: 4 }}><CheckRow checked={maint} onChange={setMaint}>标记为维护</CheckRow><InfoTip text="卡片显示「维护」，这条叫维护窗口" /></div>
        <Field label="备注"><Input value={comment} onChange={(e) => setComment(e.target.value)} placeholder="例如：换内核" /></Field>
      </FormDialog>
      <ConfirmDialog open={askAll} onOpenChange={setAskAll} title="静默全部机器？" confirmLabel="静默全部机器" danger
        description={`从${mode === 'now' ? '现在' : ` ${dtMs(startMs)} `}到 ${dtMs(startAt() + minutes * MIN)}，所有机器都不发告警`}
        onConfirm={async () => { await add.mutateAsync(body()); toast.ok(`已创建${what}`); onOpenChange(false); }} />
    </>
  );
});

/** 对象搜索同命令面板：名称前缀优先、连续子串（搜「C-01」不命中「C-10」）。keywords 是 [名称, 分组…] */
const subjectFilter = (_v: string, s: string, kw?: string[]) => matchTier(sv(kw?.[0] ?? '', '', kw?.slice(1).join(' ') ?? ''), s.trim().toLowerCase());

/** 静默对象：可搜索的下拉（名称、团队都能搜），按线路机 / 各团队分组；最前一项「全部机器」。接口一次只收一个对象 */
function SubjectSelect({ value, onChange }: { value: string; onChange: (v: string) => void }) {
  const machines = useMachineBriefs().data;
  const [open, setOpen] = React.useState(false);
  // 分组随机器列表变（结构共享：只改读数的推送不换引用）
  const groups = React.useMemo(() => {
    const by = new Map<string, MachineBrief[]>();
    for (const m of machines ?? []) {
      const g = m.kind === 'relay' ? '线路机' : m.team?.name ?? '未分团队';
      by.set(g, [...(by.get(g) ?? []), m]);
    }
    return [...by].sort(([a], [b]) => (a === '线路机' ? -1 : b === '线路机' ? 1 : collator.compare(a, b)));
  }, [machines]);
  const name = value === ALL ? '全部机器' : machines?.find((m) => m.id === value)?.name;
  const pick = (v: string) => { setOpen(false); onChange(v); };
  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <button type="button" className="select-trigger cs-trigger" aria-label="对象">
          <span className={name ? undefined : 't-muted'}>{name ?? '选择机器'}</span><ChevronDown />
        </button>
      </PopoverTrigger>
      <PopoverContent className="cs-pop is-wide" align="start">
        {/* 列表只在打开时建（一两百台），关着不占渲染 */}
        {open && (
          <Command loop filter={subjectFilter}>
            <CommandInput placeholder="搜索机器或团队" />
            <CommandList>
              <CommandEmpty>没有这台机器</CommandEmpty>
              <CommandItem value={ALL} keywords={['全部机器', '所有']} onSelect={pick}><span className="cmd-main">全部机器</span><span className="cmd-aside">所有告警</span>{value === ALL && <Check className="cs-check" />}</CommandItem>
              {groups.map(([g, list]) => (
                <CommandGroup key={g} heading={g}>
                  {list.map((m) => <CommandItem key={m.id} value={m.id} keywords={[m.name, g]} onSelect={pick}><span className="cmd-main">{m.name}</span>{m.id === value && <Check className="cs-check" />}</CommandItem>)}
                </CommandGroup>
              ))}
            </CommandList>
          </Command>
        )}
      </PopoverContent>
    </Popover>
  );
}
