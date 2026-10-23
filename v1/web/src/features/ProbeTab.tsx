// 设置「探测点」：放在靠近客户的地方（如香港）的小机器，替中控测线路机的客户入口通不通。
// 列表（状态、最近上报、版本、近 1 小时 / 24 小时 / 7 天 / 30 天到各线路机的平均延迟与成功率）、主探测来源（单选：中控或某个探测点）、中控自己的检查开关、新增（打印一次性安装命令）。
// 经 SSH 安装、升级用 fleetctl（要等、要看步骤，见 OPS.md「探测点」）。
import * as React from 'react';
import { Plus, Radar, RefreshCw, Trash2, Wrench } from 'lucide-react';
import { useAddProbePoint, useDelProbePoint, usePatchProbePoint, useProbePoints, useReachSource, useReinstallProbePoint, useSaveReachSource, type Bootstrap, type ProbePoint, type ProbeWindow, primaryIssue } from '@/lib/probePoints';
import * as F from '@/lib/format';
import { healthLabel, successPct } from '@/lib/derive';
import { useEndSubjectMaint } from '@/lib/queries';
import { useHeld } from '@/lib/useHeld';
import { HEALTH_TONE } from '@/lib/sort';
import { Button, IconButton } from '@/components/ui/button';
import { Field, InfoTip, Input, Switch } from '@/components/ui/form';
import { Segmented } from '@/components/ui/overlays';
import { SkeletonRows } from '@/components/ui/skeleton';
import { toast } from '@/components/ui/toast';
import { ConfirmDialog, FormDialog } from '@/components/app/dialogs';
import { MaintenanceDialog } from '@/components/app/MaintenanceDialog';
import { CopyButton, Empty, ErrorNote, Note, Panel, Pill, TableWrap } from '@/components/app/primitives';

const STATE: Record<ProbePoint['state'], { label: string; tone: 'ok' | 'bad' | 'neutral' | 'warn' }> = {
  online: { label: '在线', tone: 'ok' }, blind: { label: '探测无效', tone: 'warn' }, offline: { label: '离线', tone: 'bad' }, pending: { label: '等待安装', tone: 'warn' }, disabled: { label: '已停用', tone: 'neutral' },
};
const SOURCE_TIP = '线路机的心跳格、入口的「连不上」和相关告警，都以它的结果为准\n主来源离线超过 3 分钟，或在线但探测无效（它自己的网络不通）：中控探测开着就由中控代测，关着就显示灰色\n代测时连不上照常告警，连得上不算恢复';
const MAINT_TIP = '在维护窗口内：离线不告警，开着的告警保持到维护结束';
const WINDOWS: { value: ProbeWindow; label: string }[] = [{ value: '1h', label: '1 小时' }, { value: '24h', label: '24 小时' }, { value: '7d', label: '7 天' }, { value: '30d', label: '30 天' }];
const WINDOW_LABEL: Record<ProbeWindow, string> = { '1h': '近 1 小时', '24h': '近 24 小时', '7d': '近 7 天', '30d': '近 30 天' };

