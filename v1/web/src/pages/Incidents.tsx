// 告警：事件列表（确认 / 查看机器，服务端分页）+ 静默与维护窗口。列表保持服务端顺序（严重度优先），前端不重排。
import * as React from 'react';
import { useSearchParams } from 'react-router-dom';
import { BellOff, CalendarPlus, Check, CheckCircle2, Plus, Trash2, Wrench } from 'lucide-react';
import { RenewDialog } from '@/features/BillingTab';
import { INCIDENT_PAGE, useAck, useAddSilence, useDelSilence, useIncidents, useMachineBriefs, useSilences } from '@/lib/queries';
import { usePage } from '@/lib/usePage';
import { Pagination } from '@/components/app/Pagination';
import { useDrawer } from '@/lib/drawer';
import * as F from '@/lib/format';
import type { Incident, Silence } from '@/lib/types';
import { resetOnOpen, useHeld } from '@/lib/useHeld';
import { Button, IconButton } from '@/components/ui/button';
import { CheckRow, Field, Input } from '@/components/ui/form';
import { Select } from '@/components/ui/select';
import { Segmented, Trunc } from '@/components/ui/overlays';
import { SkeletonRows } from '@/components/ui/skeleton';
import { toast } from '@/components/ui/toast';
import { ConfirmDialog, FormDialog } from '@/components/app/dialogs';
import { NodeHover } from '@/components/app/NodeHover';
import { Empty, ErrorNote, Note, PageHead, Panel, Pill } from '@/components/app/primitives';

const PAGE_SIZES = [20, INCIDENT_PAGE, 100];
const SEV = (i: Incident) => (i.state === 'resolved' ? <Pill tone="ok" dot="ring">已恢复</Pill> : i.severity === 'critical' ? <Pill tone="bad">严重</Pill> : i.severity === 'info' ? <Pill tone="info">提示</Pill> : <Pill tone="warn">警告</Pill>);

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
                    <td>{sid ? <NodeHover id={sid}><button className="btn btn-link" style={{ maxWidth: '100%' }} onClick={() => open(sid)}><span className="ellipsis">{i.subject?.name ?? i.subject_name}</span></button></NodeHover> : '—'}</td>
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
  const list = q.data ?? [];
  return (
    <>
      <Panel title="静默与维护窗口" count={list.length} sub="窗口内不会产生告警" tools={<Button onClick={() => { setPreset(''); setOpen(true); }}><Plus />新建静默</Button>} flush>
        {q.isError && <div style={{ padding: 'var(--sp-12) var(--card-pad)' }}><ErrorNote error={q.error} onRetry={() => void q.refetch()} /></div>}
        {q.isLoading ? <SkeletonRows rows={2} one head /> : list.length === 0 && !q.isError ? <Empty icon={<BellOff size={22} />} text="当前没有生效的静默" hint="维护机器前先建一个，期间不会发告警" /> : (
          <div className="table-wrap">
            <table className="dt is-one" style={{ minWidth: 640 }}>
              <colgroup><col style={{ width: 140 }} /><col style={{ width: 110 }} /><col style={{ width: 230 }} /><col /><col style={{ width: 64 }} /></colgroup>
              <thead><tr><th>对象</th><th>类型</th><th>时间</th><th>备注</th><th /></tr></thead>
              <tbody>{list.map((s) => (
                <tr key={s.id}><td className="strong">{s.subject_name ?? '全部机器'}</td><td>{s.is_maintenance ? <Pill tone="info" dot="none"><Wrench size={11} />维护</Pill> : <Pill>静默</Pill>}</td>
                  <td>{F.dt(s.starts_at)} → {F.dt(s.ends_at)}</td><td><Trunc className="cell-main">{s.comment}</Trunc></td>
                  <td className="c"><IconButton label="删除静默" size="sm" onClick={() => setAskDel(s)}><Trash2 size={14} /></IconButton></td></tr>
              ))}</tbody>
            </table>
          </div>
        )}
      </Panel>
      <SilenceDialog open={open} onOpenChange={setOpen} preset={preset} />
      <ConfirmDialog open={!!askDel} onOpenChange={(o) => !o && setAskDel(null)} title={`删除「${delHeld?.subject_name ?? '全部机器'}」的${delHeld?.is_maintenance ? '维护窗口' : '静默'}？`} confirmLabel="删除" danger
        description={`删除后立即恢复告警：${delHeld?.subject_name ?? '所有机器'}若仍异常，会马上发出通知。`}
        onConfirm={async () => { await del.mutateAsync(askDel!.id); toast.ok('已删除静默'); }} />
    </>
  );
}

