// 生命周期对话框：换线路机（节点）、搬空线路机（线路机）、恢复已退役的机器。
// 抽屉“管理”页、线路页、卡片/表格的右键菜单、命令面板共用；菜单触发的（连同退役）走 LifecycleDialogsHost（外壳里只渲染一份）。
import * as React from 'react';
import { useNavigate } from 'react-router-dom';
import { ArrowRight } from 'lucide-react';
import type { MachineView } from '@/lib/types';
import { useEvacuate, useMachine, useMoveEntry, useRelayChoices, useRestore, useRestorePreview, useTeams } from '@/lib/queries';
import { closeEvacuate, closeMove, closeRestore, closeRetire, useEvacuateTarget, useMoveTarget, useRestoreTarget, useRetireTarget } from '@/lib/nodeActions';
import { CheckRow, Field, Input } from '@/components/ui/form';
import { Select } from '@/components/ui/select';
import { toast } from '@/components/ui/toast';
import { resetOnOpen, useHeld } from '@/lib/useHeld';
import { entryState, Note } from './primitives';
import { ConfirmDialog } from './dialogs';
import { RetireDialog } from './RetireDialog';

const SHARE_WARN = '换线完成后分享链接会变：需要把新的分享链接发给客户端（作业完成的通知里有“复制新分享链接”）。';

/** 节点当前在用的入口（可以被换走的） */
export const movableEntries = (m: MachineView) => m.entries.filter((e) => e.lifecycle === 'active');

// ── 换线路机 ────────────────────────────────────────────────────────────

// 三个对话框都每次打开重新挂载（resetOnOpen）：表单状态直接从 props 初始化，关闭动画期间不变
export const MoveDialog = resetOnOpen(function MoveDialog({ m, open, onOpenChange, onDone }: { m: MachineView; open: boolean; onOpenChange: (o: boolean) => void; onDone?: () => void }) {
  const nav = useNavigate();
  const move = useMoveEntry();
  const relays = useRelayChoices().data ?? [];
  const cur = movableEntries(m);
  const [to, setTo] = React.useState('');
  const [from, setFrom] = React.useState(() => (cur.length === 1 ? cur[0].relay.id : ''));
  const used = new Set(m.entries.map((e) => e.relay.id));
  const opts = relays.filter((r) => !used.has(r.id)).map((r) => ({ value: r.id, label: r.name }));
  const multi = cur.length > 1;
  const ready = !!to && cur.length > 0 && (!multi || !!from);
  return (
    <ConfirmDialog asForm open={open} onOpenChange={onOpenChange} size="md" title={`把 ${m.name} 换到另一台线路机`} confirmLabel="开始换线" confirmDisabled={!ready}
      description="先在新线路机建好入口并验收出口，通过后才摘掉旧入口；换线期间节点始终有一个可用入口，不会断网。"
      onConfirm={async () => {
        const r = await move.mutateAsync({ id: m.id, to_relay_id: to, from_relay_id: multi ? from : undefined });
        toast.ok(`已提交换线路机：${r.from} → ${r.to}`);
        onDone?.();
        nav(`/jobs/${r.job_id}`);
      }}>
      {cur.length === 0 && <Note tone="warn">这台机器没有运行中的入口，没法换线路机（入口需要先生效）。</Note>}
      {cur.length > 0 && (
        <div className="stack-sm">
          <div className="hint">当前入口：{cur.map((e) => `${e.relay.name}:${e.port}`).join('、')}</div>
          {multi && <Field label="换掉哪条线路机上的入口"><Select value={from} onChange={setFrom} options={cur.map((e) => ({ value: e.relay.id, label: `${e.relay.name}:${e.port}` }))} placeholder="选择要换掉的入口" /></Field>}
          <Field label="目标线路机" hint={opts.length === 0 ? '没有可选的线路机：目标必须是在用的，且这台节点在上面还没有入口' : undefined}>
            <Select value={to} onChange={setTo} options={opts} placeholder="选择线路机" />
          </Field>
        </div>
      )}
      <Note tone="warn">{SHARE_WARN}</Note>
    </ConfirmDialog>
  );
});

// ── 搬空线路机 ──────────────────────────────────────────────────────────

