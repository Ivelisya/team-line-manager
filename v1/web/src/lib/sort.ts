import { useMemo, useRef } from 'react';
import type { Kind, MachineView, Team } from './types';

/** 中文名称比较（编号按数值：团队1-2 排在 团队1-10 前面）；全站共用一个 */
export const collator = new Intl.Collator('zh-Hans-CN', { numeric: true });

/**
 * 默认排序：线路机在前 → 团队（sort_order，再名称）→ 编号升序。全站唯一的默认顺序定义（服务端 ORDER BY 与它不完全一致，前端不依赖服务端顺序）。
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
    // 同一团队（绝大多数比较）不用再按团队名比：每条推送都整表重排，collator 比较是这里最贵的一步
    if (a.team?.id !== b.team?.id) { const tn = collator.compare(a.team?.name ?? '', b.team?.name ?? ''); if (tn) return tn; }
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

/** 类型在下拉、分段里的先后（同默认排序：线路机在前）；中文名在 /meta（derive.kindLabel） */
export const KIND_ORDER: Kind[] = ['relay', 'vps', 'proxy'];
/** 健康状态的色调（界面样式），键的先后也是筛选下拉里的先后；中文名在 /meta（derive.healthLabel） */
export const HEALTH_TONE: Record<string, 'ok' | 'warn' | 'bad' | 'info' | 'neutral'> = {
  ok: 'ok', degraded: 'warn', down: 'bad', unknown: 'neutral', maintenance: 'info', inactive: 'neutral',
};
