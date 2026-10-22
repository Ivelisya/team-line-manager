// 入口表：抽屉的概览 / 连接质量 / 流量页签共用。线路机的入口可能有几十条：先露前 ENTRY_CAP 条，表尾展开全部。
import * as React from 'react';
import type { EntryView, MachineView } from '@/lib/types';
import * as F from '@/lib/format';
import { errText, probeFail, probeFailLabel } from '@/lib/derive';
import { entryState, Num, Pill } from '@/components/app/primitives';
import { ShowAll } from '@/components/app/Pagination';

/** 入口表先露的条数（抽屉入口表、线路机页共用） */
export const ENTRY_CAP = 10;

/** TCP / 出口探测单元格（线路机页共用）：失败的悬停看错误（英文原始错误换成中文，errText） */
export const probeCell = (p: EntryView['tcp']) => (p == null ? <span className="t-muted">—</span> : p.ok ? <Num f={F.ms(p.latency_ms)} /> : <span style={{ color: 'var(--bad-text)' }} data-tip={p.detail ? errText(p.detail) : undefined}>失败</span>);
/** TCP 单元格：直出入口没有节点，不做 TCP 探测 */
export const tcpCell = (e: EntryView) => (e.direct ? <span className="t-muted" data-tip="直出入口不经节点，不做 TCP 探测">—</span> : probeCell(e.tcp));

export function EntriesTable({ m, traffic }: { m: MachineView; traffic?: boolean }) {
  const [all, setAll] = React.useState(false);
  // 流量页：已移除入口（换线路机后的旧入口）本月的流量也计入汇总，单列一行，各行加起来等于汇总
  const removed = traffic ? m.removed_traffic.month_up + m.removed_traffic.month_down : 0;
  if (!m.entries.length && !removed) return <div className="hint">还没有入口</div>;
  const entries = all ? m.entries : m.entries.slice(0, ENTRY_CAP);
  return (
    <div className="table-wrap" style={{ border: '1px solid var(--line)', borderRadius: 'var(--r-float)' }}>
      <table className="dt is-one" style={{ minWidth: 480 }}>
        <colgroup><col style={{ width: 132 }} /><col style={{ width: 96 }} /><col style={{ width: 72 }} /><col style={{ width: 72 }} />{traffic && <col style={{ width: 112 }} />}</colgroup>
        <thead><tr><th>{m.kind === 'relay' ? '端口 · 节点' : '线路机:端口'}</th><th>状态</th><th className="r">TCP</th><th className="r">出口</th>{traffic && <th className="r">本月业务流量</th>}</tr></thead>
        <tbody>
          {entries.map((e) => <EntryRow key={e.id} e={e} relay={m.kind === 'relay'} traffic={!!traffic} />)}
          {removed > 0 && (
            <tr><td className="t-sub" colSpan={4}>已移除的入口</td><td className="r"><Num f={F.bytes(removed)} /></td></tr>
          )}
        </tbody>
      </table>
      <ShowAll total={m.entries.length} cap={ENTRY_CAP} open={all} onToggle={() => setAll((v) => !v)} />
    </div>
  );
}

/** 一行一个 memo：实时推送只换变了的入口（live.ts 的入口补丁把没变的原样返回），线路机展开全部的上百行不跟着重画 */
const EntryRow = React.memo(function EntryRow({ e, relay, traffic }: { e: EntryView; relay: boolean; traffic: boolean }) {
  const fail = probeFail(e);
  return (
    <tr>
      <td className="strong">{relay ? `${e.port} · ${e.node.name}` : `${e.relay.name}:${e.port}`}</td>
      <td>{fail ? <Pill tone="bad" tip={fail.detail ? errText(fail.detail) : undefined}>{probeFailLabel(e)}</Pill> : entryState(e)}</td>{e.lifecycle === 'disabled' ? <><td className="r t-muted">—</td><td className="r t-muted">—</td></> : <><td className="r">{tcpCell(e)}</td><td className="r">{probeCell(e.egress)}</td></>}
      {traffic && <td className="r"><Num f={F.bytes(e.traffic.month_up + e.traffic.month_down)} /></td>}
    </tr>
  );
});
