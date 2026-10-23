// 设置「探测点」：放在靠近客户的地方（如香港）的小机器，替中控测线路机的客户入口通不通。
// 列表（状态、最近上报、版本、到各线路机的平均延迟）、主探测来源（单选：中控或某个探测点）、中控自己的检查开关、新增（打印一次性安装命令）。
// 经 SSH 安装、升级用 fleetctl（要等、要看步骤，见 OPS.md「探测点」）。
import * as React from 'react';
import { Plus, Radar, RefreshCw, Trash2 } from 'lucide-react';
import { useAddProbePoint, useDelProbePoint, usePatchProbePoint, useProbePoints, useReachSource, useReinstallProbePoint, useSaveReachSource, type Bootstrap, type ProbePoint } from '@/lib/probePoints';
import * as F from '@/lib/format';
import { Button, IconButton } from '@/components/ui/button';
import { Field, InfoTip, Input, Switch } from '@/components/ui/form';
import { SkeletonRows } from '@/components/ui/skeleton';
import { toast } from '@/components/ui/toast';
import { ConfirmDialog, FormDialog } from '@/components/app/dialogs';
import { CopyButton, Empty, ErrorNote, Note, Panel, Pill, TableWrap } from '@/components/app/primitives';

const STATE: Record<ProbePoint['state'], { label: string; tone: 'ok' | 'bad' | 'neutral' | 'warn' }> = {
  online: { label: '在线', tone: 'ok' }, offline: { label: '离线', tone: 'bad' }, pending: { label: '等待安装', tone: 'warn' }, disabled: { label: '已停用', tone: 'neutral' },
};
const SOURCE_TIP = '线路机卡片的心跳格、入口表的「连不上」、「客户入口连不上」告警都看这一路。\n主来源的探测点离线（3 分钟没上报）时：中控的检查开着就暂由中控代替（连不上照报，连得上不算恢复），关着就标灰';

