import { useEffect, useRef, useState } from 'react';

// 全站共享一个 IntersectionObserver：几十张卡片各建一个观察器没有必要。
// rootMargin 200px：快滚到视口前就提前为 true，数据已在路上，滚进来时不会看到空白。
let shared: IntersectionObserver | null = null;
const callbacks = new WeakMap<Element, (v: boolean) => void>();

function watch(el: Element, cb: (v: boolean) => void) {
  shared ??= new IntersectionObserver((entries) => { for (const e of entries) callbacks.get(e.target)?.(e.isIntersecting); }, { rootMargin: '200px' });
  callbacks.set(el, cb);
  shared.observe(el);
  return () => { callbacks.delete(el); shared?.unobserve(el); };
}

/** 元素当前是否在（或快要进入）视口。进出都会更新——用来在滚出视口后停掉轮询，滚回来再恢复。
 *  不支持 IntersectionObserver 的环境恒为 true。 */
export function useInView<T extends Element>() {
  const ref = useRef<T | null>(null);
  const [inView, setInView] = useState(typeof IntersectionObserver === 'undefined');
  useEffect(() => {
    const el = ref.current;
    if (!el || typeof IntersectionObserver === 'undefined') return;
    return watch(el, setInView);
  }, []);
  return [ref, inView] as const;
}
