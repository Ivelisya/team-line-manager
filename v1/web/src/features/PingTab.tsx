// 三网 PING：设置页的全局 8 个槽位（名称、目标）与方式，和线路机「管理」页签里这台的单独设置（PingOverrideSection）。
// 线路机每分钟往外测一轮（每个目标 5 次），只展示延迟与丢包，不开告警。目标留空的槽位不测。
import * as React from 'react';
import { Radar, RotateCcw } from 'lucide-react';
import { PING_PLACEHOLDER, useMachinePing, usePingTargets, useSaveMachinePing, useSavePingTargets, type PingOverride, type PingSlot } from '@/lib/ping';
import { Button } from '@/components/ui/button';
import { InfoTip, Input } from '@/components/ui/form';
import { Select } from '@/components/ui/select';
import { SkeletonRows } from '@/components/ui/skeleton';
import { toast } from '@/components/ui/toast';
import { ErrorNote, Panel, TableWrap } from '@/components/app/primitives';

const MODE_ITEMS = [{ value: 'tcp', label: 'TCP 建连' }, { value: 'icmp', label: 'ICMP' }];
const MODE_TIP = 'TCP：建连量往返时间，目标不写端口时连 80；ICMP：线路机上的系统 ping，用不了时这一轮退回 TCP（卡片悬停里写实际用的方式）';
const TARGET_TIP = 'host、host:port、[IPv6]:port；留空不测';

export function PingTab() {
  const q = usePingTargets();
  return (
    <Panel title={<>三网 PING<InfoTip text="线路机每分钟往外连一轮这些目标（每个 5 次），记延迟中位数与丢包，显示在线路机卡片与抽屉的连接质量里；每台线路机可以在「管理」里单独覆盖" /></>}>
      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
      {q.isLoading ? <SkeletonRows rows={4} /> : q.data && <GlobalForm key={JSON.stringify(q.data.slots) + q.data.mode} mode={q.data.mode} slots={q.data.slots} />}
    </Panel>
  );
}

function GlobalForm({ mode: m0, slots: s0 }: { mode: string; slots: PingSlot[] }) {
  const save = useSavePingTargets();
  const [mode, setMode] = React.useState(m0);
  const [slots, setSlots] = React.useState(s0);
  const dirty = mode !== m0 || JSON.stringify(slots) !== JSON.stringify(s0);
  const set = (i: number, k: keyof PingSlot, v: string) => setSlots((l) => l.map((s, j) => (j === i ? { ...s, [k]: v } : s)));
  return (
    <div className="stack" style={{ gap: 'var(--sp-12)' }}>
      <label className="row" style={{ gap: 8 }}>方式<InfoTip text={MODE_TIP} /><Select sm aria-label="方式" value={mode} onChange={setMode} options={MODE_ITEMS} /></label>
      <TableWrap>
        <table className="table">
          <thead><tr><th style={{ width: 48 }}>槽位</th><th>名称</th><th>目标<InfoTip text={TARGET_TIP} /></th></tr></thead>
          <tbody>
            {slots.map((s, i) => (
              <tr key={i}>
                <td className="num">{i + 1}</td>
                <td><Input sm maxLength={20} value={s.name} onChange={(e) => set(i, 'name', e.target.value)} /></td>
                <td><Input sm value={s.target} placeholder={PING_PLACEHOLDER[i] ?? 'host[:port]'} onChange={(e) => set(i, 'target', e.target.value)} /></td>
              </tr>
            ))}
          </tbody>
        </table>
      </TableWrap>
      <div className="row">
        <Button variant="primary" disabled={!dirty} loading={save.isPending} onClick={() => save.mutate({ mode, slots }, { onSuccess: () => toast.ok('已保存，线路机一分钟内按新目标测'), onError: toast.err })}><Radar />保存</Button>
      </div>
    </div>
  );
}

/** 线路机「管理」页签：这台的三网 PING 单独设置。每个槽位「跟随全局」或填这台自己的名称与目标（目标留空 = 这台不测它） */
export function PingOverrideSection({ id }: { id: string }) {
  const q = useMachinePing(id);
  if (q.isError) return <ErrorNote error={q.error} onRetry={() => void q.refetch()} />;
  if (!q.data) return <SkeletonRows rows={3} />;
  return <OverrideForm key={JSON.stringify(q.data.override)} id={id} global={q.data.global} over={q.data.override} />;
}

function OverrideForm({ id, global, over }: { id: string; global: { mode: string; slots: PingSlot[] }; over: PingOverride | null }) {
  const save = useSaveMachinePing(id);
  const init = React.useMemo(() => Array.from({ length: 8 }, (_, i) => over?.slots?.[i] ?? null), [over]);
  const [mode, setMode] = React.useState<string>(over?.mode ?? 'global');
  const [slots, setSlots] = React.useState<(PingSlot | null)[]>(init);
  const dirty = mode !== (over?.mode ?? 'global') || JSON.stringify(slots) !== JSON.stringify(init);
  const toggle = (i: number, on: boolean) => setSlots((l) => l.map((s, j) => (j === i ? (on ? { ...global.slots[i] } : null) : s)));
  const set = (i: number, k: keyof PingSlot, v: string) => setSlots((l) => l.map((s, j) => (j === i && s ? { ...s, [k]: v } : s)));
  const submit = () => {
    const o: PingOverride = { mode: mode === 'global' ? null : (mode as 'tcp' | 'icmp'), slots };
    const empty = o.mode == null && slots.every((s) => s == null);
    save.mutate(empty ? null : o, { onSuccess: () => toast.ok('已保存，这台一分钟内按新目标测'), onError: toast.err });
  };
  return (
    <div className="stack" style={{ gap: 'var(--sp-12)' }}>
      <label className="row" style={{ gap: 8 }}>方式<InfoTip text={MODE_TIP} />
        <Select sm aria-label="方式" value={mode} onChange={setMode} options={[{ value: 'global', label: `跟随全局（${global.mode === 'icmp' ? 'ICMP' : 'TCP'}）` }, ...MODE_ITEMS]} />
      </label>
      <TableWrap>
        <table className="table">
          <thead><tr><th style={{ width: 48 }}>槽位</th><th style={{ width: 96 }}>单独设置</th><th>名称</th><th>目标<InfoTip text={TARGET_TIP} /></th></tr></thead>
          <tbody>
            {slots.map((s, i) => (
              <tr key={i}>
                <td className="num">{i + 1}</td>
                <td><input type="checkbox" aria-label="单独设置" checked={!!s} onChange={(e) => toggle(i, e.target.checked)} /></td>
                {s ? <>
                  <td><Input sm maxLength={20} value={s.name} onChange={(e) => set(i, 'name', e.target.value)} /></td>
                  <td><Input sm value={s.target} placeholder="留空 = 这台不测" onChange={(e) => set(i, 'target', e.target.value)} /></td>
                </> : <>
                  <td className="t-muted">{global.slots[i]?.name}</td>
                  <td className="t-muted">{global.slots[i]?.target || '（不测）'}</td>
                </>}
              </tr>
            ))}
          </tbody>
        </table>
      </TableWrap>
      <div className="row" style={{ gap: 8 }}>
        <Button variant="primary" disabled={!dirty} loading={save.isPending} onClick={submit}><Radar />保存</Button>
        {(over != null) && <Button onClick={() => save.mutate(null, { onSuccess: () => toast.ok('已全部改回跟随全局'), onError: toast.err })}><RotateCcw />全部跟随全局</Button>}
      </div>
    </div>
  );
}