export function ProbeTab() {
  const [win, setWin] = React.useState<ProbeWindow>('1h');
  const src = useReachSource(); const q = useProbePoints(win);
  const save = useSaveReachSource();
  const [adding, setAdding] = React.useState(false);
  const [boot, setBoot] = React.useState<{ name: string; b: Bootstrap } | null>(null);
  const [del, setDel] = React.useState<ProbePoint | null>(null);
  const [cleanup, setCleanup] = React.useState<string | null>(null);
  const [maint, setMaint] = React.useState<ProbePoint | null>(null);
  const maintHeld = useHeld(maint?.id, maint);
  const patch = usePatchProbePoint(); const reinstall = useReinstallProbePoint(); const remove = useDelProbePoint(); const endMaint = useEndSubjectMaint();
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
              中控自己的探测<InfoTip text="中控自己连各线路机的入口。主来源是中控时不能关；主来源是探测点时建议开着，探测点离线时由它代测" />
            </label>
            {/* 主探测点为什么用不了按服务端给的原因说（离线、探测无效、已停用…），不一律写成离线 */}
            {s.effective.state === 'fallback' && <Note tone="warn">{primaryIssue(s.effective)}，暂由中控代测。连不上照常告警，连得上不算恢复</Note>}
            {s.effective.state === 'none' && <Note tone="bad">{s.effective.primary_label ? primaryIssue(s.effective) : '中控探测关着'}，{s.effective.primary_label ? '中控探测也关着，' : ''}暂时判断不了入口通不通</Note>}
          </div>
        )}
      </Panel>
      <Panel title={<>探测点<InfoTip text="放在离客户近的地方（比如香港）的小机器，定时连各线路机的入口，记录通不通和延迟" /></>}
        tools={<><Segmented sm label="时间范围" value={win} onChange={setWin} items={WINDOWS} /><Button size="sm" onClick={() => setAdding(true)}><Plus />新增</Button></>}>
        {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
        {q.isLoading ? <SkeletonRows rows={2} /> : !list.length ? <Empty icon={<Radar size={22} />} text="还没有探测点" hint="新增后，在探测点上执行安装命令。中控能 SSH 到它的话，也可以用 fleetctl probe add <名称> --host <IP> 直接装" /> : (
          <TableWrap data-stale={q.isPlaceholderData}>
            <table className="dt" style={{ minWidth: 860 }}>
              <colgroup><col style={{ width: 150 }} /><col style={{ width: 170 }} /><col style={{ width: 110 }} /><col style={{ width: 110 }} /><col /><col style={{ width: 64 }} /><col style={{ width: 116 }} /></colgroup>
              <thead><tr><th>名称</th><th>状态</th><th>最近上报</th><th>版本</th><th>{WINDOW_LABEL[win]}到各线路机（平均延迟 · 成功率）<InfoTip text="超过 1 小时的按小时汇总" /></th><th>启用</th><th /></tr></thead>
              <tbody>
                {list.map((p) => (
                  <tr key={p.id}>
                    <td><span className="cell-main strong">{p.name}</span>{p.primary && <span className="sub">主来源</span>}{p.note && <span className="sub">{p.note}</span>}</td>
                    <td>
                      <Pill tone={STATE[p.state].tone}>{STATE[p.state].label}</Pill>
                      {p.maintenance && <Pill tone={HEALTH_TONE.maintenance} tip={MAINT_TIP} tabIndex={0}>{healthLabel('maintenance')}</Pill>}
                      {p.state === 'blind' && <span className="sub" data-tip={p.last_kept_at ? `上次有效 ${F.dt(p.last_kept_at)}` : ''}>{p.discard_reason ?? '原因不明'}</span>}
                      {p.install?.state === 'running' && <span className="sub">安装中：{p.install.step}</span>}
                      {p.install?.state === 'failed' && <span className="sub" data-tip={p.install.error ?? ''}>安装失败</span>}
                      {p.upgrade && <span className="sub">升级中</span>}
                      {p.upgrade_failed && <span className="sub" data-tip={`新版本没能上报，探测点已换回旧版本（${F.dt(p.upgrade_failed.at)}）；修好后再升级`}>升级到 {p.upgrade_failed.to ?? '?'} 失败，已回到 {p.upgrade_failed.from ?? '?'}</span>}
                      {p.bootstrap_expires_at && <span className="sub">安装命令 {F.dt(p.bootstrap_expires_at)} 前有效</span>}
                    </td>
                    <td>{p.last_seen_at ? <span data-tip={F.dt(p.last_seen_at)}>{F.ago(p.last_seen_at)}</span> : '—'}{p.last_ip && <span className="sub">{p.last_ip}</span>}</td>
                    <td className="num">{p.version ?? '—'}</td>
                    <td>{p.relays.length ? p.relays.map((r) => (
                      <div key={r.relay_id} className="t-sub">{r.name} · <b className="num">{r.avg_ms == null ? '—' : `${Math.round(r.avg_ms)} ms`}</b> · <span data-tip={`${r.ok}/${r.total} 次连上`}>{successPct(r.total ? (100 * r.ok) / r.total : null)}</span></div>
                    )) : '—'}</td>
                    <td><Switch checked={p.enabled} disabled={patch.isPending || (p.primary && p.enabled)} onCheckedChange={(v) => patch.mutate({ id: p.id, enabled: v }, { onError: toast.err })} aria-label="启用" /></td>
                    <td className="r"><span className="row" style={{ gap: 4, justifyContent: 'flex-end', flexWrap: 'nowrap' }}>
                      {p.maintenance
                        ? <IconButton label="结束维护" disabled={endMaint.isPending} onClick={() => endMaint.mutate({ kind: 'probe_point', id: p.id }, { onSuccess: () => toast.ok(`${p.label} 已结束维护`), onError: toast.err })}><Wrench size={14} /></IconButton>
                        : <IconButton label="维护窗口" onClick={() => setMaint(p)}><Wrench size={14} /></IconButton>}
                      <IconButton label="重新生成安装命令" onClick={() => reinstall.mutate(p.id, { onSuccess: (d) => d.bootstrap && setBoot({ name: p.name, b: d.bootstrap }), onError: toast.err })}><RefreshCw size={14} /></IconButton>
                      <IconButton label={p.primary ? '主来源不能删除' : '删除'} disabled={p.primary} onClick={() => setDel(p)}><Trash2 size={14} /></IconButton>
                    </span></td>
                  </tr>
                ))}
              </tbody>
            </table>
          </TableWrap>
        )}
      </Panel>
      {maintHeld && <MaintenanceDialog m={{ id: maintHeld.id, name: maintHeld.label }} kind="probe_point" open={!!maint} onOpenChange={(o) => !o && setMaint(null)} />}
      <AddDialog open={adding} onClose={() => setAdding(false)} onBootstrap={(name, b) => setBoot({ name, b })} />
      <ConfirmDialog open={!!boot} onOpenChange={(o) => !o && setBoot(null)} title={`在「${boot?.name ?? ''}」上用 root 执行`} confirmLabel="知道了" cancelLabel="关闭"
        description={<>{boot && F.dt(boot.b.expires_at)} 前有效，只能用一次。执行后约半分钟上线</>} onConfirm={() => undefined}>
        {boot && <div className="row" style={{ gap: 8, alignItems: 'flex-start' }}><code style={{ wordBreak: 'break-all', flex: 1 }}>{boot.b.cmd}</code><CopyButton text={boot.b.cmd} label="安装命令" /></div>}
      </ConfirmDialog>
      <ConfirmDialog open={!!del} onOpenChange={(o) => !o && setDel(null)} danger title={`删除探测点「${del?.name ?? ''}」？`} confirmLabel="删除"
        description="这个探测点会立即失效，它的探测记录也会一起删除" onConfirm={async () => { if (del) { const r = await remove.mutateAsync(del.id); setCleanup(r.cleanup); } }} />
      <ConfirmDialog open={!!cleanup} onOpenChange={(o) => !o && setCleanup(null)} title="已删除" confirmLabel="知道了" cancelLabel="关闭"
        description="中控没有改动探测点这台机器。不再用它时，用 root 在上面执行：" onConfirm={() => undefined}>
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
      desc={"生成一条安装命令（30 分钟内有效，只能用一次），用 root 在探测点上执行。中控能 SSH 到它时，也可以用 fleetctl probe add <名称> --host <IP> 代为安装"}>
      <Field label="名称" info="名称填「香港」，卡片上显示为「香港探测点」">
        <Input autoFocus maxLength={40} value={name} onChange={(e) => setName(e.target.value)} placeholder="香港" />
      </Field>
      <Field label="备注"><Input value={note} onChange={(e) => setNote(e.target.value)} placeholder="阿里云香港，Alpine" /></Field>
    </FormDialog>
  );
}
