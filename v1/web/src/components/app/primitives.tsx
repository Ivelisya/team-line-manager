// 通用展示原件：每种只有一份实现（状态 pill / 圆点、KPI 卡、到期读数、表头排序、标签/值、段标题、面板、页头…）。
import * as React from 'react';
import { Link } from 'react-router-dom';
import { ArrowDownUp, Check, ChevronDown, ChevronRight, ChevronUp, CircleAlert, Copy, Globe, Inbox, RotateCw } from 'lucide-react';
import { cn } from '@/lib/utils';
import { dueFmt, type Fmt } from '@/lib/format';
import type { DueState, EntryView, HealthReason, Lifecycle, MachineView } from '@/lib/types';
import { HEALTH_TONE } from '@/lib/sort';
import { countryName, flagUrl } from '@/lib/flags';
import { Tip, Trunc } from '@/components/ui/overlays';
import { useCountUp } from '@/lib/useCountUp';
import { dueTone, entryLabel, healthLabel, hostRel, lifecycleLabel, severityOf, type BeatCell } from '@/lib/derive';
import { useDrawer } from '@/lib/drawer';
import { copyText } from '@/lib/clipboard';

/** 数值 + 灰色小单位 */
export function Num({ f }: { f: Fmt }) {
  return <span className="num">{f.v}{f.u && <> <span className="unit">{f.u}</span></>}</span>;
}

export type Tone = 'ok' | 'warn' | 'bad' | 'info' | 'neutral';

// 下面几个原件的 tip：悬停 / 键盘聚焦时的说明，写成 data-tip 交给全站一份 TipHost 显示（外观同其它提示框，不用浏览器自带的 title）

/** 状态 pill（唯一实现）：健康、生命周期、作业状态都走它 */
export const Pill = React.forwardRef<HTMLSpanElement, { tone?: Tone; dot?: 'pulse' | 'none'; children: React.ReactNode; tip?: string; className?: string } & React.HTMLAttributes<HTMLSpanElement>>(
  ({ tone = 'neutral', dot, children, tip, className, ...p }, ref) => (
    <span ref={ref} className={cn('pill', className)} data-tone={tone} data-dot={dot} data-tip={tip} {...p}>{children}</span>
  ),
);
Pill.displayName = 'Pill';
export function HealthPill({ m }: { m: Pick<MachineView, 'health' | 'lifecycle' | 'maintenance'> }) {
  const lc = m.lifecycle;
  if (lc === 'provisioning') return <Pill tone="info" dot="pulse">{lifecycleLabel(lc)}</Pill>;
  if (lc === 'retiring') return <Pill tone="info" dot="pulse">{lifecycleLabel(lc)}</Pill>;
  if (lc === 'retired') return <Pill tone="neutral">{lifecycleLabel(lc)}</Pill>;
  if (lc === 'failed') return <Pill tone="bad">{lifecycleLabel(lc)}</Pill>;
  const s = m.health.status;
  const pill = <Pill tone={HEALTH_TONE[s]} tabIndex={m.health.reasons.length ? 0 : undefined}>{healthLabel(s)}</Pill>;
  return <Tip text={reasonsTip(m.health.reasons)}>{pill}</Tip>;
}
/** 健康原因列表（带严重度圆点）：健康 pill 的悬停提示与抽屉“状态原因”共用 */
export const ReasonList = ({ reasons, className }: { reasons: HealthReason[]; className: string }) => (
  <ul className={className}>
    {reasons.map((r, i) => <li key={i}><SeverityMark severity={r.severity} />{r.text}</li>)}
  </ul>
);
const reasonsTip = (reasons: HealthReason[]) => (reasons.length ? <ReasonList reasons={reasons} className="tip-list" /> : null);
/** 名字后面的状态标（卡片头、表格名称列）：正常不标；故障、降级、维护中、未知各一个小图标，上架中 / 退役中转圈，上架失败同故障。
 *  颜色之外给读屏与悬停一份文字（不用小圆点：用户嫌不好看） */
export const StatusMark = ({ status, lifecycle }: { status: string; lifecycle: Lifecycle }) => {
  const busy = lifecycle === 'provisioning' || lifecycle === 'retiring';
  const tone = lifecycle === 'failed' ? 'bad' : busy ? 'info' : HEALTH_TONE[status];
  if (tone === 'ok') return null;
  const text = busy || lifecycle === 'failed' ? lifecycleLabel(lifecycle) : healthLabel(status);
  return <span className="smark" data-tone={tone} data-spin={busy ? '' : undefined} role="img" aria-label={text} data-tip={text} />;
};
/** 告警级别圆点（抽屉「最近告警」时间线的节点）：颜色之外给读屏与悬停一份文字 */
export const SeverityDot = ({ severity }: { severity: string }) => {
  const s = severityOf(severity);
  return <span className="dot" data-tone={s.tone} role="img" aria-label={s.label} data-tip={s.label} />;
};
/** 告警级别图标（顶栏告警预览的列表）：严重同故障、警告同降级 */
export const SeverityMark = ({ severity }: { severity: string }) => {
  const s = severityOf(severity);
  return <span className="smark" data-tone={s.tone} data-sev={s.tone === 'info' ? 'info' : undefined} role="img" aria-label={s.label} data-tip={s.label} />;
};
/** 到期读数：N 天后 / 今天 / 已过期 N 天，颜色按服务端的 due_state：即将到期黄、已过期红（卡片、表格、悬浮卡共用）。
 *  tail：没有“到期”标签的地方接上“到期”（3 天后到期 / 今天到期 / 已过期 3 天） */
