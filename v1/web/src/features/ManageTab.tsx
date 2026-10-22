// 抽屉“管理”页：改名/改编号（带实时预览）、入口操作、同机 IP 节点、agent、退役、彻底删除（仅已退役）。
import * as React from 'react';
import { Link } from 'react-router-dom';
import { ArrowRight, PackageOpen, Plus, Repeat2, Replace, Trash2, Undo2, Wrench } from 'lucide-react';
import type { MachineView } from '@/lib/types';
import { maintOwner, useAddEntry, useEndMaintenance, useMachine, usePatchMachine, usePurge, useRelayChoices, useSilences, useTeams, useTune } from '@/lib/queries';
import { openMaint, openUpgrade } from '@/lib/nodeActions';
import { useDrawer } from '@/lib/drawer';
import { errText, lifecycleLabel } from '@/lib/derive';
import * as F from '@/lib/format';
import { Button } from '@/components/ui/button';
import { Field, InfoTip, Input } from '@/components/ui/form';
import { Select } from '@/components/ui/select';
import { toast } from '@/components/ui/toast';
import { entryState, Note, Section, SectionHeader } from '@/components/app/primitives';
import { ConfirmDialog, TypedConfirmDialog } from '@/components/app/dialogs';
import { RetireDialog } from '@/components/app/RetireDialog';
import { EntryMenuButton, EntryMenus, entryLabel } from '@/components/app/EntryActions';
import { EvacuateDialog, MoveDialog, RestoreDialog, movableEntries } from '@/components/app/LifecycleDialogs';
import { ReplaceDialog, canReplace } from '@/components/app/ReplaceDialog';
import { CountrySelect } from '@/components/app/CountrySelect';
import { countryName } from '@/lib/flags';

export function ManageTab({ m, onClosed }: { m: MachineView; onClosed: () => void }) {
  const retired = m.lifecycle === 'retired';
  return (
    <>
      {!retired && <RenameSection m={m} />}
      {!retired && <CountrySection m={m} />}
      {/* 在用的机器没有入口时也要能加：节点「添加入口」，线路机「添加直出入口」 */}
      {!retired && (m.lifecycle === 'active' || m.entries.length > 0) && <EntrySection m={m} />}
      {/* 在用的普通 VPS 节点都能当宿主机：上面另配的公网 IP 各上架成一台同机 IP 节点 */}
      {canReplace(m) && <ReplaceSection m={m} />}
      {m.kind === 'vps' && !m.host_machine && m.lifecycle === 'active' && <IpNodesSection m={m} />}
      {m.agent && m.lifecycle === 'active' && <AgentSection m={m} />}
      {m.tune && m.lifecycle === 'active' && <TuneSection m={m} />}
      {m.lifecycle === 'active' && <MaintSection m={m} />}
      <DangerSection m={m} onClosed={onClosed} />
    </>
  );
}

