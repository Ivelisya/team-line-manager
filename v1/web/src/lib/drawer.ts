// 机器详情抽屉的开关由 URL 控制：?m=<机器id>&tab=<页签>。任何页面都可以打开它，后退键关闭。
import { useCallback, useRef } from 'react';
import { useSearchParams } from 'react-router-dom';
import type { QueryClient } from '@tanstack/react-query';
import { readMachines } from './queries';

export type DrawerTab = 'overview' | 'quality' | 'traffic' | 'host' | 'billing' | 'access' | 'manage';
export const DRAWER_TABS: { value: DrawerTab; label: string }[] = [
  { value: 'overview', label: '概览' }, { value: 'quality', label: '连接质量' }, { value: 'traffic', label: '流量' },
  { value: 'host', label: '主机' }, { value: 'billing', label: '费用' }, { value: 'access', label: '访问记录' }, { value: 'manage', label: '管理' },
];

export function useDrawer() {
  const [sp, setSp] = useSearchParams();
  const id = sp.get('m');
  const rawTab = sp.get('tab');
  const tab: DrawerTab = DRAWER_TABS.some((t) => t.value === rawTab) ? (rawTab as DrawerTab) : 'overview';
  // react-router 的 setSearchParams 每次地址变化都换身份；用 ref 兜住，open/setTab/close 才稳定（卡片 memo 依赖它）
  const setRef = useRef(setSp); setRef.current = setSp;
  const open = useCallback((mid: string, t?: DrawerTab) => {
    setRef.current((p) => { const n = new URLSearchParams(p); n.set('m', mid); if (t) n.set('tab', t); else n.delete('tab'); return n; });
  }, []);
  // 抽屉开着、只是换一台（[ ] 键、抽屉头部的上一台 / 下一台）：留在当前页签，替换当前历史，后退键仍是一步关掉抽屉
  const switchTo = useCallback((mid: string) => setRef.current((p) => { const n = new URLSearchParams(p); n.set('m', mid); return n; }, { replace: true }), []);
  const setTab = useCallback((t: DrawerTab) => setRef.current((p) => { const n = new URLSearchParams(p); n.set('tab', t); return n; }, { replace: true }), []);
  const close = useCallback(() => setRef.current((p) => { const n = new URLSearchParams(p); n.delete('m'); n.delete('tab'); return n; }), []);
  return { id, tab, open, switchTo, setTab, close };
}

/** 抽屉里的上一台 / 下一台（[ ] 键与抽屉头部的按钮共用）：机器列表缓存里的默认顺序（同首页），到头为 null，不绕回。
 *  不订阅列表（订阅会让每个页面都跟着取 /machines），用时读缓存；列表没取过、或这台不在在用列表里（已退役）时没有前后，返回 undefined */
export function drawerSiblings(qc: QueryClient, id: string | null): { prev: string | null; next: string | null } | undefined {
  const list = readMachines(qc);
  const i = id && list ? list.findIndex((m) => m.id === id) : -1;
  if (!list || i < 0) return undefined;
  return { prev: i > 0 ? list[i - 1].id : null, next: i < list.length - 1 ? list[i + 1].id : null };
}
