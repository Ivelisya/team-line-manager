// 通用展示原件：每种只有一份实现（状态 pill、KPI 磚、标签/值、段标题、面板、页头…）。
import * as React from 'react';
import { Link } from 'react-router-dom';
import { Home, Inbox, Server, Waypoints, Check } from 'lucide-react';
import { cn } from '@/lib/utils';
import type { Fmt } from '@/lib/format';
import type { EntryView, HealthStatus, Kind, Lifecycle, MachineView } from '@/lib/types';
import { HEALTH_LABEL, HEALTH_TONE, KIND_LABEL, LIFECYCLE_LABEL } from '@/lib/sort';
import { Tip } from '@/components/ui/overlays';
import { toast } from '@/components/ui/toast';

/** 数值 + 灰色小单位 */
export function Num({ f, className }: { f: Fmt; className?: string }) {
  return <span className={cn('num', className)}>{f.v}{f.u && <> <span className="unit">{f.u}</span></>}</span>;
}

export type Tone = 'ok' | 'warn' | 'bad' | 'info' | 'neutral';

/** 状态 pill（唯一实现）：健康、生命周期、作业状态都走它 */
export function Pill({ tone = 'neutral', dot, children, title, className }: { tone?: Tone; dot?: 'ring' | 'pulse' | 'none'; children: React.ReactNode; title?: string; className?: string }) {
  return <span className={cn('pill', className)} data-tone={tone} data-dot={dot} title={title}>{children}</span>;
}
export function HealthPill({ m }: { m: Pick<MachineView, 'health' | 'lifecycle' | 'maintenance'> }) {
  const lc = m.lifecycle;
  if (lc === 'provisioning') return <Pill tone="info" dot="pulse">上架中</Pill>;
  if (lc === 'retiring') return <Pill tone="info" dot="pulse">下线中</Pill>;
  if (lc === 'retired') return <Pill tone="neutral" dot="ring">已退役</Pill>;
  if (lc === 'failed') return <Pill tone="bad">上架失败</Pill>;
  const s = m.health.status;
  const reason = m.health.reasons[0]?.text;
  return <Pill tone={HEALTH_TONE[s]} dot={s === 'unknown' || s === 'inactive' ? 'ring' : undefined} title={reason}>{HEALTH_LABEL[s]}</Pill>;
}
export const healthTone = (s: HealthStatus) => HEALTH_TONE[s];
export const LifecyclePill = ({ lc }: { lc: Lifecycle | string }) => (
  <Pill tone={lc === 'active' ? 'ok' : lc === 'provisioning' || lc === 'retiring' ? 'info' : lc === 'failed' ? 'bad' : 'neutral'} dot={lc === 'retired' ? 'ring' : undefined}>{LIFECYCLE_LABEL[lc] ?? lc}</Pill>
);

export const entryState = (e: EntryView) => (e.lifecycle === 'disabled' ? <Pill tone="neutral" dot="ring">已停用</Pill> : e.lifecycle === 'active' ? (e.applied ? <Pill tone="ok">运行中</Pill> : <Pill tone="info" dot="pulse">下发中</Pill>) : <Pill tone="info" dot="pulse">{LIFECYCLE_LABEL[e.lifecycle] ?? e.lifecycle}</Pill>);

export function Chip({ children, variant, title, className }: { children: React.ReactNode; variant?: 'price' | 'neutral' | 'warn'; title?: string; className?: string }) {
  return <span className={cn('chip', variant === 'price' && 'chip-price', variant === 'neutral' && 'chip-neutral', variant === 'warn' && 'chip-warn', className)} title={title}>{children}</span>;
}
export const Mark = ({ children, title }: { children: React.ReactNode; title?: string }) => <span className="mark" title={title}>{children}</span>;

/** 类型标（原版国旗位）：线路机 / 落地机 / 家宽代理 */
export function KindIcon({ kind }: { kind: Kind }) {
  const Ic = kind === 'relay' ? Waypoints : kind === 'proxy' ? Home : Server;
  return <span className="nc-kind" title={KIND_LABEL[kind]} aria-label={KIND_LABEL[kind]}><Ic /></span>;
}

/** 筛选 chip */
export function FilterChip({ pressed, onClick, children, count }: { pressed: boolean; onClick: () => void; children: React.ReactNode; count?: number }) {
  return (
    <button type="button" className={cn('fchip', count === 0 && !pressed && 'is-zero')} aria-pressed={pressed} onClick={onClick}>
      {children}{count != null && <span className="fchip-count">{count}</span>}
    </button>
  );
}

/** KPI 磚：18 标签 / 32 主值 / 18 副行，三行固定，所有磚等高 */
export function KpiTile({ label, value, unit, sub, to, onClick, pressed, loading }: {
  label: string; value: React.ReactNode; unit?: string; sub?: React.ReactNode; to?: string; onClick?: () => void; pressed?: boolean; loading?: boolean;
}) {
  const inner = (
    <>
      <div className="kpi-label">{label}</div>
      <div className="kpi-value">{loading ? <span className="skel" style={{ width: 72, height: 24 }} /> : <>{value}{unit && <span className="unit">{unit}</span>}</>}</div>
      <div className="kpi-sub">{loading ? '' : sub ?? ' '}</div>
    </>
  );
  if (to) return <Link className="kpi" to={to}>{inner}</Link>;
  if (onClick) return <button type="button" className="kpi" onClick={onClick} aria-pressed={pressed}>{inner}</button>;
  return <div className="kpi">{inner}</div>;
}
export const DotCount = ({ tone, n, label }: { tone: Tone; n: number; label: string }) => (
  <span className="dotline"><span className="dot" data-tone={tone} />{label} <b className="num" style={{ fontWeight: 600, color: 'var(--fg)' }}>{n}</b></span>
);

