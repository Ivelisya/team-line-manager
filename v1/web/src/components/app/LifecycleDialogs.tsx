// 生命周期对话框：换线路机（节点）、搬空线路机（线路机）。
// 抽屉“管理”页、线路页、卡片/表格的右键菜单、命令面板共用；菜单触发的（连同退役）走 LifecycleDialogsHost（外壳里只渲染一份）。
import * as React from 'react';
import { useNavigate } from 'react-router-dom';
import type { MachineView } from '@/lib/types';
import { useEvacuate, useEvacuatePreview, useMachine, useMoveEntry, useMovePreview, useRelays } from '@/lib/queries';
import { HEALTH_TONE } from '@/lib/sort';
import { closeEvacuate, closeMove, closeRenew, closeReplace, closeRetire, useEvacuateTarget, useMoveTarget, useRenewTarget, useReplaceTarget, useRetireTarget } from '@/lib/nodeActions';
import { PlanBlockers, ReplaceDialog, RouterExitList } from './ReplaceDialog';
import { RenewDialog } from '@/features/BillingTab';
import { useDrawer } from '@/lib/drawer';
import { errText, healthLabel } from '@/lib/derive';
import { CheckRow, Field, InfoTip } from '@/components/ui/form';
import { Select, type Option } from '@/components/ui/select';
import { toast } from '@/components/ui/toast';
import { resetOnOpen, useHeld } from '@/lib/useHeld';
import { Due, entryState, ErrorNote, Note } from './primitives';
import { ConfirmDialog } from './dialogs';
import { RetireDialog } from './RetireDialog';

const SHARE_WARN = <Note tone="warn">换线后分享链接会变 <InfoTip text="要把新的分享链接发给客户端。作业完成时的通知里，可以直接复制新链接" /></Note>;

/** 「同时改软路由」：新入口验收后、摘旧入口前改，失败全部换回、旧入口不摘 */
function RoutersCheck({ checked, onChange, none, failed }: { checked: boolean; onChange: (v: boolean) => void; none?: boolean; failed?: boolean }) {
  if (failed) return <div className="hint">预览失败，不确定有没有软路由在用这个入口（原因见下）</div>;
  if (none) return <div className="hint">没有软路由出口指向这个入口</div>;
  return <CheckRow checked={checked} onChange={onChange}>同时改软路由 <InfoTip text={'把软路由上指向旧入口的出口，改成新入口。每台先备份，改完重启 OpenClash 并测延迟，失败会全部换回，旧入口也不移除\n不改的话，旧入口移除后这些出口就断了'} /></CheckRow>;
}

/** 换线 / 搬空的目标线路机下拉：在用的线路机，每项写状态、入口数、到期；降级、故障、即将到期、已过期按级别着色。
 *  正常的（含降级）排前面，故障的排后面置灰、悬停写原因，仍可选 */
function targetOptions(relays: MachineView[], keep: (r: MachineView) => boolean): Option[] {
  const down = (r: MachineView) => r.health.status === 'down';
  const list = relays.filter((r) => r.lifecycle === 'active' && keep(r));
  return [...list.filter((r) => !down(r)), ...list.filter(down)].map((r) => {
    const tone = HEALTH_TONE[r.health.status];
    const s = r.subscription;
    return {
      value: r.id,
      label: <span className={down(r) ? 'opt-relay is-down' : 'opt-relay'} data-tip={down(r) ? r.health.reasons.map((x) => errText(x.text)).join('\n') || undefined : undefined}>{r.name}<span className="t-sub">
        {' · '}<span className="due" data-tone={tone === 'warn' || tone === 'bad' ? tone : undefined}>{healthLabel(r.health.status)}</span>
        {` · ${r.entries.length} 条入口`}{s && <>{' · '}<Due days={s.due_days} state={s.due_state} tail /></>}
      </span></span>,
    };
  });
}

/** 节点当前在用的入口（可以被换走的）。线路机的直出入口不搬：退役线路机时随之摘除 */
export const movableEntries = (m: MachineView) => m.entries.filter((e) => e.lifecycle === 'active' && !e.direct);

// ── 换线路机 ────────────────────────────────────────────────────────────

