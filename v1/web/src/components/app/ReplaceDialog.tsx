// 替换节点：新节点接替旧节点——软路由上指向旧节点的出口改成新节点的入口，编号交换（新节点接旧编号，旧节点去备用团队）。
// 抽屉「管理」页与卡片菜单（经 LifecycleDialogsHost）共用；选了新节点就拉预览（只读库），确认后建 replace 作业并跳到作业详情。
import * as React from 'react';
import { useNavigate } from 'react-router-dom';
import { ArrowRight, Check, ChevronDown } from 'lucide-react';
import type { MachineView, RouterExitPlan } from '@/lib/types';
import { useMachines, useReplace, useReplacePreview, useTeams } from '@/lib/queries';
import { useAddrName } from '@/lib/routers';
import { errText } from '@/lib/derive';
import '@/styles/routers.css';
import { prefixScore } from '@/lib/match';
import { CheckRow, Field, InfoTip, Input } from '@/components/ui/form';
import { Select } from '@/components/ui/select';
import { toast } from '@/components/ui/toast';
import { Command, CommandEmpty, CommandGroup, CommandInput, CommandItem, CommandList } from '@/components/ui/command';
import { Popover, PopoverContent, PopoverTrigger } from '@/components/ui/overlays';
import { resetOnOpen } from '@/lib/useHeld';
import { ErrorNote, Note, Pill } from './primitives';
import { ConfirmDialog } from './dialogs';

/** 能接替的节点：在用、不是线路机、不是自己 */
export const canReplace = (m: MachineView) => m.kind !== 'relay' && m.lifecycle === 'active';

/** 搜索：名称或 IP 以输入开头的排最前，其次是连着出现的（同上架页的宿主机选择） */
const nodeFilter = (_: string, search: string, keywords?: string[]) => prefixScore(keywords ?? [], search.trim().toLowerCase());

/** 新节点选择：同团队的排最前，其余按团队分组；可按名称、IP、团队搜 */
function NodePicker({ m, value, onChange }: { m: MachineView; value: string; onChange: (v: string) => void }) {
  const [open, setOpen] = React.useState(false);
  const all = useMachines().data ?? [];
  const cands = all.filter((x) => x.id !== m.id && canReplace(x));
  const cur = cands.find((x) => x.id === value);
  // 列表已按默认顺序（团队 → 编号）排好：按团队归组，旧节点的团队放第一组
  const groups = new Map<string, { name: string; items: MachineView[] }>();
  const own = m.team?.id ?? '';
  for (const x of [...cands.filter((c) => (c.team?.id ?? '') === own), ...cands.filter((c) => (c.team?.id ?? '') !== own)]) {
    const k = x.team?.id ?? '';
    if (!groups.has(k)) groups.set(k, { name: k === own ? `同团队（${x.team?.name ?? '无团队'}）` : x.team?.name ?? '无团队', items: [] });
    groups.get(k)!.items.push(x);
  }
  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <button type="button" className="select-trigger cs-trigger" aria-label="新节点">
          <span className={cur ? undefined : 't-muted'}>{cur ? <>{cur.name} <span className="t-sub">{cur.ip}</span></> : '选择接替的节点'}</span>
          <ChevronDown />
        </button>
      </PopoverTrigger>
      <PopoverContent className="cs-pop hs-pop" align="start">
        {open && (
          <Command loop filter={nodeFilter}>
            <CommandInput placeholder="搜索名称、IP 或团队" />
            <CommandList>
              <CommandEmpty>没有可接替的在用节点</CommandEmpty>
              {[...groups.entries()].map(([k, g]) => (
                <CommandGroup key={k || 'none'} heading={g.name}>
                  {g.items.map((x) => (
                    <CommandItem key={x.id} value={x.id} keywords={[x.name, x.ip, x.team?.name ?? '']} onSelect={(v) => { setOpen(false); if (v !== value) onChange(v); }}>
                      <span className="cmd-main">{x.name}</span><span className="cmd-aside num">{x.ip}</span>{x.id === value && <Check className="cs-check" />}
                    </CommandItem>
                  ))}
                </CommandGroup>
              ))}
            </CommandList>
          </Command>
        )}
      </PopoverContent>
    </Popover>
  );
}

/** 值停下来 ms 毫秒后才跟上：编号输入框每按一次键不必各发一次预览 */
function useSettled<T>(v: T, ms: number): T {
  const [s, setS] = React.useState(v);
  React.useEffect(() => { const t = setTimeout(() => setS(v), ms); return () => clearTimeout(t); }, [v, ms]);
  return s;
}

const ROUTER_STATE: Record<string, string> = { offline: '离线', pending: '未接入' };

