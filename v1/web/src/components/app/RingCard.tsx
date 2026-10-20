// 环形视图的卡片：名称 + 状态，CPU / 内存 / 磁盘三个圆环（点击打开抽屉“主机”页签），上行 / 下行实时速率。
// 高度固定（无主机也画三个空环），整页网格不需要逐槽对齐。
import { memo } from 'react';
import { ArrowDown, ArrowUp } from 'lucide-react';
import type { MachineView, Plan } from '@/lib/types';
import type { DrawerTab } from '@/lib/drawer';
import * as F from '@/lib/format';
import { isQuiet, resourceMeters } from '@/lib/derive';
import { cn } from '@/lib/utils';
import { ContextMenu, ContextMenuContent, ContextMenuTrigger } from '@/components/ui/context-menu';
import { HealthPill, KindIcon, Num } from './primitives';
import { NodeContextItems } from './NodeMenu';
import { StatusDot } from './NodeCard';
import { Ring } from './viz';

function RingCardImpl({ m, plans, current, onOpen }: { m: MachineView; plans: Plan[]; current?: boolean; onOpen: (id: string, tab?: DrawerTab) => void }) {
  const r = resourceMeters(m, plans);
  const quiet = isQuiet(m);
  const h = m.host;
  const ring = (metric: 'cpu' | 'mem' | 'disk', label: string, v: { value: F.Fmt; pct: number | null } | undefined) => (
    <div className="rc-gauge">
      <Ring metric={metric} pct={quiet ? null : v?.pct ?? null}><b className="num">{v && !quiet ? v.value.v : '—'}</b>{v && !quiet && v.value.u && <small>{v.value.u}</small>}</Ring>
      <span>{label}</span>
    </div>
  );
  const rate = (dir: 'up' | 'down', bps: number | null | undefined) => (
    <span className="rc-rate" data-speed={quiet ? undefined : F.speedOf(bps)}>{dir === 'up' ? <><ArrowUp /><small>上行</small></> : <><ArrowDown /><small>下行</small></>}<Num f={F.rate(bps)} /></span>
  );
  return (
    <ContextMenu>
      <ContextMenuTrigger asChild>
        <article className={cn('rcard', current && 'is-current', quiet && 'is-quiet')} data-health={m.health.status} data-id={m.id}>
          <header className="rc-head">
            <StatusDot m={m} /><KindIcon kind={m.kind} />
            <button type="button" className="rc-name" onClick={() => onOpen(m.id)}>{m.name}</button>
            <HealthPill m={m} />
          </header>
          <button type="button" className="rc-rings" onClick={() => onOpen(m.id, 'host')} aria-label={`${m.name}：查看主机曲线`} disabled={!h}>
            {ring('cpu', 'CPU', r?.cpu)}{ring('mem', '内存', r?.mem)}{ring('disk', '磁盘', r?.disk)}
          </button>
          <footer className="rc-foot">
            {h ? <>{rate('up', h.net_tx_bps)}{rate('down', h.net_rx_bps)}</> : <span className="t-muted">{m.kind === 'proxy' ? '家宽代理没有主机指标' : '暂无主机指标'}</span>}
          </footer>
        </article>
      </ContextMenuTrigger>
      <ContextMenuContent><NodeContextItems m={m} /></ContextMenuContent>
    </ContextMenu>
  );
}
export const RingCard = memo(RingCardImpl);