// 三个对话框都每次打开重新挂载（resetOnOpen）：表单状态直接从 props 初始化，关闭动画期间不变
export const MoveDialog = resetOnOpen(function MoveDialog({ m, open, onOpenChange, onDone, initialTo, initialFrom }: { m: MachineView; open: boolean; onOpenChange: (o: boolean) => void; onDone?: () => void; initialTo?: string; initialFrom?: string }) {
  const nav = useNavigate();
  const move = useMoveEntry();
  const relays = useRelays().data ?? [];
  const cur = movableEntries(m);
  const [to, setTo] = React.useState(initialTo ?? '');
  const [from, setFrom] = React.useState(() => (cur.length === 1 ? cur[0].relay.id : initialFrom ?? ''));
  const used = new Set(m.entries.map((e) => e.relay.id));
  const opts = targetOptions(relays, (r) => !used.has(r.id));
  const multi = cur.length > 1;
  const [routers, setRouters] = React.useState(true);
  const q = useMovePreview(m.id, open && cur.length > 0 && (!multi || !!from), { to_relay_id: to, from_relay_id: multi ? from : undefined, routers });
  const pv = q.isFetchedAfterMount ? q.data : undefined;
  const ready = !!to && cur.length > 0 && (!multi || !!from) && !!pv?.ok && !q.isFetching;
  return (
    <ConfirmDialog asForm open={open} onOpenChange={onOpenChange} size="md" title={`换线路机 · ${m.name}`} confirmLabel="开始换线" confirmDisabled={!ready}
      description={<>换线期间不断网 <InfoTip text={"先在新线路机建好入口并验证出口，再改软路由，最后才移除旧入口\n过程中节点始终有一个入口可用"} /></>}
      onConfirm={async () => {
        const r = await move.mutateAsync({ id: m.id, to_relay_id: to, from_relay_id: multi ? from : undefined, routers });
        toast.ok(`已提交换线路机：${r.from} → ${r.to}`);
        onDone?.();
        nav(`/jobs/${r.job_id}`);
      }}>
      {cur.length === 0 && <Note tone="warn">没有运行中的入口，不能换线</Note>}
      {cur.length > 0 && (
        <div className="stack-sm">
          <div className="hint">当前入口：{cur.map((e) => `${e.relay.name}:${e.port}`).join('、')}</div>
          {multi && <Field label="换掉哪条线路机上的入口"><Select value={from} onChange={setFrom} options={cur.map((e) => ({ value: e.relay.id, label: `${e.relay.name}:${e.port}` }))} placeholder="选择要换掉的入口" /></Field>}
          <Field label="目标线路机" info="必须是在用的线路机，且这个节点在上面还没有入口" hint={opts.length === 0 ? '没有可选的线路机' : undefined}>
            <Select value={to} onChange={setTo} options={opts} placeholder="选择线路机" />
          </Field>
          <RoutersCheck checked={routers} onChange={setRouters} none={!!pv && pv.ok && pv.routers.length === 0 && pv.warnings.length === 0} failed={!!pv && !pv.ok && pv.routers.length === 0} />
        </div>
      )}
      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()}>预览失败：</ErrorNote>}
      {pv && to && pv.routers.length + pv.blockers.length + pv.warnings.length > 0 && (
        <div className="stack-sm" style={{ opacity: q.isFetching ? 0.6 : 1 }}>
          {pv.routers_enabled
            ? <RouterExitList routers={pv.routers.map((plan) => ({ node: m.name, plan }))} />
            : pv.routers.length > 0 && <Note tone="bad">不改软路由的话，{pv.routers.flatMap((r) => r.exits.map((e) => `「${r.name}」的出口「${e.exit}」`)).join('、')}会在旧入口移除后断开</Note>}
          <PlanBlockers routers={pv.routers_enabled ? pv.routers : []} blockers={pv.blockers} />
          {pv.warnings.map((w) => <Note key={w} tone="warn">{w}</Note>)}
        </div>
      )}
      {SHARE_WARN}
    </ConfirmDialog>
  );
});

// ── 搬空线路机 ──────────────────────────────────────────────────────────

