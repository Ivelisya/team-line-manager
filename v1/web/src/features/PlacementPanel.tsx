// 放置矩阵（线路页顶部）：节点为行、在用线路机为列，每格是近 24 小时这台线路机到节点 IP 的往返中位数（成功率不到 100% 时带上）。
// 节点当前所在的那格标「当前」，建议换到的那格高亮。上面按目标线路机列出建议，「换线…」打开批量换线对话框（MovesDialog），
// 那里照常预览软路由、确认后才建作业；这里只读（GET /placement）。
import * as React from 'react';
import type { PlacementCell, PlacementNode, PlacementView } from '@/lib/types';
import { usePlacement } from '@/lib/queries';
import { openDrawer } from '@/lib/drawer';
import { cn } from '@/lib/utils';
import * as F from '@/lib/format';
import { Button } from '@/components/ui/button';
import { InfoTip } from '@/components/ui/form';
import { CopyButton, ErrorNote, FilterChip, Panel, TableWrap } from '@/components/app/primitives';
import { ShowAll } from '@/components/app/Pagination';
import { MovesDialog, type MoveNode } from '@/components/app/LifecycleDialogs';

/** 折叠时先露这么多行（建议换的一直露着） */
const CAP = 10;

type Group = PlacementView['suggestions'][number];