/** 改名 / 改编号：团队 + 编号（节点）或名称（线路机），实时预览新显示名 */
function RenameSection({ m }: { m: MachineView }) {
  const teams = useTeams().data;
  // 选项只随团队列表变：抽屉随实时推送重渲染时团队下拉（memo）不跟着重画
  const teamOpts = React.useMemo(() => (teams ?? []).map((t) => ({ value: t.id, label: t.name })), [teams]);
  const patch = usePatchMachine();
  const isRelay = m.kind === 'relay';
  const [team, setTeam] = React.useState(m.team?.id ?? '');
  const [num, setNum] = React.useState(m.number != null ? String(m.number) : '');
  const [label, setLabel] = React.useState(m.label ?? '');
  // 实时推送会改 revision 和编号（如同团队有机器退役后自动压缩编号）：只同步用户没动过的字段（输入仍等于上次的服务端值），
  // 正在编辑的不覆盖；换了机器则全部重置
  const srv = { id: m.id, team: m.team?.id ?? '', num: m.number != null ? String(m.number) : '', label: m.label ?? '' };
  const last = React.useRef(srv);
  React.useEffect(() => {
    const prev = last.current; last.current = srv;
    const keep = (v: string, was: string, now: string) => (prev.id === srv.id && v !== was ? v : now);
    setTeam((v) => keep(v, prev.team, srv.team)); setNum((v) => keep(v, prev.num, srv.num)); setLabel((v) => keep(v, prev.label, srv.label));
  }, [srv.id, srv.team, srv.num, srv.label]); // eslint-disable-line react-hooks/exhaustive-deps

  const teamName = teams?.find((t) => t.id === team)?.name;
  const n = Number(num);
  const numOk = Number.isInteger(n) && n >= 1 && n <= 999; // 与服务端一致：编号范围 1–999
  const preview = isRelay ? label.trim() : teamName && numOk ? F.displayName(teamName, n) : '';
  const changed = isRelay ? label.trim() !== (m.label ?? '') : team !== (m.team?.id ?? '') || (numOk && n !== m.number);
  const valid = isRelay ? label.trim().length > 0 : !!team && numOk;

  const save = () => {
    const body = isRelay ? { label: label.trim() } : { team_id: team, number: n };
    patch.mutate({ id: m.id, revision: m.revision, ...body }, { onSuccess: () => toast.ok(`已改名为 ${preview}`), onError: toast.err });
  };
  return (
    <Section>
      <SectionHeader title={<>{isRelay ? '改名' : '改名 / 改编号'} <InfoTip text="不会断开现有连接：只改显示名与线路机上的备注，不换令牌。" /></>} />
      <form className="form-grid" onSubmit={(e) => { e.preventDefault(); if (changed && valid && !patch.isPending) save(); }}>
        {isRelay ? (
          <Field label="线路机名称" className="span-2"><Input value={label} onChange={(e) => setLabel(e.target.value)} /></Field>
        ) : (
          <>
            <Field label="团队"><Select value={team} onChange={setTeam} options={teamOpts} /></Field>
            <Field label="编号" error={num && !numOk ? '编号应为 1–999 的整数' : undefined}>
              <Input inputMode="numeric" value={num} onChange={(e) => setNum(e.target.value.replace(/\D/g, ''))} aria-invalid={!!num && !numOk} />
            </Field>
          </>
        )}
        <div className="span-2 row row-between">
          {/* 没改动时只显示当前名，改了才出现「旧名 → 新名」 */}
          <span className="row" style={{ gap: 8 }}>
            <span className="t-sub">{changed ? '新显示名' : '当前显示名'}</span>
            <span className="preview-name">{changed ? <>{m.name}<ArrowRight size={14} style={{ color: 'var(--fg-3)' }} />{preview || '—'}</> : m.name}</span>
          </span>
          <Button type="submit" variant="primary" disabled={!changed || !valid || patch.isPending} loading={patch.isPending}>保存</Button>
        </div>
      </form>
    </Section>
  );
}

/** 国家（名字前的国旗）：选了就存；第一项改回按 IP 自动，没手动设时写出 IP 查到的国家 */
function CountrySection({ m }: { m: MachineView }) {
  const patch = usePatchMachine();
  const auto = m.country_set ? '自动（按 IP）' : `自动（按 IP：${m.country ? countryName(m.country) : '查不到'}）`;
  const save = (v: string) => patch.mutate({ id: m.id, revision: m.revision, country: v || null }, {
    onSuccess: () => toast.ok(v ? `国家已改为${countryName(v)}` : '国家已改回按 IP 自动'), onError: toast.err,
  });
  return (
    <Section>
      <SectionHeader title="国家" />
      <div className="form-grid"><CountrySelect value={m.country_set ?? ''} onChange={save} autoLabel={auto} disabled={patch.isPending} aria-label="国家或地区" /></div>
    </Section>
  );
}