export const EvacuateDialog = resetOnOpen(function EvacuateDialog({ relay, open, onOpenChange, onDone }: { relay: MachineView; open: boolean; onOpenChange: (o: boolean) => void; onDone?: () => void }) {
  const nav = useNavigate();
  const evac = useEvacuate();
  const relays = useRelays().data ?? [];
  const [to, setTo] = React.useState('');
  const [routers, setRouters] = React.useState(true);
  const q = useEvacuatePreview(relay.id, open, { to_relay_id: to, routers });
  const pv = q.isFetchedAfterMount && to ? q.data : undefined;
  const opts = targetOptions(relays, (r) => r.id !== relay.id);
  const list = relay.entries.filter((e) => e.lifecycle !== 'removing' && e.lifecycle !== 'removed' && !e.direct);
  const movable = list.filter((e) => e.lifecycle === 'active');
  const hasDirect = relay.entries.some((e) => e.direct);
  // 实际会建几个作业以预览为准（软路由不在线、有别的作业的节点会被跳过）；还没选目标时按在用入口数
  const n = pv?.jobs.length ?? movable.length;
  return (
    <ConfirmDialog open={open} onOpenChange={onOpenChange} size="md" title={`搬空 · ${relay.name}`} confirmLabel={`搬走 ${n} 个节点`} confirmDisabled={!to || n === 0 || !pv || q.isFetching}
      description={<>每个节点各建一个换线作业 <InfoTip text="为这台线路机上每个在用入口的节点，各建一个「换线路机」作业。搬完后就可以放心退役这台线路机" /></>}
      onConfirm={async () => {
        const r = await evac.mutateAsync({ id: relay.id, to_relay_id: to, routers });
        toast.ok(`已为 ${r.jobs.length} 个节点提交换线路机作业${r.skipped.length ? `，${r.skipped.length} 个被跳过` : ''}`);
        for (const s of r.skipped) toast.info(`${s.name ?? '节点'} 被跳过：${s.reason}`);
        onDone?.();
        nav('/jobs');
      }}>
      <Field label="搬到哪台线路机"><Select value={to} onChange={setTo} options={opts} placeholder="选择目标线路机" /></Field>
      <RoutersCheck checked={routers} onChange={setRouters} />
      <div>
        <div className="label" style={{ marginBottom: 6 }}>将被搬走的节点（{n}）</div>
        {list.length === 0 ? <div className="hint">没有节点入口</div> : (
          <ul className="reason-list" style={{ maxHeight: 220, overflow: 'auto' }}>
            {list.map((e) => (
              <li key={e.id}>
                <span className="num">{e.node.name}</span> <span className="t-sub">端口 {e.port}</span> {entryState(e, true)}
                {e.lifecycle !== 'active' && <span className="t-sub" data-tip="入口不在运行中"> · 会跳过</span>}
              </li>
            ))}
          </ul>
        )}
      </div>
      {hasDirect && <div className="hint">直出入口不搬 <InfoTip text="退役时会一起移除。用它的客户，需要换成别的线路机的直出链接" /></div>}
      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()}>预览失败：</ErrorNote>}
      {pv && (
        <div className="stack-sm" style={{ opacity: q.isFetching ? 0.6 : 1 }}>
          {routers
            ? <RouterExitList routers={pv.jobs.flatMap((j) => j.routers.map((plan) => ({ node: j.name, plan })))} title={<>要改的软路由出口 <InfoTip text="各节点同时建新入口，都验收完后一起改软路由。每台软路由只改一次" /></>} />
            : pv.jobs.some((j) => j.routers.length > 0) && <Note tone="bad">不改软路由的话，指向这些入口的出口会在旧入口移除后断开</Note>}
          {routers && (pv.router_restarts ?? []).map((r) => <Note key={r.id} tone="warn">软路由「{r.name}」上 {r.nodes} 个节点的出口一起改，OpenClash 重启 1 次，会断开路由上的所有连接</Note>)}
          {pv.skipped.map((k) => <Note key={k.machine_id} tone="warn">{k.name ?? '节点'} 会跳过：{k.reason}</Note>)}
          {pv.warnings.map((w) => <Note key={w} tone="warn">{w}</Note>)}
        </div>
      )}
      {SHARE_WARN}
    </ConfirmDialog>
  );
});

// ── 外壳里唯一一份：菜单/命令面板触发的对话框 ──────────────────────────

export function LifecycleDialogsHost() {
  const moveId = useMoveTarget();
  const evacId = useEvacuateTarget();
  const retireId = useRetireTarget();
  const renewId = useRenewTarget();
  const replaceId = useReplaceTarget();
  const rp = useHeld(replaceId, useMachine(replaceId).data);
  // id 清空后继续用最近的机器渲染 open=false 的对话框，让 Radix 播完关闭动画再卸载
  const mv = useHeld(moveId, useMachine(moveId).data);
  const ev = useHeld(evacId, useMachine(evacId).data);
  const rt = useHeld(retireId, useMachine(retireId).data);
  const rn = useHeld(renewId, useMachine(renewId).data);
  // 还没有订阅的（命令面板里选的）登记不了续费：改为打开它抽屉的「费用」页去填
  const { open: openDrawer } = useDrawer();
  React.useEffect(() => { if (renewId && rn?.id === renewId && !rn.subscription) { closeRenew(); openDrawer(renewId, 'billing'); } }, [renewId, rn, openDrawer]);
  return (
    <>
      {mv && <MoveDialog key={mv.id} m={mv} open={!!moveId} onOpenChange={(o) => { if (!o) closeMove(); }} onDone={closeMove} />}
      {rp && <ReplaceDialog key={rp.id} m={rp} open={!!replaceId} onOpenChange={(o) => { if (!o) closeReplace(); }} onDone={closeReplace} />}
      {ev && <EvacuateDialog key={ev.id} relay={ev} open={!!evacId} onOpenChange={(o) => { if (!o) closeEvacuate(); }} onDone={closeEvacuate} />}
      {rt && <RetireDialog key={rt.id} m={rt} open={!!retireId} onOpenChange={(o) => { if (!o) closeRetire(); }} onDone={closeRetire} />}
      {rn?.subscription && <RenewDialog key={rn.id} m={rn} s={rn.subscription} open={!!renewId} onOpenChange={(o) => { if (!o) closeRenew(); }} />}
    </>
  );
}
