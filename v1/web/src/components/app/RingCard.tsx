// 环形视图的卡片：名称，CPU / 内存 / 磁盘三个圆环（点击打开抽屉“主机”页签，悬停看具体用量），底部状态胶囊（同卡片视图的位置）+ 上行 / 下行实时速率。
// 家宽代理没有主机：圆环的位置换成本月业务流量与探测延迟两行（点击打开流量页签）。同机关系标在名称后面，卡窄只写「同机」，宿主机名在悬停说明里。
import { memo } from 'react';
import { ArrowDown, ArrowUp, Globe, Radar } from 'lucide-react';
import type { MachineView } from '@/lib/types';
import type { DrawerTab } from '@/lib/drawer';
import type { CardShow } from './NodeCard';
import * as F from '@/lib/format';
import { isQuiet, liveRate, monthTraffic, probeSummary, resourceMeters, trafficTotal } from '@/lib/derive';
import { cn } from '@/lib/utils';
import { Due, Flag, HealthPill, HostTag, Num } from './primitives';
import { Ring } from './viz';

type Meter = { value: F.Fmt; pct: number | null; sub?: string };
/** 圆环的悬停：内存、磁盘写用量（2.44 / 4 GiB），CPU 写核数，负载照抄格条的副行 */
const ringTip = (label: string, v: Meter | undefined, quiet: boolean) => (!v || quiet ? undefined : `${label} ${F.str(v.value)}${v.sub && v.sub !== '—' ? ` · ${v.sub}` : ''}`);

function RingCardImpl({ m, current, onOpen, show }: { m: MachineView; current?: boolean; onOpen: (id: string, tab?: DrawerTab) => void; show?: CardShow }) {
  const r = resourceMeters(m);
  const quiet = isQuiet(m);
  const proxy = m.kind === 'proxy';
  const lr = liveRate(m);
  const ring = (metric: 'cpu' | 'mem' | 'disk', label: string, v: Meter | undefined) => (
    <div className="rc-gauge" data-tip={ringTip(label, v, quiet)}>
      <Ring metric={metric} pct={quiet ? null : v?.pct ?? null}><b className="num">{v && !quiet ? v.value.v : '—'}</b>{v && !quiet && v.value.u && <small>{v.value.u}</small>}</Ring>
      <span>{label}</span>
    </div>
  );
  const rate = (dir: 'up' | 'down', bps: number | null | undefined) => (
    <span className="rc-rate" data-speed={lr ? F.speedOf(bps) : undefined}>{dir === 'up' ? <><ArrowUp /><span className="sr-only">上行</span></> : <><ArrowDown /><span className="sr-only">下行</span></>}<Num f={F.rate(bps)} /></span>
  );
  const t = proxy ? monthTraffic(m) : null; const ps = proxy ? probeSummary(m) : null;
  const ms = (v: number | null, fail: boolean) => (fail ? <span style={{ color: 'var(--bad-text)' }}>失败</span> : <Num f={F.ms(v)} />);
  // 按到期、本月流量排序时：排序依据接在名称这一行最右（环形卡上没有这两个字段）
  const extra = show === 'due' ? (m.subscription ? <Due days={m.subscription.due_days} state={m.subscription.due_state} tail /> : <span className="t-muted">没有到期日</span>)
    : show === 'traffic' && !proxy ? <span className="rc-show">本月 <Num f={F.bytes(trafficTotal(m))} /></span> : null;
  // 右键菜单由外面的 NodeMenus 整个网格共用一份（按 data-id 找到这张卡）
  return (
    <article className={cn('rcard', current && 'is-current', quiet && 'is-quiet')} data-health={m.health.status} data-id={m.id}>
      <header className="rc-head">
        <Flag country={m.country} manual={!!m.country_set} ip={m.ip} />
        <button type="button" className="nc-name" onClick={() => onOpen(m.id)}>{m.name}</button>
        <HostTag id={m.id} host={m.host_machine} nodes={m.ip_nodes} short />
        {extra && <span className="rc-extra">{extra}</span>}
      </header>
      {proxy ? (
        <button type="button" className="rc-rings rc-pair" onClick={() => onOpen(m.id, 'traffic')}>
          <span><Globe />本月业务<b>↓ <Num f={F.bytes(t!.down)} /> · ↑ <Num f={F.bytes(t!.up)} /></b></span>
          <span><Radar />TCP · 出口<b>{ms(ps!.tcp, ps!.tcpFail)} · {ps!.egressFail ? <span style={{ color: 'var(--bad-text)' }}>失败</span> : ps!.egressOk ? '正常' : '—'}</b></span>
          <span className="sr-only">（查看流量页签）</span>
        </button>
      ) : (
        <button type="button" className="rc-rings" onClick={() => onOpen(m.id, 'host')} disabled={!m.host}>
          {ring('cpu', 'CPU', r?.cpu)}{ring('mem', '内存', r?.mem)}{ring('disk', '磁盘', r?.disk)}<span className="sr-only">（查看主机曲线）</span>
        </button>
      )}
      <footer className="rc-foot">
        <HealthPill m={m} />
        {!proxy && <span className="rc-rates">{rate('up', lr?.tx)}{rate('down', lr?.rx)}</span>}
      </footer>
    </article>
  );
}
export const RingCard = memo(RingCardImpl);
