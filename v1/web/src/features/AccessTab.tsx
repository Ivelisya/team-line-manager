// 节点抽屉「访问记录」页签：常访问的目标（横条）、来源 top、每小时连接数（柱状）、最近记录。
// 线路机的抽屉汇总它名下所有节点。数据来自 GET /machines/{id}/access，30 秒轮询。
import * as React from 'react';
import { Radar } from 'lucide-react';
import { ACCESS_WINDOWS, useMachineAccess, type AccessEvent, type AccessWindow, type MachineAccess } from '@/lib/access';
import * as F from '@/lib/format';
import { Segmented } from '@/components/ui/overlays';
import { Button } from '@/components/ui/button';
import { ChartContainer, chartTooltip, Recharts, axisProps, type ChartConfig } from '@/components/ui/chart';
import { Empty, ErrorNote, Pill, Section, SectionHeader } from '@/components/app/primitives';
import { fmtN } from '@/features/AccessBits';
import { ConnCell, IspPlace, SourceProfile } from '@/features/IpIntelBits';
import type { ProfileWindow } from '@/lib/ipintel';

const { BarChart, Bar, XAxis, YAxis, CartesianGrid } = Recharts;

const cut = (s: string, n = 24) => (s.length > n ? `${s.slice(0, n - 1)}…` : s);
// 与最近记录里「已转发 / 已阻止」标签同色：绿 / 红
const BAR_CFG: ChartConfig = { routed: { label: '已转发', color: 'var(--ok)' }, blocked: { label: '已阻止', color: 'var(--bad)' } };
// 柱状图悬停时整列铺一层浅底（recharts 默认是 #ccc 实心块，暗色下刺眼；--hover 在浅色下太淡看不见）
const BAR_CURSOR = { fill: 'var(--line)' };
/** 提示框里的连接数带单位 */
const times = (_: string, v: number) => `${fmtN(v)} 次`;

/** 横向条形图：每行一个目标，已转发 + 已阻止叠在一起。limit：只画前几个（截取放在这里，轮询数据没变时 data 引用不变） */
function HostBars({ items, limit }: { items: MachineAccess['top_domains']; limit?: number }) {
  const data = React.useMemo(() => items.slice(0, limit).map((d) => ({ host: d.dest_host, routed: Math.max(0, d.conns - d.blocked), blocked: d.blocked })), [items, limit]);
  return (
    <ChartContainer config={BAR_CFG} height={Math.max(64, data.length * 26 + 8)}>
      <BarChart data={data} layout="vertical" margin={{ top: 0, right: 8, bottom: 0, left: 0 }} barCategoryGap={5}>
        <XAxis type="number" hide />
        <YAxis type="category" dataKey="host" width={176} tickFormatter={(v: string) => cut(v, 26)} interval={0} {...axisProps} />
        {chartTooltip(BAR_CFG, String, times, BAR_CURSOR)}
        <Bar dataKey="routed" stackId="a" fill="var(--color-routed)" isAnimationActive={false} />
        <Bar dataKey="blocked" stackId="a" fill="var(--color-blocked)" radius={[0, 3, 3, 0]} isAnimationActive={false} />
      </BarChart>
    </ChartContainer>
  );
}

const p2 = F.pad2;
/** 提示框标题：这一小时的时段，如 10-21 01:00–02:00 */
const hourSpan = (l: string | number) => { const d = new Date(Number(l)); const e = new Date(Number(l) + 3_600_000); return `${p2(d.getMonth() + 1)}-${p2(d.getDate())} ${p2(d.getHours())}:00–${p2(e.getHours())}:00`; };
/** 每小时连接数柱状图 */
function HourlyBars({ points, height = 160 }: { points: MachineAccess['hourly']; height?: number }) {
  const long = points.length > 30;
  const data = React.useMemo(() => points.map((p) => ({ t: new Date(p.hour).getTime(), routed: Math.max(0, p.conns - p.blocked), blocked: p.blocked })), [points]);
  const tick = (t: number) => { const d = new Date(t); return long ? `${p2(d.getMonth() + 1)}-${p2(d.getDate())}` : `${p2(d.getHours())}:00`; };
  return (
    <ChartContainer config={BAR_CFG} height={height}>
      <BarChart data={data} margin={{ top: 6, right: 8, bottom: 0, left: 0 }} barCategoryGap={long ? 0 : 2}>
        <CartesianGrid vertical={false} stroke="var(--chart-grid)" />
        <XAxis dataKey="t" tickFormatter={tick} minTickGap={long ? 40 : 28} {...axisProps} />
        <YAxis width={40} allowDecimals={false} {...axisProps} />
        {chartTooltip(BAR_CFG, hourSpan, times, BAR_CURSOR)}
        <Bar dataKey="routed" stackId="a" fill="var(--color-routed)" isAnimationActive={false} />
        <Bar dataKey="blocked" stackId="a" fill="var(--color-blocked)" isAnimationActive={false} />
      </BarChart>
    </ChartContainer>
  );
}

