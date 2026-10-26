// 抽屉“管理”页：改名/改编号（带实时预览）、国家与备注、入口操作、同机 IP 节点、agent、退役（移入备用）/ 立即回收（备用里的机器）。
// 操作类的行按苹果设置页的分组列表排：一组一个圆角容器（.mgroup），行之间细线分隔，只有行标签，没有重复的小节标题。
import * as React from 'react';
import { Link } from 'react-router-dom';
import { ArrowRight } from 'lucide-react';
import type { MachineView } from '@/lib/types';
import { maintOwner, useAddEntry, useEndMaintenance, usePatchMachine, useJob, useLastSpeedtest, useRelayChoices, useRenamePreview, useSilences, useSpeedtest, useSsRefresh, useTeams, useTune } from '@/lib/queries';
import { openMaint, openUpgrade } from '@/lib/nodeActions';
import { useDrawer } from '@/lib/drawer';
import { entryBad, errText, meta, mmdd, reachWho, recycleOn } from '@/lib/derive';
import { jobLive } from '@/lib/jobs';
import { useReachLabel } from '@/lib/probePoints';
import * as F from '@/lib/format';
import { Button } from '@/components/ui/button';
import { Field, InfoTip, Input, Textarea } from '@/components/ui/form';
import { Select } from '@/components/ui/select';
import { toast } from '@/components/ui/toast';
import { Note, Section, SectionHeader } from '@/components/app/primitives';
import { ShowAll } from '@/components/app/Pagination';
import { RouterMark, entryStatus } from './EntriesTable';
import { useEntryRouters } from '@/lib/routers';
import { ConfirmDialog } from '@/components/app/dialogs';
import { tuneBufText, tuneParamsText } from '@/components/app/TuneResult';
import { isSpeedtestOutput, speedtestLine } from '@/components/app/SpeedtestResult';
import { PingOverrideSection } from './PingTab';
import { RetireDialog } from '@/components/app/RetireDialog';
import { EntryMenuButton, EntryMenus, entryLabel } from '@/components/app/EntryActions';
import { EvacuateDialog, MoveDialog, movableEntries } from '@/components/app/LifecycleDialogs';
import { ReplaceDialog, canReplace } from '@/components/app/ReplaceDialog';
import { CountrySelect } from '@/components/app/CountrySelect';
import { countryName } from '@/lib/flags';

/** 备注长度上限（同服务端 MAX_NOTE_CHARS） */
const NOTE_MAX = 2000;

export function ManageTab({ m, onClosed }: { m: MachineView; onClosed: () => void }) {
  const retired = m.lifecycle === 'retired';
  const active = m.lifecycle === 'active';
  const ipNodes = m.kind === 'vps' && !m.host_machine && active;
  return (
    <>
      {!retired && <RenameSection m={m} />}
      {/* 线路机：三网 PING 的单独设置（目标与方式，默认跟随设置页的全局） */}
      {m.kind === 'relay' && active && <Section><SectionHeader title={<>三网 PING <InfoTip text="线路机定时测这些目标的延迟和丢包。勾选「单独设置」，只改这一台" /></>} /><PingOverrideSection id={m.id} /></Section>}
      {/* 在用的机器没有入口时也要能加：节点「添加入口」，线路机「添加直出入口」 */}
      {!retired && (m.lifecycle === 'active' || m.entries.length > 0) && <EntrySection m={m} />}
      {/* 在用的普通 VPS 节点都能当宿主机：上面另配的公网 IP 各上架成一台同机 IP 节点 */}
      {(canReplace(m) || ipNodes || active) && (
        <Section><div className="mgroup">
          {canReplace(m) && <ReplaceRow m={m} />}
          {ipNodes && <IpNodesRow m={m} />}
          {m.agent && active && <AgentRow m={m} />}
          {m.tune && active && <TuneRow m={m} />}
          {m.kind === 'vps' && !m.host_machine && active && m.entries.length > 0 && <SsRefreshRow m={m} />}
          {m.kind !== 'proxy' && active && <SpeedtestRow m={m} />}
          {active && <MaintRow m={m} />}
        </div></Section>
      )}
      <DangerSection m={m} onClosed={onClosed} />
    </>
  );
}

