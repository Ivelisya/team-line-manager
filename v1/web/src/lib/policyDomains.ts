// 访问规则的域名规范化与校验：和服务端 crates/fleet-server/src/policy.rs 的 normalize_one 保持一致（保存时服务端仍会再校验一遍）。
// 纯函数，不依赖 React，便于单独测试。

const MAX_DOMAINS = 300;
export const MAX_NOTE = 500;

const SEP = /[\s,，、;；]+/;
const PREFIXES = ['domain:', 'full:', 'regexp:', 'keyword:', 'geosite:', 'ext:'];

/** 单个域名：返回规范化结果（精确匹配保留 `=` 前缀）或中文错误信息 */
function normalizeOne(tok: string): { ok: true; value: string } | { ok: false; error: string } {
  const shown = [...tok].slice(0, 80).join('');
  const bad = (error: string) => ({ ok: false as const, error });
  const exact = tok.startsWith('=');
  let d = (exact ? tok.slice(1) : tok).toLowerCase();
  d = d.replace(/\.+$/, '');
  if (!d) return bad(`「${shown}」后面没有域名`);
  // eslint-disable-next-line no-control-regex
  if (/[^\x00-\x7f]/.test(d)) return bad(`「${shown}」含非 ASCII 字符，请填 xn-- 形式`);
  if (d.includes('://') || /[/?#@]/.test(d)) return bad(`「${shown}」是网址，请只填域名`);
  const pre = PREFIXES.find((p) => d.startsWith(p));
  if (pre) return bad(`「${shown}」不要带 ${pre} 前缀`);
  const ipLike = isIp(d) || isIp(d.replace(/^\[|\]$/g, '')) || d.split('.').every((l) => l !== '' && /^[0-9]+$/.test(l));
  if (ipLike) return bad(`「${shown}」是 IP 地址，请填域名`);
  if (d.includes(':')) return bad(`「${shown}」不要写端口`);
  if (d.includes('*')) return bad(`「${shown}」不要写 *，已含子域名`);
  if (/[\^$()[\]{}\\|+!%&'"`<>~=,]/.test(d)) return bad(`「${shown}」含有不支持的字符`);
  if (d.length > 253) return bad(`「${shown}」太长（最多 253 个字符）`);
  if (!d.includes('.')) return bad(`「${shown}」必须包含点（例如 example.com）`);
  for (const label of d.split('.')) {
    if (!label) return bad(`「${shown}」里有空的标签（连续的点或开头的点）`);
    if (label.length > 63) return bad(`「${shown}」的标签「${label}」超过 63 个字符`);
    if (!/^[a-z0-9-]+$/.test(label)) return bad(`「${shown}」的标签「${label}」只能包含字母、数字和连字符`);
    if (label.startsWith('-') || label.endsWith('-')) return bad(`「${shown}」的标签「${label}」不能以连字符开头或结尾`);
  }
  return { ok: true, value: exact ? `=${d}` : d };
}

/** IPv4 / IPv6 字面量（够用的粗判：IPv4 四段数字，或含冒号且只有十六进制与冒号点） */
function isIp(s: string): boolean {
  if (/^\d{1,3}(\.\d{1,3}){3}$/.test(s)) return s.split('.').every((n) => Number(n) <= 255);
  return s.includes(':') && /^[0-9a-f:.]+$/.test(s) && s.split(':').length >= 3;
}

export interface LineIssue { line: number; token: string; error: string }
export interface ParsedDomains {
  /** 规范化、去重、排序后的域名（与服务端一致） */
  domains: string[];
  /** 非法的位置（行号从 1 开始），一行里有多个非法项会各出一条 */
  issues: LineIssue[];
  /** 哪些行（从 1 开始）含有非法项 */
  badLines: Set<number>;
  /** 超出数量上限等整体性问题 */
  overall: string | null;
}

/** 多行文本 → 规范化结果与逐行问题。逗号 / 中文逗号 / 空白都算分隔 */
export function parseDomainText(text: string): ParsedDomains {
  const set = new Set<string>();
  const issues: LineIssue[] = [];
  text.split('\n').forEach((line, i) => {
    for (const tok of line.split(SEP).filter(Boolean)) {
      const r = normalizeOne(tok);
      if (r.ok) set.add(r.value); else issues.push({ line: i + 1, token: tok, error: r.error });
    }
  });
  const domains = [...set].sort();
  const overall = domains.length > MAX_DOMAINS ? `每份策略最多 ${MAX_DOMAINS} 个域名，现在有 ${domains.length} 个` : null;
  return { domains, issues, badLines: new Set(issues.map((x) => x.line)), overall };
}

/** 域名列表 → 文本框内容（一行一个） */
export const domainsToText = (d: string[]) => d.join('\n');

/** 两份域名列表的差异（按规范化后的值） */
export function diffDomains(before: string[], after: string[]): { added: string[]; removed: string[] } {
  const b = new Set(before); const a = new Set(after);
  return { added: after.filter((x) => !b.has(x)), removed: before.filter((x) => !a.has(x)) };
}
