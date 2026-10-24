// 列表分页：当前页 ?p=（从 1 起）、每页条数 ?ps= 存在 URL 里，刷新和分享链接都能回到同一页。
// 筛选 / 排序 / 视图（reset）一变就回到第 1 页；列表变短时夹到最后一页。换页后若列表顶部已滚出视口，平滑滚回去。
// 每个页面只有一个分页器在用这两个参数（抽屉用 m / tab，设置页用 section，不冲突）。
import { useCallback, useEffect, useMemo, useRef } from 'react';
import { useSearchParams } from 'react-router-dom';
import { scrollBehavior } from './utils';

export interface PageState {
  page: number; size: number; sizes: readonly number[]; defaultSize: number;
  /** undefined：总数还不知道（首次加载中），这时不夹页 */
  total: number | undefined; pageCount: number; offset: number;
  setPage: (p: number) => void; setSize: (n: number) => void;
  /** 挂在列表上方的元素上：换页后滚回它 */
  anchor: (el: HTMLElement | null) => void;
}

/** sizes 传模块常量：返回值按值缓存（分页器是 memo 的），每次新数组会让它每次都换引用 */
export function usePage({ total, sizes, size: def = sizes[0], reset }: { total: number | undefined; sizes: readonly number[]; size?: number; reset: readonly unknown[] }): PageState {
  const [sp, setSp] = useSearchParams();
  // setSearchParams 每次地址变化都换身份；用 ref 兜住，回调才稳定（同 lib/drawer.ts）
  const setRef = useRef(setSp); setRef.current = setSp;
  const key = JSON.stringify(reset);
  const seen = useRef(key);
  const fresh = seen.current !== key; // 筛选刚变：这一帧就按第 1 页算，URL 在下面的 effect 里改

  const ps = Number(sp.get('ps'));
  const size = sizes.includes(ps) ? ps : def;
  const raw = fresh ? 1 : Math.max(1, Math.floor(Number(sp.get('p'))) || 1);
  const pageCount = total == null ? raw : Math.max(1, Math.ceil(total / size));
  const page = Math.min(raw, pageCount);

  const write = useCallback((p: number, n?: number) => setRef.current((prev) => {
    const q = new URLSearchParams(prev);
    if (p > 1) q.set('p', String(p)); else q.delete('p');
    if (n !== undefined) { if (n === def) q.delete('ps'); else q.set('ps', String(n)); }
    return q;
  }, { replace: true }), [def]);

  const hasP = sp.has('p');
  useEffect(() => {
    if (seen.current !== key) { seen.current = key; if (hasP) write(1); return; }
    if (raw > page) write(page); // 列表变短：URL 跟着夹到最后一页
  }, [key, hasP, raw, page, write]);

  const el = useRef<HTMLElement | null>(null);
  const anchor = useCallback((e: HTMLElement | null) => { el.current = e; }, []);
  const setPage = useCallback((p: number) => { write(Math.min(Math.max(1, p), pageCount)); scrollBackTo(el.current); }, [write, pageCount]);
  // 换每页条数时保住当前第一条所在的那一页
  const setSize = useCallback((n: number) => { write(Math.floor(((page - 1) * size) / n) + 1, n); scrollBackTo(el.current); }, [write, page, size]);

  // 按值缓存：页面随实时推送重渲染时，页码、条数、总数都没变就还是同一个对象，分页器（memo）不重画
  return useMemo(() => ({ page, size, sizes, defaultSize: def, total, pageCount, offset: (page - 1) * size, setPage, setSize, anchor }),
    [page, size, sizes, def, total, pageCount, setPage, setSize, anchor]);
}

/** 换页后：列表顶部（el）已滚到顶栏下面看不见时，平滑滚回去；系统要求减少动态效果时直接跳 */
export function scrollBackTo(el: HTMLElement | null) {
  requestAnimationFrame(() => {
    if (!el) return;
    const pad = parseFloat(getComputedStyle(document.documentElement).scrollPaddingTop) || 0; // 固定顶栏的高度
    if (el.getBoundingClientRect().top >= pad) return; // 列表顶部还在视口里：不动
    el.scrollIntoView({ block: 'start', behavior: scrollBehavior() });
  });
}
