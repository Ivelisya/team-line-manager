// 时区的中文显示：「洛杉矶（UTC−7）」。偏移按当前时刻算（夏令时期间跟着变）；常用时区给中文城市名，没收录的城市名显示 IANA 原名。
// 供应商、订阅的账单时区（表格、抽屉「费用」页、时区下拉）共用
import { meta } from './derive';

const CITY: Record<string, string> = {
  'Asia/Shanghai': '上海', 'Asia/Hong_Kong': '香港', 'Asia/Macau': '澳门', 'Asia/Taipei': '台北', 'Asia/Tokyo': '东京', 'Asia/Seoul': '首尔',
  'Asia/Singapore': '新加坡', 'Asia/Kuala_Lumpur': '吉隆坡', 'Asia/Bangkok': '曼谷', 'Asia/Ho_Chi_Minh': '胡志明市', 'Asia/Jakarta': '雅加达',
  'Asia/Manila': '马尼拉', 'Asia/Kolkata': '加尔各答', 'Asia/Karachi': '卡拉奇', 'Asia/Dubai': '迪拜', 'Asia/Jerusalem': '耶路撒冷',
  'Europe/London': '伦敦', 'Europe/Dublin': '都柏林', 'Europe/Paris': '巴黎', 'Europe/Berlin': '柏林', 'Europe/Amsterdam': '阿姆斯特丹',
  'Europe/Brussels': '布鲁塞尔', 'Europe/Zurich': '苏黎世', 'Europe/Madrid': '马德里', 'Europe/Rome': '罗马', 'Europe/Stockholm': '斯德哥尔摩',
  'Europe/Warsaw': '华沙', 'Europe/Helsinki': '赫尔辛基', 'Europe/Istanbul': '伊斯坦布尔', 'Europe/Moscow': '莫斯科',
  'America/Los_Angeles': '洛杉矶', 'America/Vancouver': '温哥华', 'America/Phoenix': '凤凰城', 'America/Denver': '丹佛', 'America/Chicago': '芝加哥',
  'America/New_York': '纽约', 'America/Toronto': '多伦多', 'America/Mexico_City': '墨西哥城', 'America/Sao_Paulo': '圣保罗', 'America/Anchorage': '安克雷奇',
  'Pacific/Honolulu': '檀香山', 'Australia/Sydney': '悉尼', 'Australia/Melbourne': '墨尔本', 'Pacific/Auckland': '奥克兰',
  'Africa/Johannesburg': '约翰内斯堡', 'Africa/Cairo': '开罗',
};

/** 现在的 UTC 偏移：UTC+8、UTC−7（减号用 U+2212）、UTC+5:30；不认识的时区名为 null */
export function utcOffset(tz: string, at = new Date()): string | null {
  try {
    const p = new Intl.DateTimeFormat('en-US', { timeZone: tz, timeZoneName: 'shortOffset' }).formatToParts(at).find((x) => x.type === 'timeZoneName')?.value;
    if (!p) return null;
    const m = /^GMT(?:([+-])(\d{1,2})(?::(\d{2}))?)?$/.exec(p);
    if (!m) return null;
    return m[1] ? `UTC${m[1] === '-' ? '−' : '+'}${m[2]}${m[3] ? `:${m[3]}` : ''}` : 'UTC';
  } catch {
    return null; // RangeError：不是有效的 IANA 时区名
  }
}

/** 中文城市名；没收录的为 null */
export const tzCity = (tz: string) => CITY[tz] ?? null;

/** 显示用：「洛杉矶（UTC−7）」；没收录的城市写 IANA 原名「America/Boise（UTC−6）」；UTC 本身写「UTC」；认不出的原样返回 */
export function tzLabel(tz: string): string {
  const off = utcOffset(tz);
  if (!off) return tz;
  if (tz === 'UTC' || tz === 'Etc/UTC') return 'UTC';
  return `${CITY[tz] ?? tz}（${off}）`;
}

export interface TzOption { value: string; label: string; keywords: string[] }
const option = (tz: string): TzOption => ({ value: tz, label: tzLabel(tz), keywords: [tz, CITY[tz] ?? '', utcOffset(tz) ?? ''].filter(Boolean) });

/** 可搜索下拉的两组：常用（/meta 的常用时区，当前值不在里面时补在最前）与全部（浏览器认得的所有 IANA 时区，去掉常用里已有的） */
export function tzOptions(current?: string): { common: TzOption[]; all: TzOption[] } {
  const common = meta().timezones.slice();
  if (current && !common.includes(current)) common.unshift(current);
  const seen = new Set(common);
  return { common: common.map(option), all: Intl.supportedValuesOf('timeZone').filter((z) => !seen.has(z)).map(option) };
}