/** 阻断原因条（替换、换线路机共用）：映射失败的出口原因已经逐行写在上面的小表里，服务端又把同样的话（前面加「软路由「X」的出口「Y」：」）
 *  放进阻断原因——合并成一条；其余的阻断（软路由离线…）逐条列出，英文错误换成中文 */
export function PlanBlockers({ routers, blockers }: { routers: RouterExitPlan[]; blockers: string[] }) {
  const bad = routers.flatMap((r) => r.exits.filter((e) => e.error).map((e) => ({ r, e })));
  const rest = blockers.filter((b) => !bad.some(({ r, e }) => b === `软路由「${r.name}」的出口「${e.exit}」：${e.error}`));
  return (
    <>
      {bad.length > 0 && <Note tone="bad">{bad.length} 个出口对不上新入口：按上表的原因先处理，或不勾「同时改软路由」</Note>}
      {rest.map((b) => <Note key={b} tone="bad">{errText(b)}</Note>)}
    </>
  );
}

/** 预览里要改的软路由出口（替换、换线路机、搬空共用）：一张小表「节点 | 软路由 · 出口 | 旧 → 新」，地址写线路机名（IP 放悬停）。
 *  出口里可能有带原因（error）的：映射不到新入口，和阻断原因一致，在这里标红 */