export const EvacuateDialog = resetOnOpen(function EvacuateDialog({ relay, open, onOpenChange, onDone }: { relay: MachineView; open: boolean; onOpenChange: (o: boolean) => void; onDone?: () => void }) {
  const nav = useNavigate();
  const evac = useEvacuate();
  const relays = useRelayChoices().data ?? [];
  const [to, setTo] = React.useState('');
  const opts = relays.filter((r) => r.id !== relay.id).map((r) => ({ value: r.id, label: r.name }));
  const list = relay.entries.filter((e) => e.lifecycle !== 'removing' && e.lifecycle !== 'removed');
  const movable = list.filter((e) => e.lifecycle === 'active');
  return (
    <ConfirmDialog open={open} onOpenChange={onOpenChange} size="md" title={`搬空 ${relay.name}`} confirmLabel={`搬走 ${movable.length} 个节点`} confirmDisabled={!to || movable.length === 0}
      description="为这台线路机上每个在用入口的节点各建一个“换线路机”作业，搬完后就可以安全退役这台线路机。"
      onConfirm={async () => {
        const r = await evac.mutateAsync({ id: relay.id, to_relay_id: to });
        toast.ok(`已为 ${r.jobs.length} 个节点提交换线路机作业${r.skipped.length ? `，${r.skipped.length} 个被跳过` : ''}`);
        for (const s of r.skipped) toast.info(`${s.name ?? '节点'} 被跳过：${s.reason}`);
        onDone?.();
        nav('/jobs');
      }}>
      <Field label="搬到哪台线路机"><Select value={to} onChange={setTo} options={opts} placeholder="选择目标线路机" /></Field>
      <div>
        <div className="label" style={{ marginBottom: 6 }}>将被搬走的节点（{movable.length}）</div>
        {list.length === 0 ? <div className="hint">这台线路机上没有挂载入口。</div> : (
          <ul className="reason-list" style={{ maxHeight: 220, overflow: 'auto' }}>
            {list.map((e) => (
              <li key={e.id}>
                <span className="num">{e.node.name}</span> <span className="t-sub">端口 {e.port}</span> {entryState(e)}
                {e.lifecycle !== 'active' && <span className="t-sub"> · 会被跳过（入口需处于运行中）</span>}
              </li>
            ))}
          </ul>
        )}
      </div>
      <Note tone="warn">{SHARE_WARN}</Note>
    </ConfirmDialog>
  );
});

// ── 恢复已退役的机器 ────────────────────────────────────────────────────

/** 值停下来 ms 毫秒后才跟上：编号、名字输入框每按一次键不必各发一次预检 */
function useSettled<T>(v: T, ms: number): T {
  const [s, setS] = React.useState(v);
  React.useEffect(() => { const t = setTimeout(() => setS(v), ms); return () => clearTimeout(t); }, [v, ms]);
  return s;
}

const KEY_LOGIN: Record<string, string> = { ok: '中控密钥仍可登录，不需要密码', denied: '中控密钥被拒绝，需要 root 密码重新引导', unreachable: 'SSH 现在连不上', error: '探测失败' };

