// 告警页的「月度可用率」页签：按供应商汇总（平均 / 最差的出口与 TCP 成功率，出口为主、断线次数、告警次数）+ 每个在用节点一行，都从差到好（服务端的顺序，前端不重排）。
// 数据来自 GET /reports/uptime：小时汇总里成功次数 ÷ 探测次数，窗口是组织时区的自然日（本月算到今天）。节点行点开机器抽屉的「连接质量」。
import * as React from 'react';
import { CalendarDays } from 'lucide-react';
import * as F from '@/lib/format';
import { useUptime } from '@/lib/queries';
import { openDrawer } from '@/lib/drawer';
import { meta, uptimeTone } from '@/lib/derive';
import type { UptimeItem, UptimeProvider } from '@/lib/types';
import { InfoTip } from '@/components/ui/form';
import { Select } from '@/components/ui/select';
import { SkeletonRows } from '@/components/ui/skeleton';
import { ShowAll } from '@/components/app/Pagination';
import { Empty, ErrorNote, Panel, TableWrap } from '@/components/app/primitives';

/** 近几个月可选（本月 + 前 5 个月） */
const MONTHS_BACK = 6;
const NODE_CAP = 30;

/** 组织时区的本月 YYYY-MM（同服务端 org_today） */
export const orgMonth = () => new Date(Date.now() + meta().org_utc_offset_s * 1000).toISOString().slice(0, 7);
const monthOptions = (cur: string) => Array.from({ length: MONTHS_BACK }, (_, i) => {
  const [y, m] = cur.split('-').map(Number);
  const d = new Date(Date.UTC(y, m - 1 - i, 1));
  return { value: d.toISOString().slice(0, 7), label: `${d.getUTCFullYear()} 年 ${d.getUTCMonth() + 1} 月${i === 0 ? '（本月）' : ''}` };
});

/** 成功率：没有探测写「—」；100 才写 100%，不到 100 的最多写 99.99%（不让 99.996 四舍五入成「100.00%」冒充全程无失败），未满 99.9 标黄、未满 98 标红 */
function pctText(p: number | null): string {
  if (p == null) return '—';
  if (p >= 100) return '100%';
  return `${Math.min(p, 99.99).toFixed(2)}%`;
}
function Pct({ p, tip }: { p: number | null; tip?: string }) {
  return <span className="up-pct num" data-tone={uptimeTone(p)} data-tip={p == null ? '窗口内没有探测' : tip}>{pctText(p)}</span>;
}
const countTip = (ok: number, total: number) => `成功 ${ok.toLocaleString('zh-CN')} / 探测 ${total.toLocaleString('zh-CN')} 次`;

const TCP_TIP = '线路机到这台节点的 TCP 连接探测：成功次数 ÷ 探测次数，作参考\n个别供应商的节点 TCP 偶发失败而出口一直是通的，所以不作为可用率的主指标\n窗口内没有探测显示 —';
const EGRESS_TIP = '经入口走一遍节点出口的探测：成功次数 ÷ 探测次数，客户流量真正走的路径，可用率以它为准\n这台节点的全部入口（含已移除的）合计；窗口内没有探测显示 —';
const OUTAGE_TIP = '一个整点小时里出口探测失败 3 次以上算断线的一小时，连着的合成一次\n偶发的一两次失败不算；原始探测只留几天，月度窗口只能按小时算';
const INC_TIP = '选中的时间范围内这台机器上与可用性相关的告警次数：出口 / 线路机到节点不通、agent 离线、没有入口\n30 分钟内复发合并进原告警的也计入；续费、额度、流量异常不算';