/** 改名 / 改编号 / 国家 / 备注：团队 + 编号（节点）或名称（线路机），实时预览新显示名；国家（名字前的国旗）、备注改了不立即保存，和名字一起点「保存」。
 *  编号补成两位（同名字里的「05」） 
 *  当前显示名只在改了之后作为「旧名 → 新名」出现（没改时它就是上面的输入，不重复） */
function RenameSection({ m }: { m: MachineView }) {
  const teams = useTeams().data;
  // 选项只随团队列表变：抽屉随实时推送重渲染时团队下拉（memo）不跟着重画
  const teamOpts = React.useMemo(() => (teams ?? []).map((t) => ({ value: t.id, label: t.name })), [teams]);
  const patch = usePatchMachine();
  const isRelay = m.kind === 'relay';
  const [team, setTeam] = React.useState(m.team?.id ?? '');
  const [num, setNum] = React.useState(m.number != null ? F.pad2(m.number) : '');
  const [label, setLabel] = React.useState(m.label ?? '');
  const [country, setCountry] = React.useState(m.country_set ?? '');
  const [note, setNote] = React.useState(m.note ?? '');
  // 实时推送会改 revision 和编号（如同团队有机器退役后自动压缩编号）：只同步用户没动过的字段（输入仍等于上次的服务端值），
  // 正在编辑的不覆盖；换了机器则全部重置
  const srv = { id: m.id, team: m.team?.id ?? '', num: m.number != null ? F.pad2(m.number) : '', label: m.label ?? '', country: m.country_set ?? '', note: m.note ?? '' };
  const last = React.useRef(srv);
  React.useEffect(() => {
    const prev = last.current; last.current = srv;
    const keep = (v: string, was: string, now: string) => (prev.id === srv.id && v !== was ? v : now);
    setTeam((v) => keep(v, prev.team, srv.team)); setNum((v) => keep(v, prev.num, srv.num)); setLabel((v) => keep(v, prev.label, srv.label)); setCountry((v) => keep(v, prev.country, srv.country)); setNote((v) => keep(v, prev.note, srv.note));
  }, [srv.id, srv.team, srv.num, srv.label, srv.country, srv.note]); // eslint-disable-line react-hooks/exhaustive-deps

  const teamName = teams?.find((t) => t.id === team)?.name;
  const n = Number(num);
  const numOk = Number.isInteger(n) && n >= 1 && n <= 999; // 与服务端一致：编号范围 1–999
  const preview = isRelay ? label.trim() : teamName && numOk ? F.displayName(teamName, n) : '';
  const nameChanged = isRelay ? label.trim() !== (m.label ?? '') : team !== (m.team?.id ?? '') || (numOk && n !== m.number);
  const countryChanged = country !== (m.country_set ?? '');
  const noteChanged = note.trim() !== (m.note ?? '').trim();
  const changed = nameChanged || countryChanged || noteChanged;
  const valid = isRelay ? label.trim().length > 0 : !!team && numOk;
  // 换团队时原团队剩下的节点按编号连续压缩（同事务）：保存前列出连带改号的
  const teamMoved = !isRelay && !!team && !!m.team && team !== m.team.id;
  const also = useRenamePreview(m.id, team, numOk ? n : null, teamMoved).data?.renames;
  const auto = m.country_set ? '自动（按 IP）' : `自动（按 IP：${m.country ? countryName(m.country) : '查不到'}）`;

  const save = () => {
    const body = { ...(nameChanged ? (isRelay ? { label: label.trim() } : { team_id: team, number: n }) : null), ...(countryChanged ? { country: country || null } : null), ...(noteChanged ? { note: note.trim() } : null) };
    const what = [countryChanged && '国家', noteChanged && '备注'].filter(Boolean).join('、');
    patch.mutate({ id: m.id, revision: m.revision, ...body }, {
      onSuccess: () => toast.ok(nameChanged ? `已改名为 ${preview}${what ? `，${what}已更新` : ''}` : countryChanged && !noteChanged ? (country ? `国家已改为${countryName(country)}` : '国家已改回按 IP 自动') : `${what}已更新`), onError: toast.err,
    });
  };
  return (
    <Section>
      <SectionHeader title={<>{isRelay ? '改名 / 国家 / 备注' : '改名 / 改编号 / 国家 / 备注'} <InfoTip text="不影响现有连接。只改显示名和线路机上的备注" /></>} />
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
        <Field label="国家" className="span-2" info="显示在名字前的国旗。默认按 IP 判断，不准可以手动选"><CountrySelect value={country} onChange={setCountry} autoLabel={auto} disabled={patch.isPending} aria-label="国家或地区" /></Field>
        <Field label="备注" className="span-2" error={note.length > NOTE_MAX ? `备注最多 ${NOTE_MAX} 个字符` : undefined}><Textarea rows={2} value={note} onChange={(e) => setNote(e.target.value)} placeholder="例如：客户专用、合同到 2027 年" disabled={patch.isPending} /></Field>
        {teamMoved && !!also?.length && (
          <div className="span-2 t-sub">
            {m.team!.name} 的编号随之连续：{also.map((r) => `${r.old_name} → ${r.new_name}`).join('，')}
          </div>
        )}
        <div className="span-2 row row-between">
          {/* 改了名字才出现「旧名 → 新名」 */}
          <span className="row" style={{ gap: 8 }}>
            {nameChanged && <><span className="t-sub">新显示名</span><span className="preview-name">{m.name}<ArrowRight size={14} style={{ color: 'var(--fg-3)' }} />{preview || '—'}</span></>}
          </span>
          <Button type="submit" variant="primary" disabled={!changed || !valid || note.length > NOTE_MAX || patch.isPending} loading={patch.isPending}>保存</Button>
        </div>
      </form>
    </Section>
  );
}

