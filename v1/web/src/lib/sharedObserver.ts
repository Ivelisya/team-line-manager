// 全站共享一个观察器（IntersectionObserver / ResizeObserver）：几十个元素各建一个没有必要。
// 返回 watch(el, cb)，cb 收到该元素的条目；watch 的返回值取消观察。观察器第一次用到时才建。
// 同一元素可以被几处同时观察，各自的回调互不覆盖，最后一处取消时才 unobserve。
type Obs = { observe(el: Element): void; unobserve(el: Element): void };

export function sharedObserver<E extends { target: Element }>(make: (onEntries: (entries: E[]) => void) => Obs) {
  let obs: Obs | null = null;
  const callbacks = new Map<Element, Set<(e: E) => void>>();
  return (el: Element, cb: (e: E) => void) => {
    obs ??= make((entries) => { for (const e of entries) callbacks.get(e.target)?.forEach((f) => f(e)); });
    let set = callbacks.get(el);
    if (set) obs.unobserve(el); // 重新观察：新来的回调也收到一条初始条目（重复 observe 不会再发）
    else callbacks.set(el, (set = new Set()));
    set.add(cb);
    obs.observe(el);
    return () => {
      set.delete(cb);
      if (set.size === 0 && callbacks.get(el) === set) { callbacks.delete(el); obs?.unobserve(el); }
    };
  };
}

/** 全站共用的 ResizeObserver（省略文本 Trunc、机器表放得下几列、卡片标签行） */
export const watchSize = sharedObserver<ResizeObserverEntry>((cb) => new ResizeObserver(cb));
