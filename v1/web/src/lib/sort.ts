import { useMemo, useRef } from 'react';
import type { EntryLifecycle, MachineView, Team } from './types';

/** 中文名称比较（编号按数值：团队1-2 排在 团队1-10 前面）；全站共用一个 */
export const collator = new Intl.Collator('zh-Hans-CN', { numeric: true });

/**
 * 默认排序：线路机在前 → 团队（sort_order，再名称）→ 编号升序。
 * 故障/离线的节点不下沉；其他排序是用户的显式选择。
 */
export function makeDefaultCompare(teams: Team[]) {
  const order = new Map(teams.map((t) => [t.id, t.sort_order]));
  return (a: MachineView, b: MachineView): number => {
    const ra = a.kind === 'relay' ? 0 : 1; const rb = b.kind === 'relay' ? 0 : 1;
    if (ra !== rb) return ra - rb;
    const oa = a.team ? (order.get(a.team.id) ?? 1e9) : 1e9 + 1;
    const ob = b.team ? (order.get(b.team.id) ?? 1e9) : 1e9 + 1;
    if (oa !== ob) return oa - ob;
    const tn = collator.compare(a.team?.name ?? '', b.team?.name ?? '');
    if (tn) return tn;
    const na = a.number ?? 1e9; const nb = b.number ?? 1e9;
    if (na !== nb) return na - nb;
    return collator.compare(a.name, b.name);
  };
}

/**
 * 显示顺序保持稳定：实时推送每 3 秒改一次速率、流量、健康，按这些排的列表会跟着乱跳。
 * key（排序 / 筛选 / 分组 / 页码）不变时沿用上次的顺序，新出现的排到最后、消失的去掉；key 一变才按 list 重排。
 * key 为 null（按静态字段排序，如默认顺序、名称）时直接用 list。
 */
export function useStableOrder<T extends { id: string }>(list: T[], key: string | null): T[] {
  const prev = useRef<{ key: string | null; ids: string[] } | null>(null);
  return useMemo(() => {
    const p = prev.current;
    let out = list;
    if (key != null && p?.key === key) {
      const byId = new Map(list.map((m) => [m.id, m]));
      const old = new Set(p.ids);
      out = p.ids.flatMap((id) => byId.get(id) ?? []).concat(list.filter((m) => !old.has(m.id)));
    }
    prev.current = { key, ids: out.map((m) => m.id) };
    return out;
  }, [list, key]);
}

export const HEALTH_LABEL: Record<string, string> = {
  ok: '正常', degraded: '降级', down: '故障', unknown: '未知', maintenance: '维护中', inactive: '未启用',
};
export const HEALTH_TONE: Record<string, 'ok' | 'warn' | 'bad' | 'info' | 'neutral'> = {
  ok: 'ok', degraded: 'warn', down: 'bad', unknown: 'neutral', maintenance: 'info', inactive: 'neutral',
};
export const LIFECYCLE_LABEL: Record<string, string> = {
  draft: '草稿', provisioning: '上架中', active: '运行中', retiring: '退役中', retired: '已退役', failed: '上架失败',
};
export const KIND_LABEL: Record<string, string> = { relay: '线路机', vps: '落地机', proxy: '家宽代理' };
// 入口状态：pending / removing 是等线路机应用
export const ENTRY_LABEL: Record<EntryLifecycle, string> = { pending: '应用中', active: '运行中', disabled: '已停用', removing: '移除中', removed: '已移除' };
