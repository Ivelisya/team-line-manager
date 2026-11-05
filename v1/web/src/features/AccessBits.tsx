// 访问记录的小部件（数字格式、处理结果标、目标名）。单独成文件：访问监控页只要这几样，
// 不能连带把 AccessTab 顶层引入的图表库（bklit，约 60–90KB gzip）一起拉下来。
import { Chip, Pill } from '@/components/app/primitives';
import '@/styles/access.css';

export const fmtN = (n: number) => n.toLocaleString('zh-CN');

/** 处理结果：只标不正常的——全部转发的写「—」（同全站空值），全部阻止写「已阻止」（次数就是旁边的连接数），有转发也有阻止时才拆开写各多少 */
export function ResultPill({ conns, blocked }: { conns: number; blocked: number }) {
  const routed = conns - blocked;
  if (blocked <= 0) return <span className="t-muted">—</span>;
  if (routed <= 0) return <Pill tone="bad">已阻止</Pill>;
  return <span className="row" style={{ gap: 4 }}><Pill tone="ok">已转发 {fmtN(routed)}</Pill><Pill tone="bad">已阻止 {fmtN(blocked)}</Pill></span>;
}

/** 目标名：IP 目标带一个小标。onPick：可点（访问监控页点了把搜索设成它） */
export const HostCell = ({ host, kind, onPick }: { host: string; kind: 'domain' | 'ip'; onPick?: (host: string) => void }) => (
  <span className="row" style={{ gap: 6, flexWrap: 'nowrap' }}>
    {onPick
      ? <button type="button" className="host-pick ellipsis strong" data-tip={`${host}
点击只看它`} onClick={(e) => { e.stopPropagation(); onPick(host); }}>{host}</button>
      : <span className="ellipsis strong" data-tip={host}>{host}</span>}
    {kind === 'ip' && <Chip variant="neutral">IP</Chip>}
  </span>
);