function EntrySection({ m }: { m: MachineView }) {
  const relays = useRelayChoices().data ?? [];
  const add = useAddEntry();
  const [relay, setRelay] = React.useState('');
  const [dlg, setDlg] = React.useState<null | 'move' | 'evacuate'>(null);
  const used = new Set(m.entries.map((e) => e.relay.id));
  const opts = relays.filter((r) => !used.has(r.id)).map((r) => ({ value: r.id, label: r.name }));
  // 线路机直出入口：每台至多一条（移除中的也占着，摘完才能再加，同服务端）
  const canAddDirect = m.kind === 'relay' && m.lifecycle === 'active' && !m.entries.some((e) => e.direct);
  const canMove = m.lifecycle === 'active' && movableEntries(m).length > 0; // 节点换线路机、线路机搬空
  const canAdd = m.kind !== 'relay' && opts.length > 0;
  return (
    <Section>
      <SectionHeader title="入口" />
      {/* 入口的“⋯”菜单与对话框整个入口列表共用一份 */}
      <EntryMenus entries={m.entries}><div className="stack-sm">
        {m.entries.length === 0 && <div className="hint">还没有入口{m.kind === 'relay' && <> <InfoTip text="节点的入口在节点上添加；也可以给这台线路机加一条直出入口。" /></>}</div>}
        {m.entries.map((e) => (
          <div key={e.id} className="manage-row" style={{ border: '1px solid var(--line)', borderRadius: 'var(--r-float)', padding: '8px 12px' }}>
            <div><div className="t">{m.kind === 'relay' ? `${e.port} · ${e.node.name}` : entryLabel(e)}</div><div className="hint">{e.apply_error ? errText(e.apply_error) : e.applied ? '已应用' : '应用中'}</div></div>
            <span className="row" style={{ gap: 8 }}>{entryState(e)}<EntryMenuButton entry={e} /></span>
          </div>
        ))}
      </div></EntryMenus>
      {/* 列表下面的入口操作单成一组：与列表隔一道线，每项一行、各有小标题 */}
      {(canMove || canAddDirect || canAdd) && (
        <div className="manage-group">
          {canMove && m.kind !== 'relay' && (
            <div className="manage-row">
              <div className="t">换线路机 <InfoTip text={'不断网，完成后分享链接会变。\n把入口换到另一台线路机：新入口验收通过后才摘旧入口。'} /></div>
              <Button variant="secondary" disabled={!!m.active_job} onClick={() => setDlg('move')}><Repeat2 />换线路机…</Button>
            </div>
          )}
          {canMove && m.kind === 'relay' && (
            <div className="manage-row">
              <div className="t">搬空线路机 <InfoTip text={'把上面所有节点换到另一台线路机，之后就能安全退役这台。\n这些节点的分享链接会变。'} /></div>
              <Button variant="secondary" onClick={() => setDlg('evacuate')}><PackageOpen />搬空到…</Button>
            </div>
          )}
          {canAddDirect && (
            <div className="manage-row">
              <div><div className="t">直出入口</div><div className="hint">从 {m.ip} 直接出去 <InfoTip text={`不经节点；每台线路机一条，分享链接名「${m.name}-直出」。`} /></div></div>
              <Button variant="secondary" disabled={add.isPending || !!m.active_job} loading={add.isPending}
                onClick={() => add.mutate({ id: m.id, relay_id: m.id }, { onSuccess: () => toast.ok('已提交，直出入口正在应用'), onError: toast.err })}><Plus />添加直出入口</Button>
            </div>
          )}
          {canAdd && (
            <div className="manage-row">
              <div className="t">加入口 <InfoTip text="再挂到另一台线路机，多一条入口和分享链接；原有的入口不受影响。" /></div>
              <span className="row" style={{ gap: 8, flexWrap: 'nowrap' }}>
                <div style={{ width: 160 }}><Select value={relay} onChange={setRelay} options={opts} placeholder="选择线路机" aria-label="要挂到的线路机" /></div>
                <Button disabled={!relay || add.isPending} loading={add.isPending} onClick={() => add.mutate({ id: m.id, relay_id: relay }, { onSuccess: () => { toast.ok('已提交，新入口正在应用'); setRelay(''); }, onError: toast.err })}><Plus />添加入口</Button>
              </span>
            </div>
          )}
        </div>
      )}
      <MoveDialog m={m} open={dlg === 'move'} onOpenChange={(o) => !o && setDlg(null)} onDone={() => setDlg(null)} />
      <EvacuateDialog relay={m} open={dlg === 'evacuate'} onOpenChange={(o) => !o && setDlg(null)} onDone={() => setDlg(null)} />
    </Section>
  );
}

/** 替换节点：另一台在用节点接替这台（软路由出口跟着换、编号交换，这台进备用团队） */
function ReplaceSection({ m }: { m: MachineView }) {
  const [open, setOpen] = React.useState(false);
  return (
    <Section>
      <SectionHeader title="替换" />
      <div className="manage-row">
        <div className="t">替换为另一台节点 <InfoTip text={'新节点接这台的编号，软路由上指向这台的出口改成新节点的入口。\n这台改到备用团队，订阅过期满 1 天后自动删除。'} /></div>
        <Button variant="secondary" disabled={!!m.active_job} onClick={() => setOpen(true)}><Replace />替换为…</Button>
      </div>
      <ReplaceDialog m={m} open={open} onOpenChange={setOpen} onDone={() => setOpen(false)} />
    </Section>
  );
}

/** 宿主机：添加同机 IP 节点（去上架页，宿主机、团队与线路机预填好） */
function IpNodesSection({ m }: { m: MachineView }) {
  return (
    <Section>
      <SectionHeader title="同机 IP 节点" meta={m.ip_nodes.length ? `${m.ip_nodes.length} 个` : undefined} />
      <div className="manage-row">
        <div className="t">添加 IP 节点 <InfoTip text="这台机器上另配了公网 IP 时，每个 IP 上架成一台独立节点（自己的团队编号、入口与分享链接）；SSH、agent 与主机读数都用这台的。" /></div>
        <Button asChild variant="secondary"><Link to={`/provision?host=${m.id}`}><Plus />添加 IP 节点…</Link></Button>
      </div>
    </Section>
  );
}

