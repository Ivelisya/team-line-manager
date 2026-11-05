/** 估算一段文字在给定字号（px）下的宽度：汉字 / 全角 ≈ 1em，其余（数字、字母、标点、箭头）≈ 0.58em。
 *  图表里留边距、判断斜纹文字放不放得下、横轴标签防重叠都用它，不要各写各的常数。 */
export function textWidth(text: string, px: number): number {
  let em = 0;
  for (const ch of text) {
    const c = ch.codePointAt(0) ?? 0;
    em += (c >= 0x2e80 && c <= 0x9fff) || (c >= 0xff00 && c <= 0xffef) || (c >= 0x3000 && c <= 0x303f) ? 1 : 0.58;
  }
  return em * px;
}

/** 纵轴 / 条尾标签需要的宽度：最长一条的文字宽度 + 14px 留白（到绘图区与到图边） */
export function axisLabelWidth(labels: string[], px: number): number {
  return Math.max(0, ...labels.map((l) => textWidth(l, px))) + 14;
}