/** 标签/值（唯一实现） */
export function LabelValue({ label, children, wrap, className }: { label: string; children: React.ReactNode; wrap?: boolean; className?: string }) {
  return <div className={cn('lv', className)}><dt>{label}</dt><dd className={cn(wrap && 'wrap')}>{children ?? '—'}</dd></div>;
}
export const LabelValueGrid = ({ children, cols2 }: { children: React.ReactNode; cols2?: boolean }) => <dl className={cn('lv-grid', cols2 && 'cols-2')}>{children}</dl>;

/** 段标题（唯一实现）：抽屉与面板内的小节标题 */
export const SectionHeader = ({ title, meta, children }: { title: React.ReactNode; meta?: React.ReactNode; children?: React.ReactNode }) => (
  <div className="sec-head"><span>{title}{meta && <span className="sec-meta"> {meta}</span>}</span>{children}</div>
);
export const Section = ({ children, className }: { children: React.ReactNode; className?: string }) => <section className={cn('sec', className)}>{children}</section>;

export function Panel({ title, count, sub, tools, children, flush, className }: { title?: React.ReactNode; count?: React.ReactNode; sub?: React.ReactNode; tools?: React.ReactNode; children?: React.ReactNode; flush?: boolean; className?: string }) {
  return (
    <section className={cn('panel', className)}>
      {(title || tools) && (
        <header className="panel-head">
          {title && <h2 className="panel-title">{title}</h2>}
          {count != null && <span className="panel-count">{count}</span>}
          {sub && <span className="panel-sub ellipsis">{sub}</span>}
          <div className="panel-tools">{tools}</div>
        </header>
      )}
      {children != null && <div className={cn('panel-body', flush && 'is-flush')}>{children}</div>}
    </section>
  );
}

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

export function Empty({ icon, text, children }: { icon?: React.ReactNode; text: string; children?: React.ReactNode }) {
  return <div className="empty">{icon ?? <Inbox size={28} />}<div>{text}</div>{children}</div>;
}
export const Note = ({ tone = 'info', icon, children }: { tone?: 'info' | 'ok' | 'warn' | 'bad'; icon?: React.ReactNode; children: React.ReactNode }) => (
  <div className="note" data-tone={tone}>{icon}<div style={{ minWidth: 0 }}>{children}</div></div>
);

/** 点击复制的文本 */
export function Copyable({ text, className }: { text: string; className?: string }) {
  const [done, setDone] = React.useState(false);
  return (
    <Tip text={done ? '已复制' : '点击复制'}>
      <button type="button" className={cn('ellipsis', className)} style={{ background: 'none', border: 0, padding: 0, cursor: 'copy', color: 'inherit', textAlign: 'left', display: 'inline-flex', alignItems: 'center', gap: 4, font: 'inherit' }}
        onClick={async () => { try { await navigator.clipboard.writeText(text); setDone(true); setTimeout(() => setDone(false), 1200); } catch { toast.err('复制失败，请手动选择文本'); } }}>
        <span className="ellipsis">{text}</span>{done && <Check size={12} />}
      </button>
    </Tip>
  );
}

/** 进度线（唯一实现）：0–100，tone 随阈值变 */
export function Meter({ icon, label, value, pct, sub, empty }: { icon?: React.ReactNode; label: string; value: Fmt; pct: number | null; sub?: React.ReactNode; empty?: boolean }) {
  const p = pct == null ? 0 : Math.max(0, Math.min(100, pct));
  const tone = p >= 90 ? 'bad' : p >= 75 ? 'warn' : undefined;
  return (
    <div className={cn('meter', empty && 'is-empty')} data-tone={tone}>
      <div className="meter-head"><span className="meter-label">{icon}{label}</span><span className="meter-value"><Num f={value} /></span></div>
      <div className="meter-track"><div className="meter-fill" style={{ width: `${p}%` }} /></div>
      {sub !== undefined && <div className="meter-sub">{sub}</div>}
    </div>
  );
}
export const Quota = ({ pct }: { pct: number | null }) => {
  const p = pct == null ? 0 : Math.max(0, Math.min(100, pct));
  return <div className="quota" data-tone={p >= 95 ? 'bad' : p >= 80 ? 'warn' : undefined}><div className="quota-fill" style={{ width: `${p}%` }} /></div>;
};

/** 心跳条：n 格，ok/warn/bad/空 */
export function Beat({ cells, caption, win, stale, n = 20 }: { cells: ('ok' | 'warn' | 'bad' | null)[]; caption: React.ReactNode; win?: string; stale?: boolean; n?: number }) {
  const pad = Array.from({ length: Math.max(0, n - cells.length) }, () => null as null);
  return (
    <div className={cn('beat', stale && 'is-stale')}>
      <div className="beat-cap"><span className="beat-main ellipsis">{caption}</span>{win && <span className="beat-win">{win}</span>}</div>
      <div className="beat-cells">{[...pad, ...cells.slice(-n)].map((c, i) => <i key={i} data-t={c ?? undefined} />)}</div>
    </div>
  );
}
