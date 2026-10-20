import { useEffect, useRef, useState } from 'react';
import { sharedObserver } from './sharedObserver';

// rootMargin 200px：快滚到视口前就提前为 true，数据已在路上，滚进来时不会看到空白。
const watch = sharedObserver<IntersectionObserverEntry>((cb) => new IntersectionObserver(cb, { rootMargin: '200px' }));

/** 元素当前是否在（或快要进入）视口。进出都会更新——用来在滚出视口后停掉轮询，滚回来再恢复。 */
export function useInView<T extends Element>() {
  const ref = useRef<T | null>(null);
  const [inView, setInView] = useState(false);
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    return watch(el, (e) => setInView(e.isIntersecting));
  }, []);
  return [ref, inView] as const;
}
