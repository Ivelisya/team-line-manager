// 环形视图的卡片：名称 + 状态，CPU / 内存 / 磁盘三个圆环（点击打开抽屉“主机”页签），上行 / 下行实时速率。
// 高度固定（无主机也画三个空环），整页网格不需要逐槽对齐。
import { memo } from 'react';
import { ArrowDown, ArrowUp } from 'lucide-react';
import type { MachineView } from '@/lib/types';
import type { DrawerTab } from '@/lib/drawer';
import * as F from '@/lib/format';
import { isQuiet, liveRate, resourceMeters } from '@/lib/derive';
import { cn } from '@/lib/utils';
import { HealthPill, KindIcon, Num, StatusDot } from './primitives';
import { Ring } from './viz';

function RingCardImpl({ m, current, onOpen }: { m: MachineView; current?: boolean; onOpen: (id: string, tab?: DrawerTab) => void }) {
  const r = resourceMeters(m);
  const quiet = isQuiet(m);
  const h = m.host; const lr = liveRate(m);
  const ring = (metric: 'cpu' | 'mem' | 'disk', label: string, v: { value: F.Fmt; pct: number | null } | undefined) => (
    <div className="rc-gauge">
      <Ring metric={metric} pct={quiet ? null : v?.pct ?? null}><b className="num">{v && !quiet ? v.value.v : '—'}</b>{v && !quiet && v.value.u && <small>{v.value.u}</small>}</Ring>
      <span>{label}</span>
    </div>
  );
  const rate = (dir: 'up' | 'down', bps: number | null | undefined) => (
    <span className="rc-rate" data-speed={lr ? F.speedOf(bps) : undefined}>{dir === 'up' ? <><ArrowUp /><small>上行</small></> : <><ArrowDown /><small>下行</small></>}<Num f={F.rate(bps)} /></span>
  );
  // 右键菜单由外面的 NodeMenus 整个网格共用一份（按 data-id 找到这张卡）
  return (
    <article className={cn('rcard', current && 'is-current', quiet && 'is-quiet')} data-health={m.health.status} data-id={m.id}>
      <header className="rc-head">
        <StatusDot status={m.health.status} lifecycle={m.lifecycle} /><KindIcon kind={m.kind} />
        <button type="button" className="nc-name" onClick={() => onOpen(m.id)}>{m.name}</button>
        <HealthPill m={m} />
      </header>
      <button type="button" className="rc-rings" onClick={() => onOpen(m.id, 'host')} disabled={!h}>
        {ring('cpu', 'CPU', r?.cpu)}{ring('mem', '内存', r?.mem)}{ring('disk', '磁盘', r?.disk)}<span className="sr-only">（查看主机曲线）</span>
      </button>
      <footer className="rc-foot">
        {h ? <>{rate('up', lr?.tx)}{rate('down', lr?.rx)}</> : <span className="t-sub">{m.kind === 'proxy' ? '家宽代理没有主机指标' : '暂无主机指标'}</span>}
      </footer>
    </article>
  );
}
export const RingCard = memo(RingCardImpl);
