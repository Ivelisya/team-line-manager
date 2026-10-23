// 软路由抽屉的「曲线」页签：资源、温度、WAN 与代理速率、连接与客户端、可用出口，以及各出口的延迟。
// 单独成文件按需加载（recharts）；图表卡、加载框、时间范围与机器抽屉共用（DrawerCharts）。
import * as React from 'react';
import { Activity, Cable, Cpu, Network, Radar, Shuffle, Thermometer, Users } from 'lucide-react';
import { beatBins, exitBeats, routerMeters, useRouterExitMetrics, useRouterMetrics, type RouterView } from '@/lib/routers';
import { RANGE_MS, vMs, vPct, vRate, yMs, yPct, yRate } from '@/lib/chart';
import { useDrawer } from '@/lib/drawer';
import * as F from '@/lib/format';
import { InfoTip } from '@/components/ui/form';
import { Select } from '@/components/ui/select';
import { Skeleton } from '@/components/ui/skeleton';
import { Beat, Empty, ErrorNote, Num, Section, SectionHeader } from '@/components/app/primitives';
import { TimeChart } from '@/components/app/TimeChart';
import { ChartCard, ChartFrame, RangeSticky } from './DrawerCharts';

// 线条与格式化放模块常量：抽屉随推送重渲染时 TimeChart（memo）不重画。资源用首页的指标色，上下行 --chart-1 / --chart-2
const L_CPU = [{ key: 'cpu_pct', label: 'CPU', color: 'var(--m-cpu)' }];
const L_RES = [{ key: 'mem_pct', label: '内存', color: 'var(--m-mem)' }, { key: 'disk_pct', label: '存储', color: 'var(--m-disk)' }, { key: 'conntrack_pct', label: '连接表', color: 'var(--m-load)' }];
const L_TEMP = [{ key: 'temp_c', label: '温度', color: 'var(--m-load)' }];
const L_WAN = [{ key: 'wan_rx_bps', label: '↓ 下行', color: 'var(--chart-1)' }, { key: 'wan_tx_bps', label: '↑ 上行', color: 'var(--chart-2)' }];
const L_PROXY = [{ key: 'proxy_down_bps', label: '↓ 到客户端', color: 'var(--chart-1)' }, { key: 'proxy_up_bps', label: '↑ 发往出口', color: 'var(--chart-2)' }];
const L_CONNS = [{ key: 'conns', label: '代理连接', color: 'var(--chart-1)' }];
const L_PEOPLE = [{ key: 'clients', label: '在线客户端', color: 'var(--chart-1)' }, { key: 'exits_alive', label: '可用出口', color: 'var(--chart-2)' }];
const L_DELAY = [{ key: 'delay_ms', label: '延迟', color: 'var(--chart-1)' }];
const PCT: [number, number] = [0, 100];
const yInt = (v: number) => String(Math.round(v));
const vInt = (_: string, v: number) => String(Math.round(v));
const yTemp = (v: number) => `${Math.round(v)}°`;
const vTemp = (_: string, v: number) => `${v.toFixed(1)} °C`;

/** 点不到两个时的说明：所选范围内才接入的是数据还在积累，其余（离线）是这段时间没有数据 */
const fewText = (r: RouterView, range: string) => (Date.now() - Date.parse(r.created_at) < (RANGE_MS[range] ?? 0) ? '刚接入，数据还在积累' : '这段时间没有数据');

/** 还没接入的路由没有任何读数：一个空状态，不画一排空图 */
export function RouterChartsTab({ r }: { r: RouterView }) {
  return r.state === 'pending' ? <Empty icon={<Activity size={22} />} text="还没接入，没有曲线" /> : <Charts r={r} />;
}

