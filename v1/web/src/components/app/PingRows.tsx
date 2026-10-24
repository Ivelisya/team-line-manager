// 线路机卡片的三网 PING：每个目标一行「名称 · 当前延迟」，左边一串格子是延迟历史（按延迟分档上色），右边一串是丢包历史与当前丢包 %。
// 近 1 小时、3 分钟一格（同心跳格）；整块可点，打开抽屉的连接质量页签（那里有曲线）。全部卡片共用一个 /ping-beats 查询
import { memo } from 'react';
import { PING_LEGEND, pingLossTone, pingMsTone, usePingRows, type PingRow } from '@/lib/ping';
import type { DrawerTab } from '@/lib/drawer';
import * as F from '@/lib/format';

const ms = (v: number | null) => (v == null ? '—' : F.str(F.ms(v)));
const pct = (v: number | null) => (v == null ? '—' : `${Math.round(v)}%`);

function Row({ r }: { r: PingRow }) {
  const tip = (i: number) => { const [m, l] = r.cells[i]; return m == null && l == null ? '没有数据' : `延迟 ${ms(m)} · 丢包 ${pct(l)}`; };
  const lossCur = r.cur_loss;
  return (
    <div className="nc-ping-row" data-tip={`${r.name}：${r.target}（${r.mode === 'icmp' ? 'ICMP' : 'TCP'}）`}>
      <span className="nc-ping-name ellipsis">{r.name}</span>
      <b className="num nc-ping-ms" data-t={pingMsTone(r.cur_ms) ?? undefined}>{lossCur === 100 ? '不通' : ms(r.cur_ms)}</b>
      <span className="nc-ping-cells">{r.cells.map((c, i) => <i key={i} data-t={pingMsTone(c[0]) ?? undefined} data-tip={tip(i)} />)}</span>
      <span className="nc-ping-cells is-loss">{r.cells.map((c, i) => <i key={i} data-t={pingLossTone(c[1]) ?? undefined} data-tip={tip(i)} />)}</span>
      <b className="num nc-ping-loss" data-t={pingLossTone(lossCur) ?? undefined}>{pct(lossCur)}</b>
    </div>
  );
}

export const PingRows = memo(function PingRows({ id, onOpen }: { id: string; onOpen: (tab?: DrawerTab) => void }) {
  const rows = usePingRows(id).data;
  if (!rows?.length) return null;
  return (
    <button type="button" className="nc-ping nc-click" onClick={() => onOpen('quality')}>
      <span className="nc-ping-head"><span>三网 PING</span><span className="beat-win" data-tip={PING_LEGEND}>延迟 · 丢包 · 近 1 小时</span></span>
      {rows.map((r) => <Row key={r.slot} r={r} />)}
      <span className="sr-only">（查看连接质量页签）</span>
    </button>
  );
});