export function ProbeTab() {
  const src = useReachSource(); const q = useProbePoints();
  const save = useSaveReachSource();
  const [adding, setAdding] = React.useState(false);
  const [boot, setBoot] = React.useState<{ name: string; b: Bootstrap } | null>(null);
  const [del, setDel] = React.useState<ProbePoint | null>(null);
  const [cleanup, setCleanup] = React.useState<string | null>(null);
  const patch = usePatchProbePoint(); const reinstall = useReinstallProbePoint(); const remove = useDelProbePoint();
  const list = q.data ?? [];
  const s = src.data;
  const pick = (primary: string) => save.mutate({ primary, ...(primary === 'central' ? { central_enabled: true } : {}) }, { onSuccess: () => toast.ok('主探测来源已保存'), onError: toast.err });
  return (
    <div className="stack">
      <Panel title={<>主探测来源<InfoTip text={SOURCE_TIP} /></>}>
        {src.isError && <ErrorNote error={src.error} onRetry={() => void src.refetch()} />}
        {!s ? <SkeletonRows rows={2} /> : (
          <div className="stack" style={{ gap: 'var(--sp-12)' }}>
            <div role="radiogroup" className="row" style={{ gap: 'var(--sp-16)', flexWrap: 'wrap' }}>
              {[{ id: 'central', label: '中控（德国）', disabled: false }, ...list.map((p) => ({ id: p.id, label: p.label, disabled: !p.enabled }))].map((o) => (
                <label key={o.id} className="row" style={{ gap: 6, opacity: o.disabled ? 0.5 : 1 }}>
                  <input type="radio" name="reach-primary" checked={s.primary === o.id} disabled={o.disabled || save.isPending} onChange={() => pick(o.id)} />{o.label}
                </label>
              ))}
            </div>
            <label className="row" style={{ gap: 8 }}>
              <Switch checked={s.central_enabled} disabled={save.isPending || s.primary === 'central'} onCheckedChange={(v) => save.mutate({ central_enabled: v }, { onError: toast.err })} />
              中控自己的检查<InfoTip text="中控每 30 秒从德国连一次各线路机的客户入口。主来源是中控时不能关；主来源是探测点时开着它，探测点离线时由它代替" />
            </label>
            {s.effective.state === 'fallback' && <Note tone="warn">{s.effective.primary_label}离线，线路机的连通性暂由中控代替：连不上照报，连得上不算恢复</Note>}
            {s.effective.state === 'none' && <Note tone="bad">{s.effective.primary_label ?? '主来源'}离线、中控的检查关着：线路机的客户入口通不通暂时判不出</Note>}
          </div>
        )}
      </Panel>
      <Panel title={<>探测点<InfoTip text="放在靠近客户的地方（如香港）的小机器，跑 fleet-agent probe：每 30 秒连各线路机的客户入口，记通不通与建连延迟" /></>}
        tools={<Button size="sm" onClick={() => setAdding(true)}><Plus />新增</Button>}>
        {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
        {q.isLoading ? <SkeletonRows rows={2} /> : !list.length ? <Empty icon={<Radar size={22} />} text="还没有探测点" hint="新增后在探测点上执行安装命令；中控能 SSH 到它时也可以 fleetctl probe add <名称> --host <IP>" /> : (
          <TableWrap>
            <table className="table">
              <thead><tr><th>名称</th><th>状态</th><th>最近上报</th><th>版本</th><th>近 1 小时到各线路机（平均延迟 · 通的次数）</th><th>启用</th><th /></tr></thead>
              <tbody>
                {list.map((p) => (
                  <tr key={p.id}>
                    <td><span className="cell-main strong">{p.name}</span>{p.primary && <span className="sub">主来源</span>}{p.note && <span className="sub">{p.note}</span>}</td>
                    <td>
                      <Pill tone={STATE[p.state].tone}>{STATE[p.state].label}</Pill>
                      {p.install?.state === 'running' && <span className="sub">安装中：{p.install.step}</span>}
                      {p.install?.state === 'failed' && <span className="sub" data-tip={p.install.error ?? ''}>安装失败</span>}
                      {p.upgrade && <span className="sub">升级中</span>}
                      {p.bootstrap_expires_at && <span className="sub">安装命令 {F.dt(p.bootstrap_expires_at)} 前有效</span>}
                    </td>
                    <td>{p.last_seen_at ? <span data-tip={F.dt(p.last_seen_at)}>{F.ago(p.last_seen_at)}</span> : '—'}{p.last_ip && <span className="sub">{p.last_ip}</span>}</td>
                    <td className="num">{p.version ?? '—'}</td>
                    <td>{p.relays.length ? p.relays.map((r) => (
                      <div key={r.relay_id} className="t-sub">{r.name} · <b className="num">{r.avg_ms == null ? '—' : `${Math.round(r.avg_ms)} ms`}</b> · {r.ok}/{r.total}</div>
                    )) : '—'}</td>
                    <td><Switch checked={p.enabled} disabled={patch.isPending || (p.primary && p.enabled)} onCheckedChange={(v) => patch.mutate({ id: p.id, enabled: v }, { onError: toast.err })} aria-label="启用" /></td>
                    <td className="row" style={{ gap: 4, justifyContent: 'flex-end' }}>
                      <IconButton label="重新生成安装命令（执行后换新令牌）" onClick={() => reinstall.mutate(p.id, { onSuccess: (d) => d.bootstrap && setBoot({ name: p.name, b: d.bootstrap }), onError: toast.err })}><RefreshCw size={14} /></IconButton>
                      <IconButton label={p.primary ? '主来源不能删除' : '删除'} disabled={p.primary} onClick={() => setDel(p)}><Trash2 size={14} /></IconButton>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </TableWrap>
        )}
      </Panel>
      <AddDialog open={adding} onClose={() => setAdding(false)} onBootstrap={(name, b) => setBoot({ name, b })} />
      <ConfirmDialog open={!!boot} onOpenChange={(o) => !o && setBoot(null)} title={`在「${boot?.name ?? ''}」上用 root 执行`} confirmLabel="知道了" cancelLabel="关闭"
        description={<>{boot && F.dt(boot.b.expires_at)} 前有效，只能用一次；执行后半分钟内显示在线</>} onConfirm={() => undefined}>
        {boot && <div className="row" style={{ gap: 8, alignItems: 'flex-start' }}><code style={{ wordBreak: 'break-all', flex: 1 }}>{boot.b.cmd}</code><CopyButton text={boot.b.cmd} label="安装命令" /></div>}
      </ConfirmDialog>
      <ConfirmDialog open={!!del} onOpenChange={(o) => !o && setDel(null)} danger title={`删除探测点「${del?.name ?? ''}」？`} confirmLabel="删除"
        description="它的令牌立刻作废，它测的检查记录一起删掉" onConfirm={async () => { if (del) { const r = await remove.mutateAsync(del.id); setCleanup(r.cleanup); } }} />
      <ConfirmDialog open={!!cleanup} onOpenChange={(o) => !o && setCleanup(null)} title="已删除" confirmLabel="知道了" cancelLabel="关闭"
        description="探测点机器上的东西中控没有动，不再用它的话在它上面用 root 执行：" onConfirm={() => undefined}>
        {cleanup && <div className="row" style={{ gap: 8, alignItems: 'flex-start' }}><code style={{ wordBreak: 'break-all', flex: 1 }}>{cleanup}</code><CopyButton text={cleanup} label="清理命令" /></div>}
      </ConfirmDialog>
    </div>
  );
}

function AddDialog({ open, onClose, onBootstrap }: { open: boolean; onClose: () => void; onBootstrap: (name: string, b: Bootstrap) => void }) {
  const add = useAddProbePoint();
  const [name, setName] = React.useState(''); const [note, setNote] = React.useState('');
  React.useEffect(() => { if (open) { setName(''); setNote(''); } }, [open]);
  const submit = () => add.mutate({ name: name.trim(), note: note.trim() }, {
    onSuccess: (d) => { onClose(); if (d.bootstrap) onBootstrap(name.trim(), d.bootstrap); },
    onError: toast.err,
  });
  return (
    <FormDialog open={open} onOpenChange={(o) => !o && onClose()} title="新增探测点" onSubmit={submit} submitLabel="生成安装命令" busy={add.isPending} valid={!!name.trim()} dirty={!!name}
      desc="生成一条一次性安装命令（30 分钟内有效），在探测点上用 root 执行：下载 fleet-agent、装成服务（systemd 或 OpenRC）。中控能 SSH 到它时也可以用 fleetctl probe add <名称> --host <IP> 由中控装">
      <Field label="名称" info="卡片上写成「<名称>探测点」，如「香港」">
        <Input autoFocus maxLength={40} value={name} onChange={(e) => setName(e.target.value)} placeholder="香港" />
      </Field>
      <Field label="备注"><Input value={note} onChange={(e) => setNote(e.target.value)} placeholder="阿里云香港，Alpine" /></Field>
    </FormDialog>
  );
}