/** 命令行参数：只有字母数字（含中文）与 -_.:/@+=, 的原样，否则加单引号（同中控 api/incidents.rs 的 sh） */
const sh = (s: string) => (/^[\p{L}\p{N}\-_.:/@+=,]+$/u.test(s) ? s : `'${s.replace(/'/g, `'\\''`)}'`);

function Cell({ c, n, relay }: { c: PlacementCell; n: PlacementNode; relay: string }) {
  const cur = n.entry_relay_ids.includes(c.relay_id);
  const best = n.suggest && n.best_relay_id === c.relay_id;
  const weak = c.success_pct != null && c.success_pct < 99;
  const tip = c.samples === 0 ? `${relay} 还没测到这个节点` : [
    `${relay} → ${n.name}：近 24 小时测了 ${c.samples} 轮，通 ${c.ok} 轮`,
    cur && '节点现在挂在这台',
    best && `建议换到这台（${n.reason}）`,
    n.no_room.includes(c.relay_id) && '比最优还快，但它的流量额度放不下这个节点',
  ].filter(Boolean).join('\n');
  return (
    <td className={cn('r pm-cell', cur && 'is-current', best && 'is-best')} data-tip={tip}>
      {cur && <span className="pm-tag">当前</span>}
      {best && <span className="pm-tag">建议</span>}
      {c.samples === 0 ? <span className="t-muted">—</span> : c.p50_ms == null ? <span className="pm-bad">不通</span> : <>
        <span className="num">{F.ms(c.p50_ms).v}</span><span className="unit"> ms</span>
        {c.success_pct != null && c.success_pct < 100 && <span className={cn('pm-pct', weak && 'is-weak')}> · {c.success_pct}%</span>}
      </>}
    </td>
  );
}

/** 无 props，memo 住：父页面（线路页）订阅线路列表，约 3 秒一推，不必连带重画这张矩阵 */
export const PlacementPanel = React.memo(function PlacementPanel() {
  const q = usePlacement();
  const [team, setTeam] = React.useState<string | null>(null);
  const [all, setAll] = React.useState(false);
  const [moving, setMoving] = React.useState<Group | null>(null);
  const p = q.data;
  if (q.isError) return <ErrorNote error={q.error} onRetry={() => void q.refetch()}>放置矩阵取不到：</ErrorNote>;
  if (!p || p.nodes.length === 0 || p.relays.length < 2) return null;
  const teams = [...new Map(p.nodes.filter((n) => n.team).map((n) => [n.team!.id, n.team!])).values()];
  const nodes = p.nodes.filter((n) => !team || n.team?.id === team);
  const shown = new Set(nodes.map((n) => n.id));
  const sorted = [...nodes.filter((n) => n.suggest), ...nodes.filter((n) => !n.suggest)];
  const groups = p.suggestions.map((g) => ({ ...g, nodes: g.nodes.filter((n) => shown.has(n.id)) })).filter((g) => g.nodes.length > 0);
  const singles = p.single_suggestions.filter((s) => shown.has(s.node.id));
  const nSuggest = groups.reduce((s, g) => s + g.nodes.length, 0) + singles.length;
  const cap = Math.max(CAP, nSuggest);
  const rows = all ? sorted : sorted.slice(0, cap);
  const relayName = (id: string) => p.relays.find((r) => r.id === id)?.name ?? '';
  return (
    <Panel flush title="节点放置" count={nSuggest > 0 ? `${nSuggest} 个建议换线` : undefined}
      sub={<>近 {p.window_h} 小时线路机到节点的往返 <InfoTip text={`每 ${p.interval_s / 60} 分钟从每台线路机测一次到各节点 IP 的往返（ICMP），每格是中位数，成功率不到 100% 时写在后面\n最优：成功率 ≥ ${p.min_success_pct}%、没故障、流量额度放得下这个节点的线路机里最快的\n当前比最优慢 ${p.min_gain_ms} ms 以上，或当前成功率不到 ${p.min_success_pct}%，就建议换`} /></>}
      tools={teams.length > 1 && <>
        <FilterChip pressed={!team} onClick={() => setTeam(null)}>全部</FilterChip>
        {teams.map((t) => <FilterChip key={t.id} pressed={team === t.id} onClick={() => setTeam(t.id)}>{t.name}</FilterChip>)}
      </>}>
      {(groups.length > 0 || singles.length > 0) && (
        <div className="pm-sugs">
          {singles.map((s) => (
            <div key={`${s.node.id}-${s.from.id}`} className="pm-sug">
              <span className="pm-sug-text">换掉 <b>{s.from.name}</b> 上的入口 → <b>{s.to.name}</b>：<span className="pm-sug-node">{s.node.name}<span className="t-sub">（{s.reason}）</span></span></span>
              <CopyButton text={s.command} label="命令" />
            </div>
          ))}
          {groups.map((g) => {
            const cmd = `fleetctl move ${g.nodes.map((n) => sh(n.name)).join(',')} --to ${sh(g.to.name)}`;
            return (
              <div key={g.to.id} className="pm-sug">
                <span className="pm-sug-text">换到 <b>{g.to.name}</b>：{g.nodes.map((n) => <span key={n.id} className="pm-sug-node">{n.name}<span className="t-sub">（{n.reason}）</span></span>)}</span>
                <span className="row" style={{ flexWrap: 'nowrap' }}>
                  <CopyButton text={cmd} label="命令" />
                  <Button size="sm" onClick={() => setMoving(g)} data-tip="一起换，每台软路由只改一次、OpenClash 只重启一次；先预览再确认">换 {g.nodes.length} 个节点…</Button>
                </span>
              </div>
            );
          })}
        </div>
      )}
      <TableWrap>
        <table className="dt is-one pm-table" style={{ minWidth: 250 + p.relays.length * 170 }}>
          {/* 列宽写死：格子里最长的「建议 158 ms · 97.2%」放得下，窄屏横滑而不是被省略 */}
          <colgroup><col style={{ width: 130 }} />{p.relays.map((r) => <col key={r.id} style={{ width: 170 }} />)}<col style={{ width: 120 }} /></colgroup>
          <thead><tr>
            <th>节点</th>
            {p.relays.map((r) => <th key={r.id} className="r">{r.name}{r.status === 'down' && <span className="pm-down"> 故障</span>}</th>)}
            <th className="r"><span data-tip="近 7 天这个节点「线路机到节点不通」告警的次数（含恢复后又触发的）">TCP 不通告警</span></th>
          </tr></thead>
          <tbody>
            {rows.map((n) => (
              <tr key={n.id} className="is-link" tabIndex={0} onClick={() => openDrawer(n.id)} onKeyDown={(ev) => { if (ev.key === 'Enter') openDrawer(n.id); }}>
                <td className="strong"><span data-tip={n.current_relay_id ? undefined : n.entries > 1 ? '有多条在用入口，只给单条建议（上面带「换掉…上的入口」的）' : n.entries === 1 ? '还有待应用 / 停用的入口，不给建议' : '没有在用入口'}>{n.name}</span></td>
                {n.cells.map((c) => <Cell key={c.relay_id} c={c} n={n} relay={relayName(c.relay_id)} />)}
                <td className="r">{n.tcp_fail_7d > 0 ? <><span className="num">{n.tcp_fail_7d}</span><span className="unit"> 次</span></> : <span className="t-muted">—</span>}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </TableWrap>
      <ShowAll total={sorted.length} cap={cap} open={all} onToggle={() => setAll((v) => !v)} unit="个节点" />
      {moving && <MovesDialog nodes={moving.nodes.map((n): MoveNode => ({ id: n.id, name: n.name, port: n.port, from: n.from }))} open initialTo={moving.to.id}
        onOpenChange={(o) => { if (!o) setMoving(null); }} onDone={() => setMoving(null)} />}
    </Panel>
  );
});
