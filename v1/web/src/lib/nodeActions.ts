// 卡片/表格菜单触发的“要弹对话框”的动作：状态放在这里，由外壳里唯一一份 NodeActionsHost 渲染对话框，
// 这样几十张卡片不用各带一份对话框和 mutation。
import { useSyncExternalStore } from 'react';

let retireId: string | null = null;
const subs = new Set<() => void>();
const emit = () => subs.forEach((f) => f());
const subscribe = (f: () => void) => { subs.add(f); return () => { subs.delete(f); }; };

export const openRetire = (id: string) => { retireId = id; emit(); };
export const closeRetire = () => { retireId = null; emit(); };
export const useRetireTarget = () => useSyncExternalStore(subscribe, () => retireId, () => null);
