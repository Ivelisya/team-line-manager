import { useRef } from 'react';

/** 对话框/抽屉的关闭动画期间还要继续渲染内容：id 清空后返回最近一次有数据时的值，直到下次打开 */
export function useHeld<T>(id: string | null | undefined, data: T | undefined): T | undefined {
  const last = useRef<T | undefined>(undefined);
  if (id && data) last.current = data;
  return id ? data : last.current;
}