export function Due({ days, state, tail }: { days: number | null | undefined; state: DueState | null | undefined; tail?: boolean }) {
  const d = dueFmt(days, dueTone(state));
  const post = d.post + (tail && days != null && !d.past ? '到期' : '');
  return <span className="due" data-tone={d.tone || undefined}>{d.pre && <span className="due-t">{d.pre}</span>}{d.f && <Num f={d.f} />}{post && <span className="due-t">{post}</span>}</span>;
}
export const LifecyclePill = ({ lc }: { lc: Lifecycle | string }) => (
  <Pill tone={lc === 'active' ? 'ok' : lc === 'provisioning' || lc === 'retiring' ? 'info' : lc === 'failed' ? 'bad' : 'neutral'}>{lifecycleLabel(lc)}</Pill>
);

// 入口状态：active 按是否已应用细分
export const entryState = (e: EntryView) => (e.lifecycle === 'disabled' ? <Pill tone="neutral">{entryLabel('disabled')}</Pill> : e.lifecycle === 'active' ? (e.applied ? <Pill tone="ok">{entryLabel('active')}</Pill> : e.apply_error ? <Pill tone="bad" tip={e.apply_error}>应用失败</Pill> : <Pill tone="info" dot="pulse">{entryLabel('pending')}</Pill>) : <Pill tone="info" dot="pulse">{entryLabel(e.lifecycle)}</Pill>);

export function Chip({ children, variant, tip, className }: { children: React.ReactNode; variant?: 'neutral' | 'warn'; tip?: string; className?: string }) {
  return <span className={cn('chip', variant === 'neutral' && 'chip-neutral', variant === 'warn' && 'chip-warn', className)} data-tip={tip}>{children}</span>;
}
export const Mark = ({ children, tip }: { children: React.ReactNode; tip?: string }) => <span className="mark" data-tip={tip}>{children}</span>;

/** 同机关系标（卡片、环形卡、机器表共用）：同机 IP 节点「同机 · 宿主机名」、宿主机「+N IP」，点击打开对应机器的抽屉——
 *  宿主机上有几个同机节点时打开宿主机自己（概览里逐个列着）。在表格行里点它不触发行的点击。没有同机关系的机器不渲染（也不订阅地址） */
type HostTagProps = { id: string; host: MachineView['host_machine']; nodes: MachineView['ip_nodes']; short?: boolean; tabIndex?: number };
export const HostTag = (p: HostTagProps) => (p.host || p.nodes.length ? <HostTagButton {...p} /> : null);
function HostTagButton({ id, host, nodes, short, tabIndex }: HostTagProps) {
  const { open } = useDrawer();
  const r = hostRel(host, nodes, short)!;
  const to = host ? host.id : nodes.length === 1 ? nodes[0].id : id;
  const act = host ? '点击打开宿主机' : nodes.length === 1 ? '点击打开它' : '点击在这台机器的详情里查看';
  return <button type="button" className="tag" data-c="violet" data-tip={`${r.tip}。${act}`} tabIndex={tabIndex} onClick={(e) => { e.stopPropagation(); open(to); }}>{r.text}</button>;
}

/** 名字前的国旗（原版国旗位）：机器自己 IP 所在的国家（中控离线 IP 库按 IP 查）；查不到（内网地址、家宽代理填的是域名、库没装）
 *  是一个中性的小地球——类型（线路机 / 家宽代理）在标签行里，这里不再放类型图标。悬停：中文国名（手动设的标注）· IP */
export function Flag({ country, manual, ip }: { country?: string | null; manual?: boolean; ip?: string }) {
  const url = country ? flagUrl(country) : undefined;
  const name = country ? countryName(country) : '国家未知';
  const tip = [manual ? `${name}（手动设置）` : name, ip].filter(Boolean).join(' · ');
  return url
    ? <img className="nc-flag" src={url} alt={name} data-tip={tip} />
    : <span className="nc-flag is-none" role="img" aria-label={name} data-tip={tip}><Globe /></span>;
}