/** agent：版本、最近上报；升级 = 推送中控当前版本并重启（作业）。同机 IP 节点没有自己的 agent：不给升级，指向宿主机 */
function AgentSection({ m }: { m: MachineView }) {
  const { open } = useDrawer();
  const a = m.agent!;
  const h = m.host_machine;
  const seen = `最近上报 ${a.last_report_at ? F.ago(a.last_report_at) : '从未'}`;
  return (
    <Section>
      <SectionHeader title="agent" meta={a.online ? '在线' : '离线'} />
      <div className="manage-row">
        <div>
          <div className="t">版本 {a.version ?? '未知'}</div>
          <div className="hint">{h ? `用宿主机「${h.name}」的 · ${seen}` : seen} {h && <InfoTip text="这台同机 IP 节点没有自己的 agent；要升级就升级宿主机的。" />}</div>
        </div>
        {h ? <Button variant="secondary" onClick={() => open(h.id, 'manage')}>打开宿主机</Button> : (
          <Button variant="secondary" disabled={!!m.active_job} onClick={() => openUpgrade(m.id)}>升级 agent…</Button>
        )}
      </div>
    </Section>
  );
}

/** 系统调优：记录的调优档与时间（「调优：tune-v1 · 10-23」/「旧档 tune-v0 · 10-23」/「未调优」）；重新调优 = 作业 tune，只写内核网络参数等，不重启 ss/Xray */
function TuneSection({ m }: { m: MachineView }) {
  const tune = useTune();
  const [ask, setAsk] = React.useState(false);
  const t = m.tune!;
  const state = t.profile ? `${t.current ? '' : '旧档 '}${t.profile}${t.at ? ` · ${F.dt(t.at).slice(0, 5)}` : ''}` : '未调优';
  return (
    <Section>
      <SectionHeader title="系统调优" />
      <div className="manage-row">
        <div className="t">调优：{state} <InfoTip text="内核网络参数（缓冲区、拥塞控制、连接队列等）、日志体积上限、时间同步。逐键结果在作业详情里。" /></div>
        <Button variant="secondary" disabled={!!m.active_job} onClick={() => setAsk(true)}>重新调优…</Button>
      </div>
      <ConfirmDialog open={ask} onOpenChange={setAsk} title={`重新调优 ${m.name}？`} confirmLabel="开始调优" confirmDisabled={!!m.active_job}
        description="不重启 ss / Xray，不断开客户连接；只可能重启一次 journald。"
        onConfirm={async () => { await tune.mutateAsync(m.id); toast.ok(`已提交 ${m.name} 的调优作业`); }} />
    </Section>
  );
}

/** 维护：开一个只针对这台的维护窗口（期间不发告警，卡片显示「维护」）；维护中可以提前结束。
 *  同机 IP 节点因宿主机维护而「维护中」时，写明是宿主机的，按钮结束宿主机的维护 */
function MaintSection({ m }: { m: MachineView }) {
  const end = useEndMaintenance();
  const silences = useSilences().data;
  const onHost = m.maintenance && !!silences && maintOwner(silences, m).onHost;
  return (
    <Section>
      <SectionHeader title="维护" meta={m.maintenance ? '维护中' : undefined} />
      <div className="manage-row">
        <div><div className="t">维护窗口 <InfoTip text="期间不发告警，卡片显示「维护」" /></div>{onHost && <div className="hint">宿主机「{m.host_machine!.name}」在维护中</div>}</div>
        {m.maintenance
          ? <Button variant="secondary" loading={end.isPending} onClick={() => end.mutate(m, { onSuccess: (o) => toast.ok(`${o.name} 已结束维护`), onError: toast.err })}><Wrench />{onHost ? '结束宿主机维护' : '结束维护'}</Button>
          : <Button variant="secondary" onClick={() => openMaint(m.id)}><Wrench />维护窗口…</Button>}
      </div>
    </Section>
  );
}

