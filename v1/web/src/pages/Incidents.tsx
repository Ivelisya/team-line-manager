// 告警：事件列表（确认 / 查看机器）+ 静默与维护窗口。
import * as React from 'react';
import { BellOff, Check, Plus, Trash2, Wrench } from 'lucide-react';
import { useAck, useAddSilence, useDelSilence, useIncidents, useMachines, useSilences } from '@/lib/queries';
import { useDrawer } from '@/lib/drawer';
import * as F from '@/lib/format';
import type { Incident } from '@/lib/types';
import { Button, IconButton } from '@/components/ui/button';
import { CheckRow, Field, Input } from '@/components/ui/form';
import { Select } from '@/components/ui/select';
import { Segmented } from '@/components/ui/overlays';
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogTitle } from '@/components/ui/dialog';
import { toast } from '@/components/ui/toast';
import { Empty, Note, PageHead, Panel, Pill } from '@/components/app/primitives';

const RULES: Record<string, string> = { agent_offline: 'agent 离线', tcp_fail: 'TCP 探测失败', cpu_high: 'CPU 过高', apply_pending: '配置未下发', latency_high: '延迟过高', no_data: '无数据' };
const SEV = (i: Incident) => (i.state === 'resolved' ? <Pill tone="ok" dot="ring">已恢复</Pill> : i.severity === 'critical' ? <Pill tone="bad">严重</Pill> : i.severity === 'info' ? <Pill tone="info">提示</Pill> : <Pill tone="warn">警告</Pill>);

export function IncidentsPage() {
  const [state, setState] = React.useState<'firing' | 'resolved' | 'all'>('firing');
  const q = useIncidents(state);
  const ack = useAck();
  const { open } = useDrawer();
  const list = [...(q.data ?? [])].sort((a, b) => +new Date(b.opened_at) - +new Date(a.opened_at));
  return (
    <div className="stack">
      <PageHead title="告警" note={q.data ? `${list.length} 条` : undefined}
        tools={<Segmented label="状态" value={state} onChange={setState} items={[{ value: 'firing', label: '进行中' }, { value: 'resolved', label: '已恢复' }, { value: 'all', label: '全部' }]} />} />
      {q.isError && <Note tone="bad">{(q.error as Error).message}</Note>}
      <Panel flush>
        <div className="table-wrap">
          <table className="dt" style={{ minWidth: 820 }}>
            <colgroup><col style={{ width: 88 }} /><col style={{ width: 150 }} /><col style={{ width: 130 }} /><col /><col style={{ width: 110 }} /><col style={{ width: 130 }} /></colgroup>
            <thead><tr><th>级别</th><th>对象</th><th>规则</th><th>说明</th><th>开始</th><th /></tr></thead>
            <tbody>
              {list.map((i) => (
                <tr key={i.id}>
                  <td>{SEV(i)}</td>
                  <td>{i.subject?.id ?? i.subject_id ? <button className="btn btn-link" style={{ maxWidth: '100%' }} onClick={() => open((i.subject?.id ?? i.subject_id)!)}><span className="ellipsis">{i.subject?.name ?? i.subject_name}</span></button> : '—'}</td>
                  <td>{RULES[i.rule] ?? i.rule}</td>
                  <td><span className="cell-main">{i.summary}</span><span className="sub">{i.resolved_at ? `恢复于 ${F.dt(i.resolved_at)}` : i.ack_at ? `${i.ack_by ?? ''} 已确认 · ${F.ago(i.ack_at)}` : '未确认'}</span></td>
                  <td>{F.dt(i.opened_at)}<span className="sub">{F.ago(i.opened_at)}</span></td>
                  <td className="r">{i.state === 'firing' && !i.ack_at && <Button size="sm" loading={ack.isPending && ack.variables === i.id} onClick={() => ack.mutate(i.id, { onSuccess: () => toast.ok('已确认'), onError: toast.err })}><Check size={13} />确认</Button>}</td>
                </tr>
              ))}
            </tbody>
          </table>
          {q.data && list.length === 0 && <Empty text={state === 'firing' ? '没有进行中的告警' : '没有记录'} />}
        </div>
      </Panel>
      <Silences />
    </div>
  );
}

function Silences() {
  const q = useSilences(); const del = useDelSilence(); const add = useAddSilence();
  const machines = useMachines('live').data ?? [];
  const [open, setOpen] = React.useState(false);
  const [subject, setSubject] = React.useState(''); const [hours, setHours] = React.useState('4');
  const [maint, setMaint] = React.useState(true); const [comment, setComment] = React.useState('');
  React.useEffect(() => { if (open) { setSubject(''); setHours('4'); setMaint(true); setComment(''); } }, [open]);
  const list = q.data ?? [];
  const h = Number(hours);
  const submit = () => {
    const now = Date.now();
    add.mutate({ subject_id: subject || null, starts_at: new Date(now).toISOString(), ends_at: new Date(now + h * 3600_000).toISOString(), is_maintenance: maint, comment: comment.trim() },
      { onSuccess: () => { toast.ok('已创建静默'); setOpen(false); }, onError: toast.err });
  };
  return (
    <>
      <Panel title="静默与维护窗口" count={list.length} sub="窗口内不会产生告警" tools={<Button onClick={() => setOpen(true)}><Plus />新建静默</Button>} flush>
        {list.length === 0 ? <Empty icon={<BellOff size={28} />} text="当前没有生效的静默" /> : (
          <div className="table-wrap">
            <table className="dt is-one" style={{ minWidth: 640 }}>
              <colgroup><col style={{ width: 140 }} /><col style={{ width: 110 }} /><col style={{ width: 230 }} /><col /><col style={{ width: 52 }} /></colgroup>
              <thead><tr><th>对象</th><th>类型</th><th>时间</th><th>备注</th><th /></tr></thead>
              <tbody>{list.map((s) => (
                <tr key={s.id}><td className="strong">{s.subject_name ?? '全部'}</td><td>{s.is_maintenance ? <Pill tone="info"><Wrench size={11} />维护</Pill> : <Pill>静默</Pill>}</td>
                  <td>{F.dt(s.starts_at)} → {F.dt(s.ends_at)}</td><td><span className="cell-main">{s.comment}</span></td>
                  <td className="c"><IconButton label="删除静默" size="sm" onClick={() => del.mutate(s.id, { onSuccess: () => toast.ok('已删除静默'), onError: toast.err })}><Trash2 size={14} /></IconButton></td></tr>
              ))}</tbody>
            </table>
          </div>
        )}
      </Panel>
      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent>
          <div style={{ display: 'grid', gap: 6 }}><DialogTitle>新建静默</DialogTitle><DialogDescription>从现在开始，指定时长内这台机器不会触发告警。</DialogDescription></div>
          <Field label="对象"><Select value={subject} onChange={setSubject} allowEmpty="全部机器" options={machines.map((m) => ({ value: m.id, label: m.name }))} /></Field>
          <Field label="时长（小时）"><Input inputMode="numeric" value={hours} onChange={(e) => setHours(e.target.value.replace(/\D/g, ''))} /></Field>
          <CheckRow checked={maint} onChange={setMaint}>标记为维护（卡片显示“维护中”）</CheckRow>
          <Field label="备注"><Input value={comment} onChange={(e) => setComment(e.target.value)} placeholder="例如：换内核" /></Field>
          <DialogFooter><Button onClick={() => setOpen(false)}>取消</Button><Button variant="primary" loading={add.isPending} disabled={!h || h < 1} onClick={submit}>创建</Button></DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
}