export function UptimeTab({ month, onMonth }: { month: string; onMonth: (m: string) => void }) {
  const cur = React.useMemo(orgMonth, []);
  const opts = React.useMemo(() => monthOptions(cur), [cur]);
  const shown = opts.some((o) => o.value === month) ? month : cur;
  const q = useUptime(shown, shown === cur);
  const d = q.data;
  const [allNodes, setAllNodes] = React.useState(false);
  const picker = (
    <div className="w-[210px]"><Select value={shown} onChange={onMonth} options={opts} aria-label="月份" /></div>
  );
  const range = d && (
    <span className="t-sub up-range"><CalendarDays size={13} />{d.from.slice(5)} 至 {d.to.slice(5)} · {d.days} 天 · {meta().org_timezone_label}{shown === cur && '（本月算到今天）'}</span>
  );
  return (
    <>
      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
      <Panel title="按供应商" count={d ? `${d.providers.length} 家` : undefined} tools={<>{range}{picker}</>}
        sub={<InfoTip text={'可用率 = 出口探测的成功次数 ÷ 探测次数（TCP 作参考）；平均、最差只看窗口内有探测的节点，没有探测的显示 —；全部 100% 时不列最差的那台\n未满 99.9% 标黄，未满 98% 标红\n从差到好排；只列在用的节点（不含线路机和闲置备用）\n最差节点点开看它的连接质量'} />} flush>
        {q.isLoading ? <SkeletonRows rows={4} head /> : d && d.providers.length === 0 ? <Empty text="这个月没有在用的节点" /> : d && (
          <TableWrap>
            <table className="dt up-tbl" style={{ minWidth: 860 }}>
              {/* 两行表头：出口成功率、TCP 成功率各分「平均」「最差」两列；下表同名列写成一列（每台节点只有一个值） */}
              <thead>
                <tr className="up-grp">
                  <th rowSpan={2}>供应商</th><th className="r" rowSpan={2}>节点</th>
                  <th className="c" colSpan={2} data-tip={EGRESS_TIP}>出口成功率</th><th className="c" colSpan={2} data-tip={TCP_TIP}>TCP 成功率</th>
                  <th className="r" rowSpan={2} data-tip={OUTAGE_TIP}>断线次数</th><th className="r" rowSpan={2} data-tip={INC_TIP}>告警次数</th>
                </tr>
                <tr className="up-sub">
                  <th className="r" data-tip="这家供应商里有探测的节点的平均">平均</th><th className="r" data-tip="这家供应商的节点里，出口成功率最低的那台">最差</th>
                  <th className="r" data-tip="这家供应商里有 TCP 探测的节点的平均">平均</th><th className="r" data-tip="这家供应商的节点里，TCP 成功率最低的那台">最差</th>
                </tr>
              </thead>
              <tbody>{d.providers.map((p) => <ProviderRow key={p.id ?? 'none'} p={p} />)}</tbody>
            </table>
          </TableWrap>
        )}
      </Panel>

      <Panel title="按节点" count={d ? `${d.items.length} 台` : undefined} sub={<InfoTip text={`每台在用节点一行，出口成功率从差到好；没有探测的排在最后\n点一行打开这台的连接质量`} />} flush>
        {q.isLoading ? <SkeletonRows rows={6} head /> : d && d.items.length === 0 ? <Empty text="这个月没有在用的节点" /> : d && (
          <TableWrap>
            <table className="dt up-tbl" style={{ minWidth: 720 }}>
              <thead><tr>
                <th>节点</th><th className="r" data-tip={EGRESS_TIP}>出口成功率</th><th className="r" data-tip={TCP_TIP}>TCP 成功率</th>
                <th className="r" data-tip={OUTAGE_TIP}>断线次数</th><th className="r" data-tip={INC_TIP}>告警次数</th>
              </tr></thead>
              <tbody>{(allNodes ? d.items : d.items.slice(0, NODE_CAP)).map((i) => <NodeRow key={i.id} i={i} />)}</tbody>
            </table>
            <ShowAll total={d.items.length} cap={NODE_CAP} open={allNodes} onToggle={() => setAllNodes((v) => !v)} unit="台" />
          </TableWrap>
        )}
      </Panel>
    </>
  );
}

const open = (id: string) => openDrawer(id, 'quality');

function ProviderRow({ p }: { p: UptimeProvider }) {
  return (
    <tr>
      <td><span className="cell-main strong">{p.name}</span></td>
      <td className="r">{p.nodes} 台{p.with_data < p.nodes && <span className="sub" data-tip="窗口内有探测的节点才计入平均和最差">{p.with_data} 台有探测</span>}</td>
      <td className="r"><Pct p={p.avg_egress_pct} tip={`${p.with_data} 台有探测的节点的平均`} /></td>
      <td className="r"><Pct p={p.worst_egress_pct} />{p.worst && p.worst_egress_pct != null && p.worst_egress_pct < 100 && <button type="button" className="btn btn-link up-worst" onClick={() => open(p.worst!.id)} data-tip="打开这台的连接质量">{p.worst.name}</button>}</td>
      <td className="r"><Pct p={p.avg_tcp_pct} tip="有 TCP 探测的节点的平均" /></td>
      <td className="r"><Pct p={p.worst_tcp_pct} /></td>
      <td className="r">{p.outages > 0 ? <span className="num">{p.outages} 次</span> : <span className="t-muted">{p.with_data > 0 ? '0 次' : '—'}</span>}</td>
      <td className="r">{p.incidents > 0 ? <span className="num">{p.incidents} 次</span> : <span className="t-muted">0 次</span>}</td>
    </tr>
  );
}

function NodeRow({ i }: { i: UptimeItem }) {
  return (
    <tr className="is-link" tabIndex={0} onClick={() => open(i.id)} onKeyDown={(e) => { if (e.key === 'Enter') open(i.id); }}>
      <td><span className="cell-main strong">{i.name}</span><span className="sub">{[i.team?.name, i.provider?.name ?? '未填供应商'].filter(Boolean).join(' · ')}</span></td>
      <td className="r"><Pct p={i.egress_pct} tip={countTip(i.egress_ok, i.egress_total)} /></td>
      <td className="r"><Pct p={i.tcp_pct} tip={countTip(i.tcp_ok, i.tcp_total)} /></td>
      <td className="r">{i.outages > 0
        ? <><span className="num">{i.outages} 次</span><span className="sub" data-tip={'最长一次断线，按出口探测失败次数估算'}>最长{F.outageSpan(i.longest_outage_min)}</span></>
        : <span className="t-muted">{i.egress_total > 0 ? '0 次' : '—'}</span>}</td>
      <td className="r">{i.incidents > 0 ? <span className="num">{i.incidents} 次</span> : <span className="t-muted">0 次</span>}</td>
    </tr>
  );
}