/** 新建静默。每次打开重新挂载（resetOnOpen）：对象按 preset 预选，其余回到默认。选“全部机器”时提交前再确认一次 */
const SilenceDialog = resetOnOpen(function SilenceDialog({ open, onOpenChange, preset }: { open: boolean; onOpenChange: (o: boolean) => void; preset: string }) {
  const add = useAddSilence();
  const machines = useMachineBriefs().data;
  // 选项随机器列表变（结构共享：只改读数的推送不换引用），不随每次输入重建：几百台时 Select 不必每敲一个字重渲全部选项
  const options = React.useMemo(() => [{ value: ALL, label: '全部机器（影响所有告警）' }, ...(machines ?? []).map((m) => ({ value: m.id, label: m.name }))], [machines]);
  const [subject, setSubject] = React.useState(preset); const [hours, setHours] = React.useState('4');
  const [maint, setMaint] = React.useState(true); const [comment, setComment] = React.useState('');
  const [askAll, setAskAll] = React.useState(false);
  const h = Number(hours);
  const hoursErr = hours !== '' && (h < 1 || h > MAX_HOURS) ? `时长应在 1–${MAX_HOURS} 小时之间` : undefined;
  const valid = !!subject && h >= 1 && h <= MAX_HOURS;
  const body = () => {
    const now = Date.now();
    return { subject_id: subject === ALL ? null : subject, starts_at: new Date(now).toISOString(), ends_at: new Date(now + h * 3600_000).toISOString(), is_maintenance: maint, comment: comment.trim() };
  };
  const submit = () => add.mutate(body(), { onSuccess: () => { toast.ok('已创建静默'); onOpenChange(false); }, onError: toast.err });
  return (
    <>
      <FormDialog open={open} onOpenChange={onOpenChange} title="新建静默" desc="从现在开始，指定时长内所选对象不会触发告警。" submitLabel="创建" busy={add.isPending} valid={valid}
        onSubmit={() => (subject === ALL ? setAskAll(true) : submit())}>
        <Field label="对象" hint="必须选择：是哪台机器要静默"><Select value={subject} onChange={setSubject} placeholder="选择机器" options={options} /></Field>
        {subject === ALL && <Note tone="warn">静默“全部机器”会让所有机器在这段时间内都不发告警。提交时需要再确认一次。</Note>}
        <Field label="时长（小时）" hint={`最长 ${MAX_HOURS} 小时（30 天）`} error={hoursErr}><Input inputMode="numeric" value={hours} onChange={(e) => setHours(e.target.value.replace(/\D/g, ''))} aria-invalid={!!hoursErr} /></Field>
        <CheckRow checked={maint} onChange={setMaint}>标记为维护（卡片显示“维护中”）</CheckRow>
        <Field label="备注"><Input value={comment} onChange={(e) => setComment(e.target.value)} placeholder="例如：换内核" /></Field>
      </FormDialog>
      <ConfirmDialog open={askAll} onOpenChange={setAskAll} title="静默全部机器？" confirmLabel="静默全部机器" danger
        description={`接下来 ${hours || 0} 小时内，所有机器的告警都不会发出。确认是要这样做吗？`}
        onConfirm={async () => { await add.mutateAsync(body()); toast.ok('已创建静默'); onOpenChange(false); }} />
    </>
  );
});