export function RouterExitList({ routers, title }: { routers: { node?: string; plan: RouterExitPlan }[]; title?: React.ReactNode }) {
  const addr = useAddrName();
  const rows = routers.flatMap(({ node, plan }) => plan.exits.map((e) => ({ node, plan, e })));
  if (rows.length === 0) return null;
  const bad = rows.filter((r) => r.e.error).length;
  const ok = rows.filter((r) => !r.e.error);
  const withNode = rows.some((r) => r.node);
  const shown = (a: string) => { const x = addr(a); return <span className="rx-addr" data-tip={x.tip}>{x.text}</span>; };
  return (
    <div>
      <div className="label" style={{ marginBottom: 6 }}>
        {title ?? (ok.length ? <>改 {new Set(ok.map((r) => r.plan.id)).size} 台软路由的 {ok.length} 个出口</> : '软路由出口')}
        {bad > 0 && <>{' · '}<span className="rt-bad">{bad} 个对不上新入口</span></>}
      </div>
      <div style={{ maxHeight: 200, overflow: 'auto' }}>
        <table className="rx-tbl">
          <thead><tr>{withNode && <th>节点</th>}<th>软路由 · 出口</th><th>改成</th></tr></thead>
          <tbody>
            {rows.map(({ node, plan: r, e }) => (
              <tr key={`${node ?? ''}-${r.id}-${e.exit}`}>
                {withNode && <td className="num">{node}</td>}
                <td><span className="num">{r.name}</span> · <span className="num">{e.exit}</span>{r.state !== 'online' && <> <Pill tone="bad">{ROUTER_STATE[r.state] ?? r.state}</Pill></>}</td>
                <td className="rx-to">{e.error ? <><Pill tone="bad" tip={e.error}>对不上新入口</Pill><div className="rt-bad">{e.error}</div></> : <>{shown(e.from)} → <b>{e.to ? shown(e.to) : '?'}</b></>}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}

/** 编号压缩（后面的节点前移补号）：正常的副作用，收成一行，要看逐台再展开成两列表格（替换、退役共用） */
/** bare：不套提示条（放进别的提示条的列表里，和其它几条说明合成一块时用） */
export function RenameNote({ renames, tip, bare }: { renames: { machine_id: string; old_name: string; new_name: string }[]; tip: string; bare?: boolean }) {
  const [open, setOpen] = React.useState(false);
  if (renames.length === 0) return null;
  const first = renames[0]; const last = renames[renames.length - 1];
  const Wrap = bare ? 'div' : Note;
  return (
    <Wrap style={bare ? { flex: 1, minWidth: 0 } : undefined}>
      <div className="row row-between" style={{ gap: 8 }}>
        <span>补号：{renames.length} 台节点前移（{renames.length > 1 ? `${first.old_name} … ${last.old_name}` : `${first.old_name} → ${first.new_name}`}） <InfoTip text={tip} /></span>
        {renames.length > 1 && <button type="button" className="btn btn-link btn-sm" style={{ height: 'auto', padding: 0 }} aria-expanded={open} onClick={() => setOpen((v) => !v)}>{open ? '收起' : '展开'}</button>}
      </div>
      {open && (
        <div style={{ maxHeight: 200, overflow: 'auto', marginTop: 6 }}>
          <table className="rx-tbl">
            <thead><tr><th>原名</th><th>补号后</th></tr></thead>
            <tbody>{renames.map((r) => <tr key={r.machine_id}><td className="num">{r.old_name}</td><td className="num"><b>{r.new_name}</b></td></tr>)}</tbody>
          </table>
        </div>
      )}
    </Wrap>
  );
}

export const ReplaceDialog = resetOnOpen(function ReplaceDialog({ m, open, onOpenChange, onDone }: { m: MachineView; open: boolean; onOpenChange: (o: boolean) => void; onDone?: () => void }) {
  const nav = useNavigate();
  const replace = useReplace();
  const teams = useTeams().data ?? [];
  const [pick, setPick] = React.useState('');
  const [team, setTeam] = React.useState('');
  const [num, setNum] = React.useState('');
  const [routers, setRouters] = React.useState(true);
  const settledNum = useSettled(num, 300);
  const n = Number(settledNum);
  const number = settledNum && Number.isInteger(n) && n > 0 ? n : undefined;
  const q = useReplacePreview(m.id, open, { new_machine_id: pick, team_id: team || undefined, number, routers });
  // 只认这次打开后取回的、且和当前选择对得上的预览（换选择时先留着上一份，调淡）
  const pv = q.isFetchedAfterMount && q.data && q.data.new.id === pick ? q.data : undefined;
  const stale = q.isFetching || settledNum !== num;
  const ready = !!pv && pv.ok && !stale && !q.isError;
  const teamOpts = teams.map((t) => ({ value: t.id, label: t.spare && !t.name.includes('备用') ? <>{t.name} <span className="t-sub">· 备用</span></> : t.name }));

  const submit = async () => {
    const r = await replace.mutateAsync({ id: m.id, new_machine_id: pick, old_to: { team_id: team || undefined, number }, routers });
    toast.ok(`已提交替换：${r.new.name} 接替 ${r.old.name}`);
    onDone?.();
    nav(`/jobs/${r.job_id}`);
  };

  return (
    <ConfirmDialog asForm scroll dirty={!!pick} open={open} onOpenChange={onOpenChange} size="md" title={`替换 · ${m.name}`} confirmLabel="开始替换" confirmDisabled={!ready}
      description={<>新节点接旧编号，旧节点去备用 <InfoTip text={'先把软路由上指向旧节点的出口改成新节点（每台先备份，改完重启 OpenClash 并测延迟，失败会自动换回），再交换编号\n旧节点放进备用团队，到期满 1 天后自动回收。开始改路由后不能取消'} /></>}
      onConfirm={submit}>
      <div className="form-grid">
        <Field label="接替的节点" className="span-2"><NodePicker m={m} value={pick} onChange={setPick} /></Field>
        <Field label="旧节点去哪" info="留空则放进同团队的备用团队（名字以团队名开头；只有一个备用团队时就是它）">
          <Select value={team} onChange={setTeam} options={teamOpts} allowEmpty="自动（备用团队）" />
        </Field>
        <Field label="编号"><Input inputMode="numeric" value={num} onChange={(e) => setNum(e.target.value.replace(/\D/g, ''))} placeholder={pv?.old_to.number != null && !number ? `自动：${pv.old_to.number}` : '自动'} /></Field>
        <div className="span-2"><CheckRow checked={routers} onChange={setRouters}>同时改软路由 <InfoTip text="不改的话只交换编号，软路由上的出口仍指向旧节点。" /></CheckRow></div>
      </div>

      {pick && !pv && !q.isError && <div className="hint">正在预览…</div>}
      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()}>预览失败：</ErrorNote>}
      {pv && (
        <div className="stack-sm" style={{ opacity: stale ? 0.6 : 1 }}>
          <div className="row" style={{ gap: 8 }}>
            <span className="t-sub" style={{ minWidth: 48 }}>新节点</span>
            <span className="preview-name">{pv.new.name}<ArrowRight size={14} style={{ color: 'var(--fg-3)' }} />{pv.new_name}</span>
          </div>
          <div className="row" style={{ gap: 8 }}>
            <span className="t-sub" style={{ minWidth: 48 }}>旧节点</span>
            <span className="preview-name">{pv.old.name}<ArrowRight size={14} style={{ color: 'var(--fg-3)' }} />{pv.old_to.name}</span>
            {pv.old_to.team?.spare && !pv.old_to.team.name.includes('备用') && <Pill tone="neutral">备用</Pill>}
          </div>
          {pv.routers_enabled && <RouterExitList routers={pv.routers.map((plan) => ({ node: pv.old.name, plan }))} />}
          <RenameNote renames={pv.renames} tip="新节点原来的团队空出一个编号，后面的节点前移补号，编号保持连续。" />
          <PlanBlockers routers={pv.routers_enabled ? pv.routers : []} blockers={pv.blockers} />
          {pv.warnings.map((w) => <Note key={w} tone="warn">{w}</Note>)}
        </div>
      )}
    </ConfirmDialog>
  );
});
