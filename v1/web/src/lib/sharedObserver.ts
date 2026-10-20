// 全站共享一个观察器（IntersectionObserver / ResizeObserver）：几十个元素各建一个没有必要。
// 返回 watch(el, cb)，cb 收到该元素的条目；watch 的返回值取消观察。观察器第一次用到时才建。
type Obs = { observe(el: Element): void; unobserve(el: Element): void };

export function sharedObserver<E extends { target: Element }>(make: (onEntries: (entries: E[]) => void) => Obs) {
  let obs: Obs | null = null;
  const callbacks = new WeakMap<Element, (e: E) => void>();
  return (el: Element, cb: (e: E) => void) => {
    obs ??= make((entries) => { for (const e of entries) callbacks.get(e.target)?.(e); });
    callbacks.set(el, cb);
    obs.observe(el);
    return () => { callbacks.delete(el); obs?.unobserve(el); };
  };
}
