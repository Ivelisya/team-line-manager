import { useEffect, useState } from 'react';

/** 每 ms 毫秒刷新一次的当前时间；active 为 false 时不走表（已结束的作业不需要） */
export function useNow(active = true, ms = 1000) {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!active) return;
    setNow(Date.now());
    const h = setInterval(() => setNow(Date.now()), ms);
    return () => clearInterval(h);
  }, [active, ms]);
  return now;
}
