/** 图表文字（--font-num = 打包的 Inter Num）每个字符的宽度（em）。11px 下实测 400 / 600 两档字重取较大者，凑到 0.01：
 *  数字是等宽的 0.65（以前的统一 0.58 偏窄，「100%」会少估 20% 以上）；% 接近 1em；大写 0.70、小写 0.55；句点、逗号、冒号、空格都窄；
 *  汉字 / 全角按 1em；表里没有的符号取 0.6。换字体后要重新量（tools 里没有现成脚本，用浏览器逐字符重复 40 次量 span 宽度）。 */
const EM: Record<string, number> = {
  '%': 1.0, '-': 0.47, '+': 0.67, '/': 0.38, '(': 0.38, ')': 0.38, '↑': 0.88, '↓': 0.88, '→': 0.88,
  '.': 0.32, ',': 0.32, ':': 0.32, ';': 0.32, '·': 0.32, ' ': 0.29,
};

function emOf(ch: string): number {
  const c = ch.codePointAt(0) ?? 0;
  if ((c >= 0x2e80 && c <= 0x9fff) || (c >= 0xff00 && c <= 0xffef) || (c >= 0x3000 && c <= 0x303f)) return 1;
  const e = EM[ch];
  if (e !== undefined) return e;
  if (c >= 0x30 && c <= 0x39) return 0.65;
  if (c >= 0x41 && c <= 0x5a) return 0.7;
  if (c >= 0x61 && c <= 0x7a) return 0.55;
  return 0.6;
}

/** 估算一段文字在给定字号（px）下的宽度。图表里留边距、判断斜纹文字放不放得下、横轴标签防重叠都用它，不要各写各的常数。 */
export function textWidth(text: string, px: number): number {
  let em = 0;
  for (const ch of text) em += emOf(ch);
  return em * px;
}

/** 纵轴 / 条尾标签需要的宽度：最长一条的文字宽度 + 14px 留白（到绘图区与到图边） */
export function axisLabelWidth(labels: string[], px: number): number {
  return Math.max(0, ...labels.map((l) => textWidth(l, px))) + 14;
}
