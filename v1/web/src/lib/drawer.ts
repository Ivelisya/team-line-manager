// 机器详情抽屉的开关由 URL 控制：?m=<机器id>&tab=<页签>。任何页面都可以打开它，后退键关闭。
import { useCallback, useRef } from 'react';
import { useSearchParams } from 'react-router-dom';

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
  // replace：抽屉已开着、只是换一台（[ ] 键）时替换当前历史，后退键仍是一步关掉抽屉
  const open = useCallback((mid: string, t?: DrawerTab, o?: { replace?: boolean }) => {
    setRef.current((p) => { const n = new URLSearchParams(p); n.set('m', mid); if (t) n.set('tab', t); else n.delete('tab'); return n; }, { replace: o?.replace });
  }, []);
  const setTab = useCallback((t: DrawerTab) => setRef.current((p) => { const n = new URLSearchParams(p); n.set('tab', t); return n; }, { replace: true }), []);
  const close = useCallback(() => setRef.current((p) => { const n = new URLSearchParams(p); n.delete('m'); n.delete('tab'); return n; }), []);
  return { id, tab, open, setTab, close };
}
