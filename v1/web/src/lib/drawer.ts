// 机器详情抽屉的开关由 URL 控制：?m=<机器id>&tab=<页签>。任何页面都可以打开它，后退键关闭。
import { useCallback, useEffect, useRef, useState } from 'react';
import { useSearchParams } from 'react-router-dom';

export type DrawerTab = 'overview' | 'quality' | 'traffic' | 'host' | 'billing' | 'manage';
export const DRAWER_TABS: { value: DrawerTab; label: string }[] = [
  { value: 'overview', label: '概览' }, { value: 'quality', label: '连接质量' }, { value: 'traffic', label: '流量' },
  { value: 'host', label: '主机' }, { value: 'billing', label: '费用' }, { value: 'manage', label: '管理' },
];

export function useDrawer() {
  const [sp, setSp] = useSearchParams();
  const id = sp.get('m');
  const tab = ((sp.get('tab') as DrawerTab) || 'overview');
  const open = useCallback((mid: string, t?: DrawerTab) => {
    setSp((p) => { const n = new URLSearchParams(p); n.set('m', mid); if (t) n.set('tab', t); else n.delete('tab'); return n; });
  }, [setSp]);
  const setTab = useCallback((t: DrawerTab) => setSp((p) => { const n = new URLSearchParams(p); n.set('tab', t); return n; }, { replace: true }), [setSp]);
  const close = useCallback(() => setSp((p) => { const n = new URLSearchParams(p); n.delete('m'); n.delete('tab'); return n; }), [setSp]);
  return { id, tab, open, setTab, close };
}

/** 进入视口后才为 true（卡片里的心跳条据此懒加载，避免一次发出几十个请求） */
export function useInView<T extends Element>() {
  const ref = useRef<T | null>(null);
  const [seen, setSeen] = useState(typeof IntersectionObserver === 'undefined');
  useEffect(() => {
    if (seen || !ref.current) return;
    const io = new IntersectionObserver(([e]) => { if (e.isIntersecting) { setSeen(true); io.disconnect(); } }, { rootMargin: '200px' });
    io.observe(ref.current);
    return () => io.disconnect();
  }, [seen]);
  return [ref, seen] as const;
}
