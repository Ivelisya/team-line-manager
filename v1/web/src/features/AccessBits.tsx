// 访问记录的小部件（数字格式、处理结果标、目标名）。单独成文件：访问监控页只要这几样，
// 不能连带把 AccessTab 顶层引入的 Recharts（约 110KB gzip）一起拉下来。
import { Chip, Pill } from '@/components/app/primitives';

export const fmtN = (n: number) => n.toLocaleString('zh-CN');

/** 处理结果：已转发 / 已阻止 */
export function ResultPill({ conns, blocked }: { conns: number; blocked: number }) {
  const routed = conns - blocked;
  if (blocked <= 0) return <Pill tone="ok">已转发 {fmtN(routed)}</Pill>;
  if (routed <= 0) return <Pill tone="bad">已阻止 {fmtN(blocked)}</Pill>;
  return <span className="row" style={{ gap: 4, flexWrap: 'nowrap' }}><Pill tone="ok">已转发 {fmtN(routed)}</Pill><Pill tone="bad">已阻止 {fmtN(blocked)}</Pill></span>;
}

/** 目标名：IP 目标带一个小标 */
export const HostCell = ({ host, kind }: { host: string; kind: 'domain' | 'ip' }) => (
  <span className="row" style={{ gap: 6, flexWrap: 'nowrap' }}><span className="ellipsis strong" data-tip={host}>{host}</span>{kind === 'ip' && <Chip variant="neutral">IP</Chip>}</span>
);