function Charts({ r }: { r: RouterView }) {
  const { range, setRange } = useDrawer(); // 同机器抽屉：?range=，默认 24 小时
  const q = useRouterMetrics(r.id, range);
  const few = fewText(r, range);
  const s = r.state === 'online' ? r.summary : null;
  const mt = s ? routerMeters(s) : null;
  const pts = q.data?.points;
  const hasTemp = React.useMemo(() => pts?.some((p) => typeof p.temp_c === 'number'), [pts]);
  const chart = (lines: typeof L_CPU, yFmt: (v: number) => string, valueFmt: (k: string, v: number) => string, o: { domain?: [number, number]; yBase?: 1000; kind?: 'line' } = {}) => (
    <ChartFrame q={q} few={few} height={140}><TimeChart points={pts} range={range} lines={lines} yFmt={yFmt} valueFmt={valueFmt} height={140} {...o} /></ChartFrame>
  );
  return (
    <>
      {/* 同机器抽屉的曲线页签：顶部吸顶的「时间范围」条 */}
      <RangeSticky range={range} setRange={setRange} />
      <Section className="chart-stack">
        <ChartCard c="var(--m-cpu)" icon={<Cpu />} title="CPU" value={s ? <Num f={F.pct(s.cpu_pct, 1)} /> : '—'} caption="使用率">{chart(L_CPU, yPct, vPct, { domain: PCT })}</ChartCard>
        <ChartCard c="var(--m-mem)" icon={<Activity />} title="内存 · 存储 · 连接表" value={mt ? <>内存 <Num f={mt.mem.value} /> <span className="t-sub">·</span> 存储 <Num f={mt.disk.value} /> <span className="t-sub">·</span> 连接表 <Num f={mt.ct.value} /></> : '—'} caption="占总量的百分比">{chart(L_RES, yPct, vPct, { domain: PCT, kind: 'line' })}</ChartCard>
        {hasTemp && <ChartCard c="var(--m-load)" icon={<Thermometer />} title="温度" value={s?.temp_c != null ? <Num f={{ v: s.temp_c.toFixed(0), u: '°C' }} /> : '—'} caption="当前">{chart(L_TEMP, yTemp, vTemp, { kind: 'line' })}</ChartCard>}
        <ChartCard c="var(--chart-1)" icon={<Network />} title="WAN 速率" value={s ? <>↓ <Num f={F.rate(s.wan.rx_bps)} /> <span className="t-sub">·</span> ↑ <Num f={F.rate(s.wan.tx_bps)} /></> : '—'} caption="当前">{chart(L_WAN, yRate, vRate, { yBase: 1000 })}</ChartCard>
        <ChartCard c="var(--chart-1)" icon={<Shuffle />} title="OpenClash 代理速率" value={s ? <>↓ <Num f={F.rate(s.openclash.down_bps)} /> <span className="t-sub">·</span> ↑ <Num f={F.rate(s.openclash.up_bps)} /></> : '—'} caption="当前">{chart(L_PROXY, yRate, vRate, { yBase: 1000 })}</ChartCard>
        <ChartCard c="var(--chart-1)" icon={<Cable />} title="代理连接数" value={s ? s.openclash.conns.toLocaleString() : '—'} caption="当前">{chart(L_CONNS, yInt, vInt, { kind: 'line' })}</ChartCard>
        <ChartCard c="var(--chart-1)" icon={<Users />} title="客户端与可用出口" value={s ? `${s.clients} 台 · ${s.exits.alive}/${s.exits.total}` : '—'} caption="当前">{chart(L_PEOPLE, yInt, vInt, { kind: 'line' })}</ChartCard>
      </Section>
      <ExitDelays r={r} range={range} few={few} />
    </>
  );
}

/** 各出口的延迟测试：每个出口一条格子（一眼看出哪个、什么时候不通；格子按所选范围从现在往前等分），下面选一个看延迟曲线 */
function ExitDelays({ r, range, few }: { r: RouterView; range: string; few: string }) {
  const q = useRouterExitMetrics(r.id, range);
  const exits = q.data?.exits;
  const [pick, setPick] = React.useState('');
  const cur = exits?.find((x) => x.name === pick) ?? exits?.[0];
  const opts = React.useMemo(() => (exits ?? []).map((x) => ({ value: x.name, label: x.name })), [exits]);
  const bins = beatBins(range);
  // 所选范围比数据长（新接入的路由）：格子从有数据的那刻起切，和上面的曲线横轴一致，左边写「数据从 … 起」
  const since = React.useMemo(() => (exits ?? []).reduce((m, x) => x.points.reduce((m2, p) => Math.min(m2, Date.parse(p.ts)), m), Infinity), [exits]);
  const late = Number.isFinite(since) && since > Date.now() - (RANGE_MS[range] ?? 0) + (RANGE_MS[range] ?? 0) / bins;
  const beats = React.useMemo(() => (exits ?? []).map((x) => ({ name: x.name, ...exitBeats(x.points, range, bins, Date.now(), late ? since : undefined) })), [exits, range, bins, late, since]);
  return (
    <Section>
      <SectionHeader meta={late ? `数据从 ${F.dt(new Date(since).toISOString())} 起` : undefined} title={<>各出口延迟<InfoTip text={'每格是一段时间：全部成功是绿，有失败是黄，失败过半是红，斜纹是没有数据\n右边是整段时间的成功率\n延迟高低看下面的曲线，曲线断开表示那次没测通'} /></>} />
      {q.isError && !q.data ? <ErrorNote error={q.error} onRetry={() => void q.refetch()}>出口曲线加载失败：</ErrorNote>
        : q.isLoading ? <Skeleton h={180} className="is-float" />
        : !exits?.length ? <div className="chart-empty" style={{ height: 120 }}>{few}</div> : (
          <div className="chart-frame stack-sm" data-stale={q.isPlaceholderData}>
            <div className="rt-exit-beats">
              {beats.map((b) => <Beat key={b.name} n={bins} cells={b.cells} titles={b.titles} caption={<b>{b.name}</b>} win={b.rate != null ? `${Math.round(b.rate * 100)}%` : undefined} winTone={b.rate == null ? undefined : b.rate >= 1 ? 'ok' : b.rate < 0.5 ? 'bad' : 'warn'} />)}
            </div>
            <ChartCard c="var(--chart-1)" icon={<Radar />} title="延迟曲线" tools={<Select sm value={cur?.name ?? ''} onChange={setPick} options={opts} aria-label="出口" className="w-[160px]" />}>
              {cur && cur.points.length > 1 ? <TimeChart points={cur.points} range={range} kind="line" lines={L_DELAY} yFmt={yMs} valueFmt={vMs} height={160} /> : <div className="chart-empty" style={{ height: 160 }}>{few}</div>}
            </ChartCard>
          </div>
        )}
    </Section>
  );
}