/** 紧凑的最近记录表（抽屉里用）。时间列放得下「10-21 01:02:33」，不出省略号 */
function RecentTable({ items }: { items: AccessEvent[] }) {
  return (
    <div className="table-wrap" style={{ border: '1px solid var(--line)', borderRadius: 'var(--r-float)' }}>
      <table className="dt is-one" style={{ minWidth: 520 }}>
        <colgroup><col style={{ width: 140 }} /><col style={{ width: 136 }} /><col /><col style={{ width: 76 }} /></colgroup>
        <thead><tr><th>时间</th><th>来源</th><th>目标</th><th>结果</th></tr></thead>
        <tbody>
          {items.map((e, i) => (
            <tr key={`${e.ts}-${i}`}>
              <td data-tip={F.dtFull(e.ts)}>{F.dt(e.ts)}:{p2(new Date(e.ts).getSeconds())}</td>
              <td>{e.src_ip ?? '—'}</td>
              <td data-tip={e.dest_host}><span className="strong">{e.dest_host}</span>{e.dest_port != null && <span className="t-sub">:{e.dest_port}</span>}</td>
              <td>{e.blocked ? <Pill tone="bad">已阻止</Pill> : <Pill tone="ok">已转发</Pill>}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

// 只收 id 和是否线路机：抽屉的机器详情几乎每条实时推送都在变，传整台机器的话这里（连同图表）每 3 秒重画一次
export const AccessTab = React.memo(function AccessTab({ id, relay }: { id: string; relay: boolean }) {
  const [w, setW] = React.useState<AccessWindow>('24h');
  const [all, setAll] = React.useState(false);
  const q = useMachineAccess(id, w);
  const d = q.data;
  const long = ACCESS_WINDOWS.find((x) => x.value === w)!.long;
  const head = (
    <SectionHeader title="常访问的目标" meta={relay ? `汇总名下所有节点 · 最近 ${long}` : `最近 ${long}`}>
      <Segmented sm label="时间范围" value={w} onChange={setW} items={ACCESS_WINDOWS.filter((x) => x.value !== '30d').map((x) => ({ value: x.value, label: x.label }))} />
    </SectionHeader>
  );
  // 出错时留着时间范围控件：可以重试，也可以换一段时间
  if (q.isError && !d) return <Section>{head}<ErrorNote error={q.error} onRetry={() => void q.refetch()} /></Section>;
  if (!d) return <div className="skel" style={{ height: 160 }} />;
  const total = d.top_domains.reduce((s, x) => s + x.conns, 0);
  const none = d.top_domains.length === 0 && d.recent.length === 0;
  // 换时间范围、新数据未到时沿用的旧数据调淡（extras.css [data-stale]）；段标题和时间范围控件不调淡
  const stale = q.isPlaceholderData;
  return (
    <>
      <Section>
        {head}
        <div data-stale={stale}>
          {none ? (
            <Empty icon={<Radar size={22} />} text={`最近 ${long}没有访问记录（需要线路机开启访问日志、且有真实客户端流量）`} />
          ) : d.top_domains.length === 0 ? (
            <div className="hint">这段时间没有汇总数据。汇总每 5 分钟更新一次，下方「最近记录」是实时的。</div>
          ) : (
            <>
              <HostBars items={d.top_domains} limit={all ? undefined : 10} />
              <div className="row row-between" style={{ marginTop: 6 }}>
                <span className="hint">共 {fmtN(total)} 次连接（前 {d.top_domains.length} 个目标）。绿色已转发，红色已阻止。</span>
                {d.top_domains.length > 10 && <Button size="sm" variant="quiet" onClick={() => setAll((v) => !v)}>{all ? '收起' : `展开全部 ${d.top_domains.length} 个`}</Button>}
              </div>
            </>
          )}
        </div>
      </Section>
      {!none && (
        <>
          <SourceProfile id={id} window={(w === '7d' ? '7d' : '24h') as ProfileWindow} relay={relay} />
          <Section>
            <SectionHeader title="每小时连接数" meta={w === '1h' ? '最近 24 小时' : `最近 ${long}`} />
            {/* 服务端按小时补零：全是 0 就是没有汇总，不画一张只有 0–4 刻度的空图 */}
            <div data-stale={stale}>{d.hourly.some((p) => p.conns > 0) ? <HourlyBars points={d.hourly} /> : <div className="hint">这段时间没有汇总数据</div>}</div>
          </Section>
          <Section>
            <SectionHeader title="来源地址" meta="按连接数" />
            <div data-stale={stale}>
              {d.top_sources.length ? (
                <div className="table-wrap" style={{ border: '1px solid var(--line)', borderRadius: 'var(--r-float)' }}>
                  <table className="dt is-one" style={{ minWidth: 520 }}>
                    <colgroup><col style={{ width: 150 }} /><col /><col style={{ width: 110 }} /><col style={{ width: 70 }} /><col style={{ width: 110 }} /></colgroup>
                    <thead><tr><th>来源 IP</th><th>运营商 · 城市</th><th>接入方式</th><th className="r">连接数</th><th>最近</th></tr></thead>
                    <tbody>
                      {d.top_sources.map((s) => (
                        <tr key={`${s.node_id}-${s.src_ip}`}>
                          <td>{s.src_ip}{relay && <span className="t-sub"> · {s.name}</span>}</td>
                          <td><IspPlace intel={s.intel} /></td><td><ConnCell intel={s.intel} /></td>
                          <td className="r">{fmtN(s.conns)}</td><td>{F.dt(s.last_ts)}</td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              ) : <div className="hint">没有来源记录</div>}
            </div>
          </Section>
          <Section>
            <SectionHeader title="最近记录" meta="最近 20 条 · 原始记录保留 7 天" />
            <div data-stale={stale}>{d.recent.length ? <RecentTable items={d.recent} /> : <div className="hint">最近 7 天没有原始记录</div>}</div>
          </Section>
        </>
      )}
    </>
  );
});
