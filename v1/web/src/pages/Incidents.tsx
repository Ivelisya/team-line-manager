// 告警：事件列表（确认 / 查看机器，服务端分页）+ 静默与维护窗口。列表保持服务端顺序（严重度优先），前端不重排。
import * as React from 'react';
import { useSearchParams } from 'react-router-dom';
import { BellOff, CalendarPlus, Check, CheckCircle2, Plus, Trash2, Wrench } from 'lucide-react';
import { RenewDialog } from '@/features/BillingTab';
import { INCIDENT_PAGE, useAck, useAddSilence, useDelSilence, useIncidents, useMachines, useSilences } from '@/lib/queries';
import { usePage } from '@/lib/usePage';
import { Pagination } from '@/components/app/Pagination';
import { useDrawer } from '@/lib/drawer';
import * as F from '@/lib/format';
import type { Incident } from '@/lib/types';
import { Button, IconButton } from '@/components/ui/button';
import { CheckRow, Field, Input } from '@/components/ui/form';
import { Select } from '@/components/ui/select';
import { Segmented, Trunc } from '@/components/ui/overlays';
import { SkeletonRows } from '@/components/ui/skeleton';
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogTitle, DialogHeader } from '@/components/ui/dialog';
import { toast } from '@/components/ui/toast';
import { ConfirmDialog } from '@/components/app/dialogs';
import { NodeHover } from '@/components/app/NodeHover';
import { Empty, ErrorNote, Note, PageHead, Panel, Pill } from '@/components/app/primitives';

const SEV = (i: Incident) => (i.state === 'resolved' ? <Pill tone="ok" dot="ring">已恢复</Pill> : i.severity === 'critical' ? <Pill tone="bad">严重</Pill> : i.severity === 'info' ? <Pill tone="info">提示</Pill> : <Pill tone="warn">警告</Pill>);

