// 入口表：抽屉的概览 / 连接质量 / 流量页签共用。
import type { EntryView, MachineView } from '@/lib/types';
import * as F from '@/lib/format';
import { entryState, Num } from '@/components/app/primitives';

const probeCell = (p: EntryView['tcp']) => (p == null ? <span className="t-muted">—</span> : p.ok ? <Num f={F.ms(p.latency_ms)} /> : <span style={{ color: 'var(--bad-text)' }} title={p.detail ?? undefined}>失败</span>);

export function EntriesTable({ m, traffic }: { m: MachineView; traffic?: boolean }) {
  if (!m.entries.length) return <div className="hint">还没有入口</div>;
  return (
    <div className="table-wrap" style={{ border: '1px solid var(--line)', borderRadius: 'var(--r-float)' }}>
      <table className="dt is-one" style={{ minWidth: 480 }}>
        <colgroup><col style={{ width: 132 }} /><col style={{ width: 96 }} /><col style={{ width: 72 }} /><col style={{ width: 72 }} />{traffic && <col style={{ width: 112 }} />}</colgroup>
        <thead><tr><th>{m.kind === 'relay' ? '端口 · 节点' : '线路机:端口'}</th><th>状态</th><th className="r">TCP</th><th className="r">出口</th>{traffic && <th className="r">本月流量</th>}</tr></thead>
        <tbody>
          {m.entries.map((e) => (
            <tr key={e.id}>
              <td className="strong">{m.kind === 'relay' ? `${e.port} · ${e.node.name}` : `${e.relay.name}:${e.port}`}</td>
              <td>{entryState(e)}</td><td className="r">{probeCell(e.tcp)}</td><td className="r">{probeCell(e.egress)}</td>
              {traffic && <td className="r"><Num f={F.bytes(e.traffic.month_up + e.traffic.month_down)} /></td>}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
