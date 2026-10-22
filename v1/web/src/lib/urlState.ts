// 写在地址参数里的页面状态（筛选、排序、页签）：按补丁改写参数、?sort= 的解析与写回，各页面共用一份
import * as React from 'react';
import { useSearchParams } from 'react-router-dom';
import type { SortState } from '@/components/app/primitives';

/** ?sort= 的原值（列 id，「-」开头为降序）→ 排序；空 = 默认顺序 */
export const parseSort = (raw: string | null | undefined): SortState | null => (raw ? { id: raw.replace(/^-/, ''), desc: raw.startsWith('-') } : null);
/** 排序 → ?sort= 的值；默认顺序为空串（upd 会删掉这个参数） */
export const sortParam = (s: SortState | null) => (s ? `${s.desc ? '-' : ''}${s.id}` : '');

/** 当前地址参数与按补丁改写它的 upd（值为空串的删掉，替换当前历史记录，不新增一步后退）。
 *  setSearchParams 每次地址变化都换身份；用 ref 兜住，upd 身份稳定（memo 的下拉、分段控件不被击穿，同 lib/drawer.ts） */
export function useUrlState() {
  const [sp, setSp] = useSearchParams();
  const setRef = React.useRef(setSp); setRef.current = setSp;
  const upd = React.useCallback((patch: Record<string, string>) => setRef.current((p) => {
    const n = new URLSearchParams(p);
    for (const [k, v] of Object.entries(patch)) { if (v) n.set(k, v); else n.delete(k); }
    return n;
  }, { replace: true }), []);
  return [sp, upd] as const;
}

/** 表头排序记在 ?sort=：返回当前排序与写回它的函数 */
export function useUrlSort() {
  const [sp, upd] = useUrlState();
  const raw = sp.get('sort');
  const sort = React.useMemo(() => parseSort(raw), [raw]);
  const setSort = React.useCallback((s: SortState | null) => upd({ sort: sortParam(s) }), [upd]);
  return [sort, setSort] as const;
}
