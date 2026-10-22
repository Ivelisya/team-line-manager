// 替换节点：新节点接替旧节点——软路由上指向旧节点的出口改成新节点的入口，编号交换（新节点接旧编号，旧节点去备用团队）。
// 抽屉「管理」页与卡片菜单（经 LifecycleDialogsHost）共用；选了新节点就拉预览（只读库），确认后建 replace 作业并跳到作业详情。
import * as React from 'react';
import { useNavigate } from 'react-router-dom';
import { ArrowRight, Check, ChevronDown } from 'lucide-react';
import type { MachineView } from '@/lib/types';
import { useMachines, useReplace, useReplacePreview, useTeams } from '@/lib/queries';
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
  const teamOpts = teams.map((t) => ({ value: t.id, label: t.spare ? <>{t.name} <span className="t-sub">· 备用</span></> : t.name }));
  const exits = pv ? pv.routers.reduce((s, r) => s + r.exits.length, 0) : 0;

  const submit = async () => {
    const r = await replace.mutateAsync({ id: m.id, new_machine_id: pick, old_to: { team_id: team || undefined, number }, routers });
    toast.ok(`已提交替换：${r.new.name} 接替 ${r.old.name}`);
    onDone?.();
    nav(`/jobs/${r.job_id}`);
  };

  return (
    <ConfirmDialog asForm scroll dirty={!!pick} open={open} onOpenChange={onOpenChange} size="md" title={`替换 ${m.name}`} confirmLabel="开始替换" confirmDisabled={!ready}
      description={<>新节点接旧编号，旧节点去备用 <InfoTip text={'先把软路由上指向旧节点的出口改成新节点的入口（逐台备份、改完重启 OpenClash 并测延迟，失败自动换回原配置），再交换编号。\n旧节点放进备用团队，订阅过期满 1 天后自动删除。开始改路由后不能取消。'} /></>}
      onConfirm={submit}>
      <div className="form-grid">
        <Field label="接替的节点" className="span-2"><NodePicker m={m} value={pick} onChange={setPick} /></Field>
        <Field label="旧节点去哪" info="留空：同团队的备用团队（名字以团队名开头的那个；只有一个备用团队时就是它）">
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
            {pv.old_to.team?.spare && <Pill tone="neutral">备用</Pill>}
          </div>
          {pv.routers_enabled && pv.routers.length > 0 && (
            <div>
              <div className="label" style={{ marginBottom: 6 }}>改 {pv.routers.length} 台软路由的 {exits} 个出口</div>
              <ul className="reason-list" style={{ maxHeight: 200, overflow: 'auto' }}>
                {pv.routers.flatMap((r) => r.exits.map((e) => (
                  <li key={`${r.id}-${e.exit}`}>
                    <span>{r.name}</span>
                    {r.state !== 'online' && <Pill tone="bad">{ROUTER_STATE[r.state] ?? r.state}</Pill>}
                    <span className="num">{e.exit}</span>
                    <span className="t-sub num">{e.from} → {e.to}</span>
                  </li>
                )))}
              </ul>
            </div>
          )}
          {pv.renames.length > 0 && (
            <div className="hint">编号压缩：{pv.renames.map((r) => `${r.old_name} → ${r.new_name}`).join('、')} <InfoTip text="新节点原来的团队空出一个编号，后面的节点前移补号。" /></div>
          )}
          {pv.blockers.map((b) => <Note key={b} tone="bad">{b}</Note>)}
          {pv.warnings.map((w) => <Note key={w} tone="warn">{w}</Note>)}
        </div>
      )}
    </ConfirmDialog>
  );
});