/** 筛选 chip */
export function FilterChip({ pressed, onClick, children, count }: { pressed: boolean; onClick: () => void; children: React.ReactNode; count?: number }) {
  return (
    <button type="button" className={cn('fchip', count === 0 && !pressed && 'is-zero')} aria-pressed={pressed} onClick={onClick}>
      {children}{count != null && <span className="fchip-count">{count}</span>}
    </button>
  );
}

type StatTone = 'ok' | 'teal' | 'bad' | 'amber' | 'gold' | 'sky' | 'violet' | 'blue';
/** KPI 卡（唯一实现）：彩色图标 + 标签，大号读数 + 小单位，一行灰色说明；三行固定，同一排等高。可点的（to）标题行最右多一个小箭头 */
export function Stat({ icon, tone, label, value, unit, caption, to, loading }: { icon: React.ReactNode; tone: StatTone; label: string; value: React.ReactNode; unit?: string; caption?: React.ReactNode; to?: string; loading?: boolean }) {
  const inner = (
    <>
      <span className="stat-label"><span className="stat-ic" data-tone={tone}>{icon}</span>{label}{to && <ChevronRight className="stat-go" aria-hidden />}</span>
      <span className="stat-value">{loading ? <span className="skel" style={{ width: 64, height: 20 }} /> : <>{value}{unit && <small>{unit}</small>}</>}</span>
      <span className="stat-cap">{loading ? '' : caption ?? ' '}</span>
    </>
  );
  return to ? <Link className="stat" to={to}>{inner}</Link> : <div className="stat">{inner}</div>;
}
/** 数字平滑过渡：值变了从旧值滚到新值。format 决定怎么显示（默认取整） */
export function CountUp({ value, format }: { value: number | null | undefined; format?: (n: number) => string }) {
  const v = useCountUp(value ?? NaN); // 没有值时是 NaN：数据到达的那一刻直接显示目标值，不从 0 滚过去
  if (value == null) return <>—</>;
  return <>{format ? format(v) : Math.round(v)}</>;
}
/** 说明行里的分项计数（降级 4 · 故障 2，状态名同全站）：不配小圆点，有数时数字按级别着色 */
export const DotCount = ({ tone, n, label }: { tone: Tone; n: number; label: string }) => (
  <span className="dotline" data-tone={n > 0 ? tone : undefined}>{label} <b className="num">{n}</b></span>
);

/** 表头排序：首次点击按 first 的方向（数值列多为先降序，名称、到期先升序），再点反向，第三次回到默认顺序（null） */
export interface SortState { id: string; desc: boolean }
const nextSort = (s: SortState | null, id: string, first: 'asc' | 'desc'): SortState | null =>
  s?.id !== id ? { id, desc: first === 'desc' } : s.desc === (first === 'desc') ? { id, desc: !s.desc } : null;
/** 表头一行（唯一实现）：first 为空的列不可排序 */
export function SortHead({ cols, sort, onSort }: { cols: { id: string; header: string; r?: boolean; first?: 'asc' | 'desc' }[]; sort: SortState | null; onSort: (s: SortState | null) => void }) {
  return (
    <thead><tr>{cols.map((c) => {
      const dir = sort?.id === c.id ? (sort.desc ? 'desc' : 'asc') : undefined;
      return (
        <th key={c.id} className={c.r ? 'r' : undefined} aria-sort={dir === 'asc' ? 'ascending' : dir === 'desc' ? 'descending' : undefined}>
          {c.first ? <button type="button" className="th-sort" data-dir={dir} onClick={() => onSort(nextSort(sort, c.id, c.first!))}>{c.header}{dir === 'asc' ? <ChevronUp /> : dir === 'desc' ? <ChevronDown /> : <ArrowDownUp />}</button> : c.header}
        </th>
      );
    })}</tr></thead>
  );
}

/** 标签/值（唯一实现）：值单行显示；字符串值放不下时省略、悬停看全文（IPv6、公网地址），wrap 的折行显示 */
export function LabelValue({ label, children, wrap, className }: { label: string; children: React.ReactNode; wrap?: boolean; className?: string }) {
  const v = children ?? '—';
  return <div className={cn('lv', className)}><dt>{label}</dt><dd className={cn(wrap && 'wrap')}>{typeof v === 'string' && !wrap ? <Trunc>{v}</Trunc> : v}</dd></div>;
}
export const LabelValueGrid = ({ children, cols2 }: { children: React.ReactNode; cols2?: boolean }) => <dl className={cn('lv-grid', cols2 && 'cols-2')}>{children}</dl>;

/** 段标题（唯一实现）：抽屉与面板内的小节标题 */
export const SectionHeader = ({ title, meta, children }: { title: React.ReactNode; meta?: React.ReactNode; children?: React.ReactNode }) => (
  <div className="sec-head"><span>{title}{meta && <span className="sec-meta"> {meta}</span>}</span>{children}</div>
);
export const Section = ({ children, className }: { children: React.ReactNode; className?: string }) => <section className={cn('sec', className)}>{children}</section>;