export const RestoreDialog = resetOnOpen(function RestoreDialog({ m, open, onOpenChange, onDone }: { m: MachineView; open: boolean; onOpenChange: (o: boolean) => void; onDone?: () => void }) {
  const nav = useNavigate();
  const restore = useRestore();
  const teams = useTeams().data ?? [];
  const relays = useRelayChoices().data ?? [];
  const isRelay = m.kind === 'relay';
  const isProxy = m.kind === 'proxy';
  const [team, setTeam] = React.useState(m.team?.id ?? '');
  const [num, setNum] = React.useState('');
  const [label, setLabel] = React.useState('');
  const [pickedRelay, setRelay] = React.useState<string | null>(null); // null = 还没选：用预检给的上次接的线路机
  const [pw, setPw] = React.useState('');
  const [up, setUp] = React.useState({ protocol: 'socks5', host: '', port: '', username: '', password: '' });
  const [reason, setReason] = React.useState('');
  const [reset, setReset] = React.useState(false);

  // 第一份预检带 SSH 探测（慢一点），决定要不要密码；改团队/编号/名字后只重算名字（probe=false）。
  // 编号、名字停手 300ms 后才进预检（查询键）；提交用的是当下的输入（over），没跟上之前不让提交
  const toOver = (numText: string, labelText: string) => {
    const n = Number(numText);
    return { team_id: team && team !== m.team?.id ? team : undefined, number: numText && Number.isInteger(n) && n > 0 ? n : undefined, label: labelText.trim() || undefined };
  };
  const over = toOver(num, label);
  const settledNum = useSettled(num, 300), settledLabel = useSettled(label, 300);
  const settled = settledNum === num && settledLabel === label;
  const asked = toOver(settledNum, settledLabel);
  const edited = !!(over.team_id || over.number != null || over.label);
  const base = useRestorePreview(m.id, open, {});
  // 重开时缓存里还是上一次的预检（关闭动画期间对话框还挂着、观察者没走，gcTime 0 也回收不了）：
  // 只认这次打开后成功取回的，之前不显示、也不让提交（要不要密码、要不要选线路机都以它为准）
  const b = base.isFetchedAfterMount && base.isSuccess ? base.data : undefined;
  const live = useRestorePreview(m.id, open && !!(asked.team_id || asked.number != null || asked.label), { ...asked, probe: false });
  const pv = edited && live.data ? live.data : b;
  const needs = b?.needs;
  const relay = pickedRelay ?? b?.last_relay?.id ?? '';

  const relayOpts = relays.map((r) => ({ value: r.id, label: r.name }));
  const upOk = !isProxy || (up.host.trim() !== '' && Number(up.port) > 0);
  const pwOk = needs?.password !== 'required' || pw !== '';
  const ready = !!pv && pv.restorable && !!needs && (!needs.relay || !!relay) && upOk && pwOk && settled && !live.isFetching;
  // 填过东西就别让误点遮罩关掉（Esc、取消仍可关）
  const dirty = edited || !!pw || !!reason.trim() || reset || relay !== (b?.last_relay?.id ?? '') || !!(up.host || up.port || up.username || up.password);

  const submit = async () => {
    const body: Parameters<typeof restore.mutateAsync>[0] = { id: m.id };
    if (over.team_id) body.team_id = over.team_id;
    if (over.number != null) body.number = over.number;
    if (over.label) body.label = over.label;
    if (needs?.relay) body.relay_id = relay;
    if (isProxy) body.upstream = { protocol: up.protocol as 'socks5' | 'http', host: up.host.trim(), port: Number(up.port), username: up.username || undefined, password: up.password || undefined };
    if (pw) body.login = { password: pw };
    if (reason.trim()) body.reason = reason.trim();
    if (reset) body.reset_host_key = true;
    const r = await restore.mutateAsync(body);
    toast.ok(`已提交恢复：${r.old_name} → ${r.name}`);
    onDone?.();
    nav(`/jobs/${r.job_id}`);
  };

  return (
    <ConfirmDialog asForm dirty={dirty} open={open} onOpenChange={onOpenChange} size="md" title={`恢复 ${m.name}`} confirmLabel="恢复" confirmDisabled={!ready}
      description="机器保持原来的 ID（历史、账单、审计都接上），重新走一遍上架的收敛步骤；成功后回到“在用”。失败会停在“上架失败”，可在作业页重试；取消会回到已退役。"
      onConfirm={submit}>
      {!base.isFetchedAfterMount && <div className="hint">正在预检（会试一下中控密钥能不能登录这台机器，最多十几秒）…</div>}
      {base.isFetchedAfterMount && base.isError && <Note tone="bad">预检失败：{(base.error as Error).message}</Note>}
      {pv && (
        <div className="stack-sm">
          <div className="row row-between">
            <span className="row" style={{ gap: 8 }}>
              <span className="t-sub">恢复后的名字</span>
              <span className="preview-name">{pv.old_name}<ArrowRight size={14} style={{ color: 'var(--fg-3)' }} />{pv.name}</span>
            </span>
            <span className="t-sub">IP {pv.ip}</span>
          </div>
          {pv.ip_conflict && <Note tone="bad">IP {pv.ip} 已被在用的「{pv.ip_conflict.name}」占用，现在不能恢复。先把它退役，或确认这台机器的 IP 没有变。</Note>}
          {pv.blockers.filter((b) => !pv.ip_conflict || !b.includes(pv.ip)).map((b) => <Note key={b} tone="bad">{b}</Note>)}
          {pv.warnings.map((w) => <Note key={w} tone="warn">{w}</Note>)}
          {pv.retired_reason && <div className="hint">退役原因：{pv.retired_reason}</div>}
        </div>
      )}

      {pv && (
        <div className="form-grid">
          {!isRelay && <Field label="团队"><Select value={team} onChange={setTeam} options={teams.map((t) => ({ value: t.id, label: t.name }))} /></Field>}
          {!isRelay && <Field label="编号" hint="留空 = 自动接在团队末尾"><Input inputMode="numeric" value={num} onChange={(e) => setNum(e.target.value.replace(/\D/g, ''))} placeholder={pv.number != null ? `自动：${pv.number}` : '自动'} /></Field>}
          {isRelay && <Field label="线路机名称" hint="原名被占用时换一个" className="span-2"><Input value={label} onChange={(e) => setLabel(e.target.value)} placeholder={pv.label ?? ''} /></Field>}
          {needs?.relay && (
            <Field label="接入的线路机" hint={b?.last_relay ? `上次接的是 ${b.last_relay.name}；旧入口不会复活，会建新入口` : '会在这台线路机上建新入口'} className="span-2">
              <Select value={relay} onChange={setRelay} options={relayOpts} placeholder="选择线路机" />
            </Field>
          )}
          {isProxy && (
            <>
              <Field label="上游协议"><Select value={up.protocol} onChange={(v) => setUp({ ...up, protocol: v })} options={[{ value: 'socks5', label: 'SOCKS5' }, { value: 'http', label: 'HTTP' }]} /></Field>
              <Field label="上游端口"><Input inputMode="numeric" value={up.port} onChange={(e) => setUp({ ...up, port: e.target.value.replace(/\D/g, '') })} /></Field>
              <Field label="上游地址" className="span-2" hint="退役时出站参数已清掉，需要重新填写"><Input value={up.host} onChange={(e) => setUp({ ...up, host: e.target.value })} placeholder="例如 proxy.example.com" /></Field>
              <Field label="用户名（选填）"><Input value={up.username} onChange={(e) => setUp({ ...up, username: e.target.value })} autoComplete="off" /></Field>
              <Field label="密码（选填）"><Input type="password" value={up.password} onChange={(e) => setUp({ ...up, password: e.target.value })} autoComplete="new-password" /></Field>
            </>
          )}
          {!isProxy && needs && (
            needs.password === 'no' ? (
              <div className="hint span-2">{KEY_LOGIN[b?.ssh?.key_login ?? 'ok']}。</div>
            ) : (
              <Field className="span-2" label={needs.password === 'required' ? 'root 密码（必填）' : 'root 密码（选填）'}
                hint={needs.password === 'required' ? `${KEY_LOGIN[b?.ssh?.key_login ?? 'denied']}。密码只用来推一次中控公钥，用完即丢。` : `${KEY_LOGIN[b?.ssh?.key_login ?? 'error'] ?? ''}。中控密钥登录不了时才会用到这个密码。`}>
                <Input type="password" value={pw} onChange={(e) => setPw(e.target.value)} autoComplete="new-password" />
              </Field>
            )
          )}
          <Field label="原因（选填）" className="span-2"><Input value={reason} onChange={(e) => setReason(e.target.value)} placeholder="例如：客户续约" /></Field>
          {!isProxy && <div className="span-2"><CheckRow checked={reset} onChange={setReset}>机器重装过：允许覆盖已记录的主机指纹</CheckRow></div>}
        </div>
      )}
    </ConfirmDialog>
  );
});

