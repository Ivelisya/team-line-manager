// 列表搜索的匹配档位：命令面板、新建静默的对象选择共用（cmdk 的 filter 用它打分）。

/** 条目的可搜索文本：「名称 \t IP \t 其它（团队、类型、别名）」。名称就是界面上显示的那行字，没有 IP 的留空 */
export const sv = (name: string, ip: string, rest: string) => `${name}\t${ip}\t${rest}`;
/** 编号去掉前导零：团队1-04 → 团队1-4，让「1-4」也能按名称命中它 */
const unpad = (s: string) => s.replace(/\d+/g, (d) => d.replace(/^0+(?=\d)/, ''));
const subseq = (s: string, q: string) => { const t = [...q]; let i = 0; for (const c of s) if (c === t[i]) i++; return i >= t.length; };
const CJK = /[㐀-鿿]/;
/** s 里有没有 q，且 q 以数字结尾时后面不紧跟数字：去前导零后「c-1」命中「C-01」，不命中「C-10」 */
function hasWhole(s: string, q: string): boolean {
  const tail = /\d$/.test(q);
  for (let i = s.indexOf(q); i >= 0; i = s.indexOf(q, i + 1)) if (!tail || !/\d/.test(s[i + q.length] ?? '')) return true;
  return false;
}
/** 匹配档位（大的在前，0 = 不匹配）：名称相同 > 名称前缀 > IP 前缀 > 名称包含 > 去前导零后名称包含（编号整段） > IP、团队、别名等包含 > 名称模糊。
 *  模糊（子序列）只对名称、且只在搜的不含中文时：中文按连续子串匹配（搜「团队1」不该命中「模拟团队B-17」）；IP、UUID 里数字多，短数字串做子序列几乎条条命中 */
export function matchTier(value: string, q: string): number {
  const [name, ip = '', rest = ''] = value.toLowerCase().split('\t');
  if (name === q) return 8;
  if (name.startsWith(q)) return 7;
  if (ip && ip.startsWith(q)) return 6;
  if (name.includes(q)) return 5;
  if (hasWhole(unpad(name), unpad(q))) return 4;
  if (ip.includes(q) || rest.includes(q)) return 3;
  return !CJK.test(q) && subseq(name, q) ? 2 : 0;
}