export function IncidentsPage() {
  const [state, setState] = React.useState<'firing' | 'resolved' | 'all'>('firing');
  // 页码先于查询算出（偏移量进查询）；总数取上一次的结果
  const [total, setTotal] = React.useState<number>();
  const pg = usePage({ total, sizes: [20, INCIDENT_PAGE, 100], size: INCIDENT_PAGE, reset: [state] });
  const q = useIncidents(state, pg.offset, pg.size);
  React.useEffect(() => { if (q.data) setTotal(q.data.total); }, [q.data]);
  const ack = useAck();
  const { open } = useDrawer();
  const list = q.data?.items ?? [];
  // 续费类告警的「登记续费」要用机器的订阅信息；续费后下一轮评估（≤30 秒）自动恢复
  const machines = useMachines('live').data;
  const [renewId, setRenewId] = React.useState<string | null>(null);
  const renewM = machines?.find((m) => m.id === renewId);
  return (
    <div className="stack" ref={pg.anchor}>
      <PageHead title="告警" note={q.data ? `${q.data.total} 条` : undefined}
        tools={<Segmented label="状态" value={state} onChange={setState} items={[{ value: 'firing', label: '进行中' }, { value: 'resolved', label: '已恢复' }, { value: 'all', label: '全部' }]} />} />
      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
      <Panel flush>
        <div className="table-wrap">
          <table className="dt" style={{ minWidth: 820 }}>
            <colgroup><col style={{ width: 88 }} /><col style={{ width: 150 }} /><col style={{ width: 150 }} /><col /><col style={{ width: 110 }} /><col style={{ width: 110 }} /></colgroup>
            <thead><tr><th>级别</th><th>对象</th><th>规则</th><th>说明</th><th>开始</th><th /></tr></thead>
            <tbody>
              {list.map((i) => {
                const sid = i.subject?.id ?? i.subject_id;
                const canRenew = i.state === 'firing' && i.rule.startsWith('renewal_') && !!machines?.find((m) => m.id === sid)?.subscription;
                return (
                  <tr key={i.id}>
                    <td>{SEV(i)}</td>
                    <td>{sid ? <NodeHover id={sid}><button className="btn btn-link" style={{ maxWidth: '100%' }} onClick={() => open(sid)}><span className="ellipsis">{i.subject?.name ?? i.subject_name}</span></button></NodeHover> : '—'}</td>
                    <td>{i.rule_name}{i.reopen_count > 0 && <span className="sub" title={`恢复后 30 分钟内又复发 ${i.reopen_count} 次，合并为这一条`}>反复 {i.reopen_count + 1} 次</span>}</td>
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
      {renewM?.subscription && <RenewDialog m={renewM} s={renewM.subscription} open onOpenChange={(o) => !o && setRenewId(null)} />}
      <Silences />
    </div>
  );
}

const ALL = '__all__';
const MAX_HOURS = 720; // 30 天

function Silences() {
  const q = useSilences(); const del = useDelSilence(); const add = useAddSilence();
  const machines = useMachines('live').data ?? [];
  const [sp, setSp] = useSearchParams();
  const [open, setOpen] = React.useState(false);
  const [askAll, setAskAll] = React.useState(false);
  const [subject, setSubject] = React.useState(''); const [hours, setHours] = React.useState('4');
  const [maint, setMaint] = React.useState(true); const [comment, setComment] = React.useState('');
  // 命令面板 / 右键菜单：/incidents?silence=<机器id> 直接打开“新建静默”并选好对象；silence=new 不预选
  React.useEffect(() => {
    const want = sp.get('silence');
    if (!want) return;
    setSubject(want === 'new' ? '' : want); setHours('4'); setMaint(true); setComment(''); setOpen(true);
    setSp((p) => { const n = new URLSearchParams(p); n.delete('silence'); return n; }, { replace: true });
  }, [sp, setSp]);
  const list = q.data ?? [];
  const h = Number(hours);
  const hoursErr = hours !== '' && (h < 1 || h > MAX_HOURS) ? `时长应在 1–${MAX_HOURS} 小时之间` : undefined;
  const valid = !!subject && h >= 1 && h <= MAX_HOURS;
  const submit = () => {
    const now = Date.now();
    add.mutate({ subject_id: subject === ALL ? null : subject, starts_at: new Date(now).toISOString(), ends_at: new Date(now + h * 3600_000).toISOString(), is_maintenance: maint, comment: comment.trim() },
      { onSuccess: () => { toast.ok('已创建静默'); setOpen(false); setAskAll(false); }, onError: toast.err });
  };
  return (
    <>
      <Panel title="静默与维护窗口" count={list.length} sub="窗口内不会产生告警" tools={<Button onClick={() => { setSubject(''); setHours('4'); setMaint(true); setComment(''); setOpen(true); }}><Plus />新建静默</Button>} flush>
        {q.isError && <div style={{ padding: 'var(--sp-12) var(--card-pad)' }}><ErrorNote error={q.error} onRetry={() => void q.refetch()} /></div>}
        {q.isLoading ? <SkeletonRows rows={2} /> : list.length === 0 && !q.isError ? <Empty icon={<BellOff size={22} />} text="当前没有生效的静默" hint="维护机器前先建一个，期间不会发告警" /> : (
          <div className="table-wrap">
            <table className="dt is-one" style={{ minWidth: 640 }}>
              <colgroup><col style={{ width: 140 }} /><col style={{ width: 110 }} /><col style={{ width: 230 }} /><col /><col style={{ width: 64 }} /></colgroup>
              <thead><tr><th>对象</th><th>类型</th><th>时间</th><th>备注</th><th /></tr></thead>
              <tbody>{list.map((s) => (
                <tr key={s.id}><td className="strong">{s.subject_name ?? '全部机器'}</td><td>{s.is_maintenance ? <Pill tone="info"><Wrench size={11} />维护</Pill> : <Pill>静默</Pill>}</td>
                  <td>{F.dt(s.starts_at)} → {F.dt(s.ends_at)}</td><td><Trunc className="cell-main">{s.comment}</Trunc></td>
                  <td className="c"><IconButton label="删除静默" size="sm" disabled={del.isPending} onClick={() => del.mutate(s.id, { onSuccess: () => toast.ok('已删除静默'), onError: toast.err })}><Trash2 size={14} /></IconButton></td></tr>
              ))}</tbody>
            </table>
          </div>
        )}
      </Panel>
      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent>
          <DialogHeader><DialogTitle>新建静默</DialogTitle><DialogDescription>从现在开始，指定时长内所选对象不会触发告警。</DialogDescription></DialogHeader>
          <Field label="对象" hint="必须选择：是哪台机器要静默"><Select value={subject} onChange={setSubject} placeholder="选择机器" options={[{ value: ALL, label: '全部机器（影响所有告警）' }, ...machines.map((m) => ({ value: m.id, label: m.name }))]} /></Field>
          {subject === ALL && <Note tone="warn">静默“全部机器”会让所有机器在这段时间内都不发告警。提交时需要再确认一次。</Note>}
          <Field label="时长（小时）" hint={`最长 ${MAX_HOURS} 小时（30 天）`} error={hoursErr}><Input inputMode="numeric" value={hours} onChange={(e) => setHours(e.target.value.replace(/\D/g, ''))} aria-invalid={!!hoursErr} /></Field>
          <CheckRow checked={maint} onChange={setMaint}>标记为维护（卡片显示“维护中”）</CheckRow>
          <Field label="备注"><Input value={comment} onChange={(e) => setComment(e.target.value)} placeholder="例如：换内核" /></Field>
          <DialogFooter><Button onClick={() => setOpen(false)}>取消</Button><Button variant="primary" loading={add.isPending} disabled={!valid} onClick={() => (subject === ALL ? setAskAll(true) : submit())}>创建</Button></DialogFooter>
        </DialogContent>
      </Dialog>
      <ConfirmDialog open={askAll} onOpenChange={setAskAll} title="静默全部机器？" confirmLabel="静默全部机器" danger
        description={`接下来 ${hours || 0} 小时内，所有机器的告警都不会发出。确认是要这样做吗？`}
        onConfirm={async () => { await add.mutateAsync({ subject_id: null, starts_at: new Date().toISOString(), ends_at: new Date(Date.now() + h * 3600_000).toISOString(), is_maintenance: maint, comment: comment.trim() }); toast.ok('已创建静默'); setOpen(false); }} />
    </>
  );
}