/** 管理页签的入口列表先露的条数 */
const MANAGE_CAP = 5;

function EntrySection({ m }: { m: MachineView }) {
  const who = reachWho(useReachLabel());
  const relays = useRelayChoices().data ?? [];
  const add = useAddEntry();
  const routers = useEntryRouters();
  const [relay, setRelay] = React.useState('');
  const [dlg, setDlg] = React.useState<null | 'move' | 'evacuate'>(null);
  const used = new Set(m.entries.map((e) => e.relay.id));
  const opts = relays.filter((r) => !used.has(r.id)).map((r) => ({ value: r.id, label: r.name }));
  // 线路机直出入口：每台至多一条（移除中的也占着，摘完才能再加，同服务端）
  const canAddDirect = m.kind === 'relay' && m.lifecycle === 'active' && !m.entries.some((e) => e.direct);
  const canMove = m.lifecycle === 'active' && movableEntries(m).length > 0; // 节点换线路机、线路机搬空
  const canAdd = m.kind !== 'relay' && opts.length > 0;
  // 线路机的入口有几十条：异常的排前面，先露前 5 条（异常的全露），其余「显示全部」
  const [all, setAll] = React.useState(false);
  const sorted = React.useMemo(() => [...m.entries.filter(entryBad), ...m.entries.filter((e) => !entryBad(e))], [m.entries]);
  const cap = Math.max(MANAGE_CAP, m.entries.filter(entryBad).length);
  const shown = all ? sorted : sorted.slice(0, cap);
  return (
    <Section>
      <SectionHeader title="入口" />
      {/* 入口的“⋯”菜单与对话框整个入口列表共用一份 */}
      <EntryMenus entries={m.entries}><div className="stack-sm">
        {m.entries.length === 0 && <div className="hint">还没有入口{m.kind === 'relay' && <> <InfoTip text="节点的入口要在节点上添加。也可以给这台线路机加一条直出入口" /></>}</div>}
        {/* 入口行的副标题是配置状态（是否已推到线路机），右边的胶囊只标不正常的（中控连不上、探测不通…），两者各说各的 */}
        {m.entries.length > 0 && <div className="mgroup">{shown.map((e) => (
          <div key={e.id} className="manage-row">
            <div><div className="t">{m.kind === 'relay' ? `${e.port} · ${e.node.name}` : entryLabel(e)}{routers.get(e.id) && <RouterMark users={routers.get(e.id)!} />}</div><div className="hint">{e.apply_error ? errText(e.apply_error) : e.applied ? '配置已应用' : '配置未应用'}</div></div>
            <span className="row" style={{ gap: 8 }}>{entryStatus(e, who)}<EntryMenuButton entry={e} /></span>
          </div>
        ))}<ShowAll total={m.entries.length} cap={cap} open={all} onToggle={() => setAll((v) => !v)} /></div>}
      </div></EntryMenus>
      {/* 列表下面的入口操作单成一组：每项一行 */}
      {(canMove || canAddDirect || canAdd) && (
        <div className="mgroup">
          {canMove && m.kind !== 'relay' && (
            <div className="manage-row">
              <div className="t">换线路机 <InfoTip text={'换的过程中不断网，完成后分享链接会变\n新入口验证通过后，才会移除旧的'} /></div>
              <Button variant="secondary" disabled={!!m.active_job} onClick={() => setDlg('move')}>换线路机…</Button>
            </div>
          )}
          {canMove && m.kind === 'relay' && (
            <div className="manage-row">
              <div className="t">搬空线路机 <InfoTip text={'把上面的节点都换到另一台线路机，之后就可以放心退役这台\n这些节点的分享链接会变'} /></div>
              <Button variant="secondary" onClick={() => setDlg('evacuate')}>搬空到…</Button>
            </div>
          )}
          {canAddDirect && (
            <div className="manage-row">
              <div><div className="t">直出入口</div><div className="hint">从 {m.ip} 直接出去 <InfoTip text={`不经过节点。每台线路机只能有一条，分享链接叫「${m.name}-直出」`} /></div></div>
              <Button variant="secondary" disabled={add.isPending || !!m.active_job} loading={add.isPending}
                onClick={() => add.mutate({ id: m.id, relay_id: m.id }, { onSuccess: () => toast.ok('已提交，直出入口正在应用'), onError: toast.err })}>添加直出入口</Button>
            </div>
          )}
          {canAdd && (
            <div className="manage-row is-ctl">
              <div className="t">加入口 <InfoTip text="挂到另一台线路机，多一条入口和分享链接，原有的不受影响" /></div>
              <span className="row" style={{ gap: 8, flexWrap: 'nowrap' }}>
                <div style={{ flex: '0 1 160px', minWidth: 0 }}><Select value={relay} onChange={setRelay} options={opts} placeholder="选择线路机" aria-label="要挂到的线路机" /></div>
                <Button disabled={!relay || add.isPending} loading={add.isPending} onClick={() => add.mutate({ id: m.id, relay_id: relay }, { onSuccess: () => { toast.ok('已提交，新入口正在应用'); setRelay(''); }, onError: toast.err })}>添加入口</Button>
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
function ReplaceRow({ m }: { m: MachineView }) {
  const [open, setOpen] = React.useState(false);
  return (
    <>
      <div className="manage-row">
        <div className="t">替换为另一台节点 <InfoTip text={'新节点沿用这台的编号，软路由上指向这台的出口也会改成新节点\n这台移到备用团队，到期满 1 天后自动回收'} /></div>
        <Button variant="secondary" disabled={!!m.active_job} onClick={() => setOpen(true)}>替换为…</Button>
      </div>
      <ReplaceDialog m={m} open={open} onOpenChange={setOpen} onDone={() => setOpen(false)} />
    </>
  );
}

/** 宿主机：添加同机 IP 节点（去上架页，宿主机、团队与线路机预填好） */
function IpNodesRow({ m }: { m: MachineView }) {
  return (
    <div className="manage-row">
      <div>
        <div className="t">添加 IP 节点 <InfoTip text={"机器上另配了公网 IP 时，每个 IP 可以上架成一个独立节点，有自己的编号、入口和分享链接\nSSH、agent 和主机读数仍用这台的"} /></div>
        {m.ip_nodes.length > 0 && <div className="hint">已有 {m.ip_nodes.length} 个同机 IP 节点</div>}
      </div>
      <Button asChild variant="secondary"><Link to={`/provision?host=${m.id}`}>添加 IP 节点…</Link></Button>
    </div>
  );
}

/** agent：版本、最近上报；升级 = 推送中控当前版本并重启（作业）。同机 IP 节点没有自己的 agent：不给升级，指向宿主机 */
function AgentRow({ m }: { m: MachineView }) {
  const { open } = useDrawer();
  const a = m.agent!;
  const h = m.host_machine;
  const seen = a.last_report_at ? `最近上报 ${F.ago(a.last_report_at)}` : '从未上报';
  return (
    <div className="manage-row">
      <div>
        <div className="t">agent 版本 {a.version ?? '未知'}</div>
        <div className="hint">{a.online ? '在线' : '离线'} · {h ? `读数来自宿主机「${h.name}」 · ${seen}` : seen} {h && <InfoTip text="同机 IP 节点没有自己的 agent，要升级请升级宿主机" />}</div>
      </div>
      {h ? <Button variant="secondary" onClick={() => open(h.id, 'manage')}>打开宿主机</Button> : (
        <Button variant="secondary" disabled={!!m.active_job} onClick={() => openUpgrade(m.id)}>升级 agent…</Button>
      )}
    </div>
  );
}

/** 系统调优：记录的调优档与时间（「已调优 · 10-23」/「需要重新调优（旧档）」/「未调优」），下面一行是这台的推导参数（缓冲上限从哪来）；重新调优 = 作业 tune，只写内核网络参数等，不重启 ss/Xray */
function TuneRow({ m }: { m: MachineView }) {
  const tune = useTune();
  const [ask, setAsk] = React.useState(false);
  const t = m.tune!;
  // 档名是内部名，放进悬停；这里只写「已调优 · 10-23」，旧档写「需要重新调优」；下面一行是这台的推导参数（缓冲上限从哪来）
  const state = !t.profile ? '未调优' : !t.current ? '有新版调优，建议重新调优' : `已调优${t.at ? ` · ${F.dt(t.at).slice(0, 5)}` : ''}`;
  return (
    <>
      <div className="manage-row">
        <div>
          <div className="t">系统调优 <InfoTip text={`优化网络参数（缓冲区、拥塞控制、连接队列）、日志体积上限和时间同步\n每一项的结果在作业详情里${t.profile ? `\n版本 ${t.profile}${t.at ? ` · ${F.dtFull(t.at)}` : ''}` : ''}`} /></div>
          {/* 同别的行：标题 + 灰色副行（状态 · 推导参数），不写成「系统调优：未调优」 */}
          <div className="hint">{state}{t.params && <> · {tuneBufText(t.params)} <InfoTip text={tuneParamsText(t.params)} /></>}</div>
        </div>
        <Button variant="secondary" disabled={!!m.active_job} onClick={() => setAsk(true)}>{t.profile ? '重新调优…' : '调优…'}</Button>
      </div>
      <ConfirmDialog open={ask} onOpenChange={setAsk} title={`${t.profile ? '重新调优' : '调优'} · ${m.name}`} confirmLabel="开始调优" confirmDisabled={!!m.active_job}
        description="不会重启代理服务，也不会断开客户连接。可能重启一次系统日志服务"
        onConfirm={async () => { await tune.mutateAsync(m.id); toast.ok(`已提交「${m.name}」的调优作业`); }} />
    </>
  );
}

/** SS 配置：把中控当前的 ssserver 配置（含出站目标黑名单）重新推到这台节点（作业 ss_refresh）；配置有变化才重启 ssserver，同机 IP 节点的实例一起刷 */
function SsRefreshRow({ m }: { m: MachineView }) {
  const refresh = useSsRefresh();
  const [ask, setAsk] = React.useState(false);
  return (
    <>
      <div className="manage-row">
        <div>
          <div className="t">SS 配置 <InfoTip text={`重新下发中控当前的 ssserver 配置（含出站目标黑名单）\n配置没有变化就不重启${m.ip_nodes.length > 0 ? '\n同机 IP 节点的 ssserver 一起刷' : ''}\n完成后经入口验收出口`} /></div>
        </div>
        <Button variant="secondary" disabled={!!m.active_job} onClick={() => setAsk(true)}>重新下发…</Button>
      </div>
      <ConfirmDialog open={ask} onOpenChange={setAsk} title={`重新下发 SS 配置 · ${m.name}`} confirmLabel="开始下发" confirmDisabled={!!m.active_job}
        description="配置有变化时会重启 ssserver，该节点的连接会断开几秒"
        onConfirm={async () => { await refresh.mutateAsync(m.id); toast.ok(`已提交「${m.name}」的 SS 配置下发作业`); }} />
    </>
  );
}

/** 测速：机器自己从北美的测速主机下载一个文件，量它出站的平均速率（作业 speedtest）。会用掉约下载量那么多的流量，所以确认里写明。
 *  副行是最近一次的结果（成功写速率与耗时，没下完、失败、进行中各自说明），详细的在作业详情里；同机 IP 节点和宿主机共用网络，提示去宿主机测；家宽代理没有我们的机器，不显示 */
function SpeedtestRow({ m }: { m: MachineView }) {
  const { open } = useDrawer();
  const h = m.host_machine;
  if (h) {
    return (
      <div className="manage-row">
        <div><div className="t">测速</div><div className="hint">同机 IP 节点和宿主机共用网络，请对宿主机「{h.name}」测速</div></div>
        <Button variant="secondary" onClick={() => open(h.id, 'manage')}>打开宿主机</Button>
      </div>
    );
  }
  return <SpeedtestOwnRow m={m} />;
}

function SpeedtestOwnRow({ m }: { m: MachineView }) {
  const st = meta().speedtest;
  const run = useSpeedtest();
  const last = useLastSpeedtest(m.id).data;
  const detail = useJob(last && last.state === 'succeeded' ? last.id : null).data;
  const out = detail?.steps.find((x) => isSpeedtestOutput(x.output))?.output;
  const [ask, setAsk] = React.useState(false);
  const [size, setSize] = React.useState(String(st.default_mb));
  const [target, setTarget] = React.useState(st.targets[0]?.key ?? '');
  const sizeOpts = React.useMemo(() => st.sizes_mb.map((n) => ({ value: String(n), label: `${n} MB` })), [st]);
  const targetOpts = React.useMemo(() => st.targets.map((t) => ({ value: t.key, label: t.name })), [st]);
  const link = last && <Link to={`/jobs/${last.id}`} className="btn btn-link btn-sm" style={{ height: 'auto', padding: 0 }}>作业详情</Link>;
  const hint = !last ? '还没测过'
    : jobLive(last.state) ? <>测速进行中… {link}</>
    : last.state === 'succeeded' ? (out && isSpeedtestOutput(out)
      ? <><span style={out.complete ? undefined : { color: 'var(--warn-text)' }}>{speedtestLine(out)}</span> · {F.dt(last.finished_at).slice(0, 11)} {link}</> : <>上次测速已完成 {link}</>)
    : last.state === 'failed' ? <><span style={{ color: 'var(--bad-text)' }}>上次测速失败{last.error ? `：${errText(last.error)}` : ''}</span> {link}</>
    : <>上次测速已取消 {link}</>;
  return (
    <>
      <div className="manage-row">
        <div>
          <div className="t">测速 <InfoTip text={`这台机器自己从北美的测速主机下载一个文件，量它出站的平均速率（线路机量的是它本机，不是经入口到节点的链路）\n下载的流量计入这台机器的流量额度\n300 秒内没下完的，速率按已下载的算并标明`} /></div>
          <div className="hint">{hint}</div>
        </div>
        <Button variant="secondary" disabled={!!m.active_job} onClick={() => setAsk(true)}>测速…</Button>
      </div>
      <ConfirmDialog open={ask} onOpenChange={setAsk} title={`测速 · ${m.name}`} confirmLabel="开始测速" confirmDisabled={!!m.active_job || !target}
        description={<>这台机器会自己下载一个测速文件，<b>会用掉约 {size} MB 流量</b>，计入它的流量额度</>}
        onConfirm={async () => { const r = await run.mutateAsync({ id: m.id, size_mb: Number(size), target }); toast.ok(`已提交「${m.name}」的测速作业（${r.target_name} ${r.size_mb} MB）`); }}>
        <div className="form-grid">
          <Field label="下载量" info="越大越能测出跑满的带宽；慢线路 300 秒内下不完的，按已下载的算"><Select value={size} onChange={setSize} options={sizeOpts} /></Field>
          <Field label="测速目标" info="北美的测速主机"><Select value={target} onChange={setTarget} options={targetOpts} /></Field>
        </div>
      </ConfirmDialog>
    </>
  );
}

/** 维护：开一个只针对这台的维护窗口（期间不发告警，卡片显示「维护」）；维护中可以提前结束。
 *  同机 IP 节点因宿主机维护而「维护中」时，写明是宿主机的，按钮结束宿主机的维护 */
function MaintRow({ m }: { m: MachineView }) {
  const end = useEndMaintenance();
  const silences = useSilences().data;
  const onHost = m.maintenance && !!silences && maintOwner(silences, m).onHost;
  return (
    <div className="manage-row">
        <div><div className="t">维护窗口 <InfoTip text="期间不发告警，卡片上显示「维护」" /></div>{m.maintenance && <div className="hint">{onHost ? `宿主机「${m.host_machine!.name}」在维护中` : '维护中'}</div>}</div>
        {m.maintenance
          ? <Button variant="secondary" loading={end.isPending} onClick={() => end.mutate(m, { onSuccess: (o) => toast.ok(`${o.name} 已结束维护`), onError: toast.err })}>{onHost ? '结束宿主机维护' : '结束维护'}</Button>
          : <Button variant="secondary" onClick={() => openMaint(m.id)}>维护窗口…</Button>}
    </div>
  );
}

function DangerSection({ m, onClosed }: { m: MachineView; onClosed: () => void }) {
  const [open, setOpen] = React.useState(false);
  // 上架中有作业在跑、退役中：不能退役；上架失败（或上架中但作业已不在跑）可以退役，释放 IP 与编号
  const busyJob = (m.lifecycle === 'provisioning' || m.lifecycle === 'retiring') && !!m.active_job;
  // 备用里的机器：退役已经做过了，这里只剩提前回收；要留下它就改回别的团队（上面的改名）
  const inSpare = m.spare && m.lifecycle === 'active';
  const on = recycleOn(m);
  // ip_nodes 含已在退役中的；退役宿主机只连带还没开始退役的（同服务端），说法按这个写
  const n = m.ip_nodes.length;
  if (m.lifecycle === 'retired') return null;

  return (
    <Section>
      <div className="mgroup is-danger">
        <div className="manage-row">
          {inSpare ? (
            <div>
              <div className="t">立即回收</div>
              <div className="hint">
                {m.spare_since ? `已于 ${mmdd(m.spare_since)} 退役` : '在备用团队'}{m.spare_from && `（原 ${m.spare_from}）`}，{on ? `${on} 起自动回收` : '没有订阅，不会自动回收'}
                {' '}<InfoTip text={'清理节点并移入回收站，不等到期\n想留下它，请到上面的「改名」把团队改回去'} />
              </div>
            </div>
          ) : (
            <div>
              <div className="t">退役{m.kind !== 'relay' && <> <InfoTip text={[
                  m.host_machine ? '只退役这个 IP 的节点，宿主机不受影响。' : '默认移入备用团队，机器照常服务，到期满 1 天后自动回收。也可以立即清理并移入回收站',
                  '同团队后面的节点会自动前移补号。', n > 0 && `这台机器上还有 ${n} 个同机 IP 节点，会一起退役`,
                ].filter(Boolean).join('\n')} /></>}</div>
              {/* 线路机不能进备用；节点默认移入备用，没有同机关系时也写一句短话，不留一个孤零零的 (i) */}
              <div className="hint">{m.kind === 'relay' ? '线路机只能立即回收。请先搬空节点，直出入口会一起移除' : m.host_machine ? `宿主机「${m.host_machine.name}」不受影响` : n > 0 ? `${n} 个同机 IP 节点会一起退役` : `移入备用${m.subscription ? ` · ${F.addDays(m.subscription.due_date, 2).slice(5)} 起自动回收` : ''}`}</div>
            </div>
          )}
          <Button variant="outline-danger" disabled={busyJob || m.lifecycle === 'retiring'} onClick={() => setOpen(true)}>{inSpare ? '立即回收…' : '退役…'}</Button>
        </div>
        {busyJob && m.active_job && <Note tone="info">作业进行中，暂不能操作。<Link to={`/jobs/${m.active_job.id}`} className="btn btn-link" onClick={onClosed}>查看作业</Link></Note>}
        {/* 退役作业失败：机器仍是退役中，不能再发起退役，只能重试那个作业 */}
        {m.lifecycle === 'retiring' && m.failed_job && !m.active_job && (
          <Note tone="bad">退役作业失败：<Link to={`/jobs/${m.failed_job.id}`} className="btn btn-link" onClick={onClosed}>到作业页重试</Link></Note>
        )}
      </div>

      <RetireDialog m={m} open={open} onOpenChange={setOpen} onDone={onClosed} />
    </Section>
  );
}
