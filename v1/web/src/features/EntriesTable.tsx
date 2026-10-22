// 入口表：抽屉的概览 / 连接质量 / 流量页签共用。线路机的入口可能有几十条：异常的排在前面，先露 max(ENTRY_CAP, 异常条数) 条，表尾展开全部。
import * as React from 'react';
import type { EntryView, MachineView } from '@/lib/types';
import * as F from '@/lib/format';
import { entryBad, probeErr, probeFail, probeFailLabel, reachFail, tcpMs } from '@/lib/derive';
import { entryState, Num, Pill } from '@/components/app/primitives';
import { ShowAll } from '@/components/app/Pagination';

/** 入口表先露的条数（抽屉入口表、线路机页共用） */
export const ENTRY_CAP = 10;
/** 入口表的表头说明：「29 条 · 2 条异常」（异常 = 中控连不上、探测不通或应用失败，见 entryBad） */
export const entriesMeta = (m: MachineView) => { const bad = m.entries.filter(entryBad).length; return `${m.entries.length} 条${bad ? ` · ${bad} 条异常` : ''}`; };

const failCell = (p: NonNullable<EntryView['tcp']>) => <span style={{ color: 'var(--bad-text)' }} data-tip={probeErr(p.detail)}>失败</span>;
/** 出口单元格（线路机页共用）：只写通不通，悬停给出口 IP 与这次出口验证的耗时（经节点去中控，不是线路延迟）；
 *  失败的悬停看错误（英文原始错误换成中文，errText） */
export const probeCell = (p: EntryView['egress']) => (p == null ? <span className="t-muted">—</span> : p.ok
  ? <span data-tip={`${p.detail ? `出口 IP ${p.detail}\n` : ''}出口验证耗时 ${p.latency_ms == null ? '—' : `${Math.round(p.latency_ms)} ms`}`}>正常</span>
  : failCell(p));
/** TCP 单元格：线路机 → 节点的建连延迟，近 1 小时中位数（悬停给最近一次）；直出入口没有节点，不做 TCP 探测 */
export const tcpCell = (e: EntryView) => {
  const p = e.tcp;
  if (e.direct) return <span className="t-muted" data-tip="直出入口不经节点，不做 TCP 探测">—</span>;
  if (p == null) return <span className="t-muted">—</span>;
  if (!p.ok) return failCell(p);
  return <span data-tip={`近 1 小时中位数；最近一次 ${p.latency_ms == null ? '—' : `${Math.round(p.latency_ms)} ms`}`}><Num f={F.ms(tcpMs(p))} /></span>;
};

/** 入口状态列：中控连不上 > 线路机到节点探测不通 > 配置状态；正常的不写（一整列同一个绿胶囊没有信息，同线路页） */
export const entryStatus = (e: EntryView) => {
  const reach = reachFail(e); const fail = probeFail(e);
  if (reach) return <Pill tone="bad" tip={`中控连不上线路机的 ${e.port} 端口（${F.dt(reach.ts)} 最近一次检查）${reach.detail ? `\n${probeErr(reach.detail)}` : ''}`}>中控连不上</Pill>;
  return fail ? <Pill tone="bad" tip={probeErr(fail.detail)}>{probeFailLabel(e)}</Pill> : entryState(e, true);
};

/** onlyBad：只列异常的入口（线路机概览：正常的几十条不重复列，全部在连接质量页签） */
export function EntriesTable({ m, traffic, onlyBad }: { m: MachineView; traffic?: boolean; onlyBad?: boolean }) {
  const [all, setAll] = React.useState(false);
  const sorted = React.useMemo(() => (onlyBad ? m.entries.filter(entryBad) : [...m.entries.filter(entryBad), ...m.entries.filter((e) => !entryBad(e))]), [m.entries, onlyBad]);
  // 流量页：已移除入口（换线路机后的旧入口）本月的流量也计入汇总，单列一行，各行加起来等于汇总
  const removed = traffic ? m.removed_traffic.month_up + m.removed_traffic.month_down : 0;
  if (!m.entries.length && !removed) return <div className="hint">还没有入口</div>;
  if (onlyBad && !sorted.length) return <div className="hint">入口都正常</div>;
  const cap = Math.max(ENTRY_CAP, m.entries.filter(entryBad).length);
  const entries = all ? sorted : sorted.slice(0, cap);
  return (
    <div className="table-wrap is-boxed">
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
      <ShowAll total={sorted.length} cap={cap} open={all} onToggle={() => setAll((v) => !v)} />
    </div>
  );
}

/** 一行一个 memo：实时推送只换变了的入口（live.ts 的入口补丁把没变的原样返回），线路机展开全部的上百行不跟着重画 */
const EntryRow = React.memo(function EntryRow({ e, relay, traffic }: { e: EntryView; relay: boolean; traffic: boolean }) {
  return (
    <tr>
      <td className="strong">{relay ? `${e.port} · ${e.node.name}` : `${e.relay.name}:${e.port}`}</td>
      <td>{entryStatus(e)}</td>{e.lifecycle === 'disabled' ? <><td className="r t-muted">—</td><td className="r t-muted">—</td></> : <><td className="r">{tcpCell(e)}</td><td className="r">{probeCell(e.egress)}</td></>}
      {traffic && <td className="r"><Num f={F.bytes(e.traffic.month_up + e.traffic.month_down)} /></td>}
    </tr>
  );
});
