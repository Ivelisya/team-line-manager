// 国旗：flag-icons 的 4:3 SVG（MIT）。构建时每面旗是一个单独的静态文件（no-inline：小的也不内联进脚本），
// 这里只有「国家代码 → 地址」的表，图片由浏览器按需加载。不用 emoji 国旗：Windows 上的 Chrome 显示成两个字母
const urls = import.meta.glob<string>('/node_modules/flag-icons/flags/4x3/*.svg', { query: '?no-inline', import: 'default', eager: true });
const byCode = new Map(Object.entries(urls).map(([path, url]) => [path.slice(path.lastIndexOf('/') + 1, -4).toUpperCase(), url]));
export const flagUrl = (cc: string) => byCode.get(cc);

const names = new Intl.DisplayNames(['zh-CN'], { type: 'region' });
/** 中文国名（ISO 3166-1 两位代码，如 US → 美国） */
export const countryName = (cc: string) => names.of(cc) ?? cc;
