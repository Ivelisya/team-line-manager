// 本地偏好（localStorage）：隐私模式、存储被禁用时读写都会抛错，读不到当作没有、写不进就算了
export const prefs = {
  get: (k: string): string | null => { try { return localStorage.getItem(k); } catch { return null; } },
  set: (k: string, v: string) => { try { localStorage.setItem(k, v); } catch { /* 隐私模式 */ } },
};
/** 读一个枚举偏好：值不在 allowed 里（旧版本或手改的脏值）回退默认 */
export const prefOf = <T extends string>(k: string, d: T, allowed: readonly T[]): T => {
  const v = prefs.get(k) as T | null;
  return v && allowed.includes(v) ? v : d;
};
