// 列表搜索的打分（cmdk 的 filter 用）：matchTier 给命令面板、新建静默的对象选择；prefixScore 给宿主机、时区这类下拉。

/** 条目的可搜索文本：「名称 \t IP \t 其它（团队、类型、别名）」。名称就是界面上显示的那行字，没有 IP 的留空 */
export const sv = (name: string, ip: string, rest: string) => `${name}\t${ip}\t${rest}`;
/** 编号去掉前导零：团队1-04 → 团队1-4，让「1-4」也能按名称命中它 */
const unpad = (s: string) => s.replace(/\d+/g, (d) => d.replace(/^0+(?=\d)/, ''));
const subseq = (s: string, q: string) => { const t = [...q]; let i = 0; for (const c of s) if (c === t[i]) i++; return i >= t.length; };
const CJK = /[㐀-鿿]/;
/** s 里有没有 q，且编号段按整段比：q 以数字结尾时后面不紧跟数字、以数字开头时前面不紧挨数字——
 *  去前导零后「c-1」命中「C-01」，不命中「C-10」；「1-4」命中「团队1-04」，不命中「团队11-04」 */
function hasWhole(s: string, q: string): boolean {
  const tail = /\d$/.test(q); const head = /^\d/.test(q);
  for (let i = s.indexOf(q); i >= 0; i = s.indexOf(q, i + 1)) {
    if ((!tail || !/\d/.test(s[i + q.length] ?? '')) && (!head || !/\d/.test(s[i - 1] ?? ''))) return true;
  }
  return false;
}
/** 匹配档位（大的在前，0 = 不匹配）：名称相同（编号补零后相同也算） > 名称前缀 > IP 前缀 > 名称包含 > 去前导零后名称包含（编号整段） > IP、团队、别名等包含 > 名称模糊。
 *  模糊（子序列）只对名称、且只在搜的不含中文时：中文按连续子串匹配（搜「团队1」不该命中「模拟团队B-17」）；IP、UUID 里数字多，短数字串做子序列几乎条条命中 */
export function matchTier(value: string, q: string): number {
  const [name, ip = '', rest = ''] = value.toLowerCase().split('\t');
  if (name === q || unpad(name) === unpad(q)) return 8; // 补零后相同也算完全匹配：「团队1-4」时「团队1-04」排在「团队1-40」前面
  if (name.startsWith(q)) return 7;
  if (ip && ip.startsWith(q)) return 6;
  if (name.includes(q)) return 5;
  if (hasWhole(unpad(name), unpad(q))) return 4;
  if (ip.includes(q) || rest.includes(q)) return 3;
  return !CJK.test(q) && subseq(name, q) ? 2 : 0;
}
/** 前缀优先、连续子串：某段文字或其中某个词（按空白、/、_、- 切开）以 q 开头 1，连着出现 0.5，其余 0；q 为空都算命中。
 *  不做模糊匹配：cmdk 默认的字符散落就算太宽（搜「1.63」带出 10.77.1.36，搜「Tokyo」带出 America/Kentucky/Monticello）。q 已小写、去首尾空白 */
export function prefixScore(texts: string[], q: string): number {
  if (!q) return 1;
  const ts = texts.map((t) => t.toLowerCase());
  if (ts.some((t) => t.startsWith(q) || t.split(/[\s/_-]+/).some((w) => w.startsWith(q)))) return 1;
  return ts.some((t) => t.includes(q)) ? 0.5 : 0;
}