// ── 外壳里唯一一份：菜单/命令面板触发的对话框 ──────────────────────────

export function LifecycleDialogsHost() {
  const moveId = useMoveTarget();
  const evacId = useEvacuateTarget();
  const restoreId = useRestoreTarget();
  const retireId = useRetireTarget();
  // id 清空后继续用最近的机器渲染 open=false 的对话框，让 Radix 播完关闭动画再卸载
  const mv = useHeld(moveId, useMachine(moveId).data);
  const ev = useHeld(evacId, useMachine(evacId).data);
  const rs = useHeld(restoreId, useMachine(restoreId).data);
  const rt = useHeld(retireId, useMachine(retireId).data);
  return (
    <>
      {mv && <MoveDialog key={mv.id} m={mv} open={!!moveId} onOpenChange={(o) => { if (!o) closeMove(); }} onDone={closeMove} />}
      {ev && <EvacuateDialog key={ev.id} relay={ev} open={!!evacId} onOpenChange={(o) => { if (!o) closeEvacuate(); }} onDone={closeEvacuate} />}
      {rs && <RestoreDialog key={rs.id} m={rs} open={!!restoreId} onOpenChange={(o) => { if (!o) closeRestore(); }} onDone={closeRestore} />}
      {rt && <RetireDialog key={rt.id} m={rt} open={!!retireId} onOpenChange={(o) => { if (!o) closeRetire(); }} onDone={closeRetire} />}
    </>
  );
}
