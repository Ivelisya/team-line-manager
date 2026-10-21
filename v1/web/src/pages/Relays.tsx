// 线路：每台线路机一张卡——下发状态（配置代数 vs 已下发）+ 入口表（端口、节点、状态、是否已下发、TCP/出口探测、流量）。
// 入口多的线路机（几十条）只先露前 ENTRY_CAP 条，表尾「显示全部 N 条」展开：线路机只有几台，一页里放几个分页器反而难用。
import * as React from 'react';
import { CheckCircle2, CircleAlert, Loader2, PackageOpen, PanelRightOpen } from 'lucide-react';
import { useRelays } from '@/lib/queries';
import { useDrawer } from '@/lib/drawer';
import * as F from '@/lib/format';
import type { EntryView, MachineView } from '@/lib/types';
import { Button } from '@/components/ui/button';
import { Chip, Empty, ErrorNote, HealthPill, KindIcon, Num, PageHead, Pill } from '@/components/app/primitives';
import { NodeHover } from '@/components/app/NodeHover';
import { Waypoints } from 'lucide-react';
import { Skeleton } from '@/components/ui/skeleton';
import { EntryActions } from '@/components/app/EntryActions';
import { entryState } from '@/components/app/primitives';
import { openEvacuate } from '@/lib/nodeActions';
import { ShowAll } from '@/components/app/Pagination';
import { ENTRY_CAP } from '@/features/EntriesTable';

const probe = (p: EntryView['tcp']) => (p == null ? <span className="t-muted">—</span> : p.ok ? <Num f={F.ms(p.latency_ms)} /> : <span style={{ color: 'var(--bad-text)' }} title={p.detail ?? undefined}>失败</span>);

function ApplyStatus({ m }: { m: MachineView }) {
  const r = m.relay; if (!r) return null;
  const pending = r.generation > r.applied_generation;
  const pendingEntries = m.entries.filter((e) => !e.applied && e.lifecycle !== 'disabled').length;
  return (
    <div className="apply-box">
      {r.apply_error ? <Pill tone="bad"><CircleAlert size={12} />下发失败</Pill>
        : pending ? <Pill tone="info" dot="pulse"><Loader2 size={12} className="spin" />下发中</Pill>
        : <Pill tone="ok"><CheckCircle2 size={12} />已下发</Pill>}
      <span>配置代数 <b className="num" style={{ color: 'var(--fg)' }}>{r.generation}</b> · 已应用 <b className="num" style={{ color: 'var(--fg)' }}>{r.applied_generation}</b></span>
      {pending && <span>待下发 {pendingEntries} 条入口</span>}
      <span style={{ marginLeft: 'auto' }}>上次应用 {r.applied_at ? F.ago(r.applied_at) : '—'}</span>
      {r.apply_error && <div style={{ flexBasis: '100%', color: 'var(--bad-text)' }}>{r.apply_error}</div>}
    </div>
  );
}

function RelayCard({ m, open }: { m: MachineView; open: ReturnType<typeof useDrawer>['open'] }) {
  const r = m.relay;
  const due = F.dueFmt(m.subscription?.due_days);
  const [all, setAll] = React.useState(false);
  const entries = all ? m.entries : m.entries.slice(0, ENTRY_CAP);
  return (
    <section className="panel relay-card">
      <header className="relay-head">
        <KindIcon kind="relay" />
        <span className="ttl">{m.name}</span>
        <HealthPill m={m} />
        <span className="t-sub">{r?.public_host ?? m.ip} · {m.ip}</span>
        {m.subscription && <Chip variant={due.tone === 'warn' || due.tone === 'bad' ? 'warn' : undefined}>到期 <Num f={due.text} /></Chip>}
        <span style={{ marginLeft: 'auto' }} className="row">
          {m.lifecycle === 'active' && m.entries.some((e) => e.lifecycle === 'active') && <Button size="sm" onClick={() => openEvacuate(m.id)}><PackageOpen size={14} />搬空到…</Button>}
          <Button size="sm" onClick={() => open(m.id)}><PanelRightOpen size={14} />详情</Button></span>
      </header>
      <ApplyStatus m={m} />
      <div className="table-wrap">
        <table className="dt is-one" style={{ minWidth: 760 }}>
          <colgroup><col style={{ width: 84 }} /><col style={{ width: 150 }} /><col style={{ width: 104 }} /><col style={{ width: 84 }} /><col style={{ width: 80 }} /><col style={{ width: 80 }} /><col style={{ width: 120 }} /><col style={{ width: 52 }} /></colgroup>
          <thead><tr><th>端口</th><th>节点</th><th>状态</th><th>已下发</th><th className="r">TCP</th><th className="r">出口</th><th className="r">本月业务流量</th><th /></tr></thead>
          <tbody>
            {entries.map((e) => (
              <tr key={e.id}>
                <td className="strong num">{e.port}</td>
                <td><NodeHover id={e.node.id}><button className="btn-link btn" style={{ height: 'auto', padding: 0, border: 0, maxWidth: '100%', overflow: 'hidden', textOverflow: 'ellipsis' }} onClick={() => open(e.node.id)}>{e.node.name}</button></NodeHover></td>
                <td>{entryState(e)}</td>
                <td>{e.lifecycle === 'disabled' ? <span className="t-muted">—</span> : e.applied ? <CheckCircle2 size={14} style={{ color: 'var(--ok)' }} aria-label="已下发" /> : <span title={e.apply_error ?? '等待下发'} style={{ color: e.apply_error ? 'var(--bad-text)' : 'var(--fg-2)' }}>{e.apply_error ? '失败' : '待下发'}</span>}</td>
                <td className="r">{probe(e.tcp)}</td><td className="r">{probe(e.egress)}</td>
                <td className="r"><Num f={F.bytes(e.traffic.month_up + e.traffic.month_down)} /></td>
                <td className="c"><EntryActions entry={e} /></td>
              </tr>
            ))}
          </tbody>
        </table>
        {m.entries.length === 0 && <Empty text="这台线路机还没有挂载入口" />}
      </div>
      <ShowAll total={m.entries.length} cap={ENTRY_CAP} open={all} onToggle={() => setAll((v) => !v)} />
    </section>
  );
}

export function RelaysPage() {
  const q = useRelays();
  const { open } = useDrawer();
  const list = q.data ?? [];
  return (
    <div className="stack">
      <PageHead title="线路" note={q.data ? `${list.length} 台线路机 · ${list.reduce((s, m) => s + m.entries.length, 0)} 条入口` : undefined} />
      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
      {q.isLoading && <Skeleton h={220} />}
      {list.map((m) => <RelayCard key={m.id} m={m} open={open} />)}
      {q.data && list.length === 0 && <Empty icon={<Waypoints size={22} />} text="还没有线路机" hint="去“上架”添加第一台线路机" />}
    </div>
  );
}
