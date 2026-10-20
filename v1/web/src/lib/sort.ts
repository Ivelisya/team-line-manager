import type { MachineView, Team } from './types';

/**
 * 默认排序：线路机在前 → 团队（sort_order，再名称）→ 编号升序。
 * 故障/离线的节点不下沉；其他排序是用户的显式选择。
 */
export function makeDefaultCompare(teams: Team[]) {
  const order = new Map(teams.map((t) => [t.id, t.sort_order]));
  const zh = new Intl.Collator('zh-Hans-CN', { numeric: true });
  return (a: MachineView, b: MachineView): number => {
    const ra = a.kind === 'relay' ? 0 : 1; const rb = b.kind === 'relay' ? 0 : 1;
    if (ra !== rb) return ra - rb;
    const oa = a.team ? (order.get(a.team.id) ?? 1e9) : 1e9 + 1;
    const ob = b.team ? (order.get(b.team.id) ?? 1e9) : 1e9 + 1;
    if (oa !== ob) return oa - ob;
    const tn = zh.compare(a.team?.name ?? '', b.team?.name ?? '');
    if (tn) return tn;
    const na = a.number ?? 1e9; const nb = b.number ?? 1e9;
    if (na !== nb) return na - nb;
    return zh.compare(a.name, b.name);
  };
}

export const HEALTH_LABEL: Record<string, string> = {
  ok: '正常', degraded: '降级', down: '故障', unknown: '未知', maintenance: '维护中', inactive: '未启用',
};
export const HEALTH_TONE: Record<string, 'ok' | 'warn' | 'bad' | 'info' | 'neutral'> = {
  ok: 'ok', degraded: 'warn', down: 'bad', unknown: 'neutral', maintenance: 'info', inactive: 'neutral',
};
export const LIFECYCLE_LABEL: Record<string, string> = {
  draft: '草稿', provisioning: '上架中', active: '运行中', retiring: '下线中', retired: '已退役', failed: '上架失败',
};
export const KIND_LABEL: Record<string, string> = { relay: '线路机', vps: '落地机', proxy: '家宽代理' };