/** ref 落在 section 上（分页器换页后滚回面板顶部用） */
export const Panel = React.forwardRef<HTMLElement, { title?: React.ReactNode; count?: React.ReactNode; sub?: React.ReactNode; tools?: React.ReactNode; children?: React.ReactNode; flush?: boolean; className?: string }>(function Panel({ title, count, sub, tools, children, flush, className }, ref) {
  return (
    <section ref={ref} className={cn('panel', className)}>
      {(title || tools) && (
        <header className="panel-head">
          {title && <h2 className="panel-title">{title}</h2>}
          {count != null && <span className="panel-count">{count}</span>}
          {sub && <span className={cn('panel-sub', typeof sub === 'string' && 'ellipsis')}>{sub}</span>}
          <div className="panel-tools">{tools}</div>
        </header>
      )}
      {children != null && <div className={cn('panel-body', flush && 'is-flush')}>{children}</div>}
    </section>
  );
});

/** 页头：全站同一个 */
export function PageHead({ title, note, tools }: { title: string; note?: React.ReactNode; tools?: React.ReactNode }) {
  return (
    <header className="page-head">
      <h1 className="page-title">{title}</h1>
      {note && <span className="page-note">{note}</span>}
      <div className="page-tools">{tools}</div>
    </header>
  );
}

/** 空状态：图标放在柔和的圆底里，一句话说明现状；children 放下一步操作 */
export function Empty({ icon, text, hint, children }: { icon?: React.ReactNode; text: string; hint?: React.ReactNode; children?: React.ReactNode }) {
  return <div className="empty"><span className="empty-ic">{icon ?? <Inbox size={22} />}</span><div className="empty-text">{text}</div>{hint && <div className="hint">{hint}</div>}{children}</div>;
}
export const Note = ({ tone = 'info', icon, children }: { tone?: 'info' | 'ok' | 'warn' | 'bad'; icon?: React.ReactNode; children: React.ReactNode }) => (
  <div className="note" data-tone={tone}>{icon}<div style={{ flex: 1, minWidth: 0 }}>{children}</div></div>
);

/** 查询出错的提示条：错误信息 + 可选的重试 */
export function ErrorNote({ error, onRetry, children }: { error: unknown; onRetry?: () => void; children?: React.ReactNode }) {
  return (
    <Note tone="bad" icon={<CircleAlert size={14} />}>
      <div className="row" style={{ gap: 8, flexWrap: 'nowrap', alignItems: 'baseline' }}>
        <span style={{ minWidth: 0, overflowWrap: 'anywhere' }}>{children}{error instanceof Error ? error.message : String(error)}</span>
        {onRetry && <button type="button" className="btn btn-link btn-sm" style={{ flex: 'none', height: 'auto' }} onClick={onRetry}><RotateCw size={12} />重试</button>}
      </div>
    </Note>
  );
}

/** 复制按钮：点击复制 text，图标短暂变成对勾作为反馈 */
export function CopyButton({ text, label = '内容', className, size = 14 }: { text: string; label?: string; className?: string; size?: number }) {
  const [done, setDone] = React.useState(false);
  React.useEffect(() => { if (!done) return; const h = setTimeout(() => setDone(false), 1400); return () => clearTimeout(h); }, [done]);
  return (
    <Tip text={done ? '已复制' : `复制${label}`}>
      <button type="button" className={cn('copy-btn', done && 'is-done', className)} aria-label={`复制${label}`}
        onClick={async (e) => { e.stopPropagation(); if (await copyText(text, label)) setDone(true); }}>
        {done ? <Check size={size} /> : <Copy size={size} />}
      </button>
    </Tip>
  );
}

/** 心跳条：n 格（默认 20 格 = 卡片上的近 1 小时），ok/warn/bad/空；titles 与 cells 一一对应，作每格的悬停说明 */
export function Beat({ cells, titles, caption, win, stale, n = 20 }: { cells: BeatCell[]; titles?: string[]; caption: React.ReactNode; win?: string; stale?: boolean; n?: number }) {
  const pad = Math.max(0, n - cells.length);
  const shown = cells.slice(-n); const tips = titles?.slice(-n);
  return (
    <div className={cn('beat', stale && 'is-stale')}>
      <div className="beat-cap"><span className="beat-main ellipsis">{caption}</span>{win && <span className="beat-win">{win}</span>}</div>
      <div className="beat-cells">{Array.from({ length: pad + shown.length }, (_, i) => i < pad ? <i key={i} /> : <i key={i} data-t={shown[i - pad] ?? undefined} data-tip={tips?.[i - pad]} />)}</div>
    </div>
  );
}
