// 机器详情抽屉的开关由 URL 控制：?m=<机器id>&tab=<页签>。任何页面都可以打开它，后退键关闭。
import { useCallback, useRef } from 'react';
import { useSearchParams, type SetURLSearchParams } from 'react-router-dom';
import type { QueryClient } from '@tanstack/react-query';
import { readMachines } from './queries';

export type DrawerTab = 'overview' | 'quality' | 'traffic' | 'host' | 'billing' | 'access' | 'manage';
export const DRAWER_TABS: { value: DrawerTab; label: string }[] = [
  { value: 'overview', label: '概览' }, { value: 'quality', label: '连接质量' }, { value: 'traffic', label: '流量' },
  { value: 'host', label: '主机' }, { value: 'billing', label: '费用' }, { value: 'access', label: '访问记录' }, { value: 'manage', label: '管理' },
];

// 全站只有一个路由：任意一处 useDrawer 渲染时记下地址参数的写函数，模块级的 openDrawer 用它（同机关系标等成批的小按钮不必各自订阅地址）
let setLatest: { current: SetURLSearchParams } | null = null;
const openWith = (set: SetURLSearchParams, mid: string, t?: DrawerTab) =>
  set((p) => { const n = new URLSearchParams(p); n.set('m', mid); if (t) n.set('tab', t); else n.delete('tab'); return n; });
/** 打开抽屉（不订阅地址）：外壳里一直挂着 useDrawer，调用时总有写函数 */
export const openDrawer = (mid: string, t?: DrawerTab) => { if (setLatest) openWith(setLatest.current, mid, t); };

export function useDrawer() {
  const [sp, setSp] = useSearchParams();
  const id = sp.get('m');
  const rawTab = sp.get('tab');
  const tab: DrawerTab = DRAWER_TABS.some((t) => t.value === rawTab) ? (rawTab as DrawerTab) : 'overview';
  // react-router 的 setSearchParams 每次地址变化都换身份；用 ref 兜住，open/setTab/close 才稳定（卡片 memo 依赖它）
  const setRef = useRef(setSp); setRef.current = setSp; setLatest = setRef;
  const open = useCallback((mid: string, t?: DrawerTab) => openWith(setRef.current, mid, t), []);
  // 抽屉开着、只是换一台（[ ] 键、抽屉头部的上一台 / 下一台）：留在当前页签，替换当前历史，后退键仍是一步关掉抽屉
  const switchTo = useCallback((mid: string) => setRef.current((p) => { const n = new URLSearchParams(p); n.set('m', mid); return n; }, { replace: true }), []);
  const setTab = useCallback((t: DrawerTab) => setRef.current((p) => { const n = new URLSearchParams(p); n.set('tab', t); return n; }, { replace: true }), []);
  const close = useCallback(() => setRef.current((p) => { const n = new URLSearchParams(p); n.delete('m'); n.delete('tab'); return n; }), []);
  return { id, tab, open, switchTo, setTab, close };
}

// 上一台 / 下一台的顺序来源：列着机器的页面（首页、机器页）把当前筛选、排序、分组后的整份顺序（不只当前页）交过来，离开页面时清掉
let order: readonly string[] | null = null;
export const setDrawerOrder = (ids: readonly string[] | null) => { order = ids; };

/** 抽屉里的上一台 / 下一台（[ ] 键与抽屉头部的按钮共用）：按页面交来的顺序（setDrawerOrder）；没有、或这台不在其中时按机器列表缓存的默认顺序。
 *  到头为 null，不绕回。不订阅列表（订阅会让每个页面都跟着取 /machines），用时读缓存；列表没取过、或这台不在在用列表里（已退役）时返回 undefined */
export function drawerSiblings(qc: QueryClient, id: string | null): { prev: string | null; next: string | null } | undefined {
  if (!id) return undefined;
  const at = (n: number, i: number, idAt: (k: number) => string) => (i < 0 ? undefined : { prev: i > 0 ? idAt(i - 1) : null, next: i < n - 1 ? idAt(i + 1) : null });
  const ids = order; const oi = ids ? ids.indexOf(id) : -1;
  if (ids && oi >= 0) return at(ids.length, oi, (k) => ids[k]);
  const list = readMachines(qc);
  return list && at(list.length, list.findIndex((m) => m.id === id), (k) => list[k].id);
}
