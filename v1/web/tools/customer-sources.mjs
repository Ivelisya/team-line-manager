// 客户页样式的 @source 清单：从 customerEntry.tsx 起静态追 import（静态 + 动态、相对路径 + @/ 别名），打印客户页真正会打进包里的 ts / tsx 文件。
// 用法：node tools/customer-sources.mjs          打印 @source 行（粘进 src/styles/customer.css）
//       node tools/customer-sources.mjs --check  与 customer.css 里现有的清单比对，不一致就退出码 1（pnpm build 先跑它）
// 管理端页面里的工具类不该进客户页 CSS：customer.css 不再 @source 整个 src，只认这份清单。
// 只导入类型的 import type / export type 不跟（不进包，也就不带样式）。路径按本脚本的位置算，在哪个目录下跑都一样。
import fs from 'node:fs';
import path from 'node:path';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
const web = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const require = createRequire(path.join(web, 'package.json'));
const ts = require('typescript');
const src = path.join(web, 'src');
const exts = ['.ts', '.tsx', '/index.ts', '/index.tsx'];
const resolve = (from, spec) => {
  let base;
  if (spec.startsWith('@/')) base = path.join(src, spec.slice(2));
  else if (spec.startsWith('.')) base = path.resolve(path.dirname(from), spec);
  else return null;
  for (const e of ['', ...exts]) { const f = base + e; if (fs.existsSync(f) && fs.statSync(f).isFile() && /\.tsx?$/.test(f)) return f; }
  return null;
};
const seen = new Set();
const walk = (file) => {
  if (seen.has(file)) return;
  seen.add(file);
  const sf = ts.createSourceFile(file, fs.readFileSync(file, 'utf8'), ts.ScriptTarget.Latest, true, file.endsWith('x') ? ts.ScriptKind.TSX : ts.ScriptKind.TS);
  const visit = (n) => {
    let spec = null;
    const typeOnly = (ts.isImportDeclaration(n) && n.importClause?.isTypeOnly) || (ts.isExportDeclaration(n) && n.isTypeOnly);
    if ((ts.isImportDeclaration(n) || ts.isExportDeclaration(n)) && !typeOnly && n.moduleSpecifier && ts.isStringLiteral(n.moduleSpecifier)) spec = n.moduleSpecifier.text;
    else if (ts.isCallExpression(n) && n.expression.kind === ts.SyntaxKind.ImportKeyword && n.arguments[0] && ts.isStringLiteral(n.arguments[0])) spec = n.arguments[0].text;
    if (spec) { const r = resolve(file, spec); if (r) walk(r); }
    ts.forEachChild(n, visit);
  };
  visit(sf);
};
walk(path.join(src, 'customerEntry.tsx'));
const lines = [...seen].map((f) => path.relative(path.join(src, 'styles'), f).split(path.sep).join('/')).sort().map((f) => `@source "${f}";`);
if (process.argv.includes('--check')) {
  const css = fs.readFileSync(path.join(src, 'styles/customer.css'), 'utf8');
  const have = css.split('\n').filter((l) => l.startsWith('@source "') && l.includes('.ts')).sort();
  const same = have.length === lines.length && have.every((l, i) => l === lines[i]);
  // 不一致的说明写到 stderr：部署脚本跑 pnpm build 时把 stdout 丢掉了（>/dev/null），只剩 stderr 能看到为什么失败
  if (same) console.log('customer.css @source 清单与 import 图一致');
  else console.error('customer.css 的 @source 清单与客户页的 import 图不一致：在 v1/web 下重跑 node tools/customer-sources.mjs，把输出替换进 src/styles/customer.css 的 @source 行');
  process.exit(same ? 0 : 1);
}
console.log(lines.join('\n'));
