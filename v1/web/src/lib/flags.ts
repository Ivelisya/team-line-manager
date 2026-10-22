// 国旗：flag-icons 的 4:3 SVG（MIT）。构建时每面旗是一个单独的静态文件（no-inline：小的也不内联进脚本），
// 这里只有「国家代码 → 地址」的表，图片由浏览器按需加载。不用 emoji 国旗：Windows 上的 Chrome 显示成两个字母
const urls = import.meta.glob<string>('/node_modules/flag-icons/flags/4x3/*.svg', { query: '?no-inline', import: 'default', eager: true });
const byCode = new Map(Object.entries(urls).map(([path, url]) => [path.slice(path.lastIndexOf('/') + 1, -4).toUpperCase(), url]));
export const flagUrl = (cc: string) => byCode.get(cc);

const names = new Intl.DisplayNames(['zh-CN'], { type: 'region' });
const en = new Intl.DisplayNames(['en'], { type: 'region' });
/** 中文国名（ISO 3166-1 两位代码，如 US → 美国） */
export const countryName = (cc: string) => names.of(cc) ?? cc;

/** 可选的国家：有国旗的两位代码，去掉不在 ISO 3166-1 正式分配里的（欧盟、联合国、太平洋共同体、未知、几个保留代码的岛屿；科索沃 XK 服务端认，保留），
 *  同服务端 country.rs 的表；按中文名（拼音）排序 */
const NOT_ISO = new Set(['CP', 'DG', 'EU', 'IC', 'PC', 'UN', 'XX']);
type Country = { code: string; zh: string; en: string };
let list: Country[] | undefined;
/** 第一次用到（打开选择器、粘贴）时才建表，首屏不花这份时间 */
export function countries() {
  if (!list) {
    const zh = new Intl.Collator('zh-CN');
    list = [...byCode.keys()]
      .filter((c) => /^[A-Z]{2}$/.test(c) && !NOT_ISO.has(c) && names.of(c) !== c)
      .map((code) => ({ code, zh: names.of(code)!, en: en.of(code)! }))
      .sort((a, b) => zh.compare(a.zh, b.zh));
  }
  return list;
}
/** 选择器最上面的一组：机器常在的地方 */
export const COMMON_COUNTRIES = ['HK', 'JP', 'SG', 'US', 'TW', 'KR', 'DE', 'GB'];

/** 粘贴或输入的国家 → 代码：认两位代码、中文名和英文名（不分大小写）；空串是「自动」，认不出是 undefined */
export function parseCountry(raw: string): string | undefined {
  const v = raw.trim(); if (!v) return '';
  const low = v.toLowerCase();
  return countries().find((c) => c.code.toLowerCase() === low || c.zh === v || c.en.toLowerCase() === low)?.code;
}