function DangerSection({ m, onClosed }: { m: MachineView; onClosed: () => void }) {
  const [dlg, setDlg] = React.useState<null | 'retire' | 'purge' | 'restore'>(null);
  const purge = usePurge();
  // 上架中有作业在跑、退役中：不能退役；上架失败（或上架中但作业已不在跑）可以退役，释放 IP 与编号
  const busyJob = (m.lifecycle === 'provisioning' || m.lifecycle === 'retiring') && !!m.active_job;
  const retired = m.lifecycle === 'retired';
  // 已退役的同机 IP 节点：宿主机在用才能恢复（SSH、agent 都是宿主机的）
  const host = useMachine(retired && m.host_machine ? m.host_machine.id : null).data;
  const hostDown = !!host && host.lifecycle !== 'active';
  // ip_nodes 含已在退役中的；退役宿主机只连带还没开始退役的（同服务端），说法按这个写
  const n = m.ip_nodes.length;

  return (
    <Section>
      <SectionHeader title={retired ? '已退役' : '退役'} />
      <div className="danger-zone">
        {retired ? (
          <>
            <div className="manage-row">
              <div>
                <div className="t">恢复</div>
                {/* 宿主机不在用：只说为什么不能恢复，退役时间与宿主机的状态放进 (i) */}
                <div className="hint">{hostDown ? '宿主机不在用，暂不能恢复' : `已于 ${F.dtFull(m.retired_at)} 退役`} <InfoTip text={hostDown
                  ? `已于 ${F.dtFull(m.retired_at)} 退役；宿主机「${host.name}」${lifecycleLabel(host.lifecycle)}。\n同机 IP 节点要等宿主机在用才能恢复。`
                  : '恢复会保持原来的机器 ID（历史、账单、审计接上），重新走一遍上架，编号接在团队末尾。'} /></div>
              </div>
              <Button variant="primary" disabled={hostDown} onClick={() => setDlg('restore')}><Undo2 />恢复…</Button>
            </div>
            <div className="manage-row">
              <div>
                <div className="t">彻底删除</div>
                <div className="hint">无法找回，审计记录保留</div>
              </div>
              <Button variant="outline-danger" onClick={() => setDlg('purge')}><Trash2 />彻底删除…</Button>
            </div>
          </>
        ) : (
          <div className="manage-row">
            <div>
              <div className="t">退役</div>
              <div className="hint">
                {m.host_machine ? `宿主机「${m.host_machine.name}」不受影响 ` : n > 0 ? `${n} 个同机 IP 节点会一起退役 ` : ''}
                <InfoTip text={[
                  m.host_machine ? '只退役这个 IP 的节点：摘除它的入口，宿主机上它自己的 SS 实例可在退役时选择清理。' : '停用入口、撤销令牌，机器进入「已退役」，不再出现在首页和统计里。',
                  m.kind !== 'relay' && '同团队后面的节点会自动前移补号。', n > 0 && `这台机器上有 ${n} 个同机 IP 节点，还没开始退役的会一起退役。`,
                ].filter(Boolean).join('\n')} />
              </div>
            </div>
            <Button variant="outline-danger" disabled={busyJob || m.lifecycle === 'retiring'} onClick={() => setDlg('retire')}>退役…</Button>
          </div>
        )}
        {busyJob && m.active_job && <Note tone="info">作业进行中，暂不能操作。<Link to={`/jobs/${m.active_job.id}`} className="btn btn-link" onClick={onClosed}>查看作业</Link></Note>}
        {/* 退役作业失败：机器仍是退役中，不能再发起退役，只能重试那个作业 */}
        {m.lifecycle === 'retiring' && m.failed_job && !m.active_job && (
          <Note tone="bad">退役作业失败：<Link to={`/jobs/${m.failed_job.id}`} className="btn btn-link" onClick={onClosed}>到作业页重试</Link></Note>
        )}
      </div>

      <RestoreDialog m={m} open={dlg === 'restore'} onOpenChange={(o) => !o && setDlg(null)} onDone={onClosed} />
      <RetireDialog m={m} open={dlg === 'retire'} onOpenChange={(o) => !o && setDlg(null)} onDone={onClosed} />

      <TypedConfirmDialog open={dlg === 'purge'} onOpenChange={(o) => !o && setDlg(null)} title={`彻底删除 ${m.name}？`} expected={m.name} prompt="请输入显示名" confirmLabel="彻底删除" danger
        description={<>删除后无法找回 <InfoTip text={'会删除机器记录与全部遥测数据；审计记录保留。\n它的入口近 35 天的流量还计入其他在用机器的统计时，要等到期后才能删。'} /></>}
        onConfirm={async () => { await purge.mutateAsync({ id: m.id, confirm: m.name }); toast.ok('已提交删除作业'); onClosed(); }} />
    </Section>
  );
}
