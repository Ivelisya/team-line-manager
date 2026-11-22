// 客户页样式的 @source 清单：从 customerEntry.tsx 起静态追 import（静态 + 动态、相对路径 + @/ 别名），打印客户页真正会打进包里的 ts / tsx 文件。
// 用法：node tools/customer-sources.mjs          打印 @source 行（粘进 src/styles/customer.css）
//       node tools/customer-sources.mjs --check  与 customer.css 里现有的清单比对，不一致就退出码 1（pnpm build 先跑它）
// 管理端页面里的工具类不该进客户页 CSS：customer.css 不再 @source 整个 src，只认这份清单。
// 只导入类型的 import type / export type 不跟（不进包，也就不带样式）。路径按本脚本的位置算，在哪个目录下跑都一样。
// 解析用 oxc-parser：TypeScript 7 的 npm 包不再带 JS 编译器接口（原来的 ts.createSourceFile 用不了）
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseSync } from 'oxc-parser';
const web = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
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
  const code = fs.readFileSync(file, 'utf8');
  const { module: mod, errors } = parseSync(file, code, { lang: file.endsWith('x') ? 'tsx' : 'ts' });
  if (errors.length) throw new Error(`${file}：${errors[0].message}`);
  // 整句是 import type / export type 的不跟；import { type A } 这种混写的照跟（同原来按 TypeScript 语法树判断的口径）
  const typeOnly = (start) => /^(import|export)\s+type\b/.test(code.slice(start, start + 20));
  const specs = [
    ...mod.staticImports.filter((i) => !typeOnly(i.start)).map((i) => i.moduleRequest.value),
    ...mod.staticExports.filter((e) => !typeOnly(e.start)).flatMap((e) => e.entries.map((x) => x.moduleRequest?.value)),
    // 动态 import：只认参数是字符串字面量的
    ...mod.dynamicImports.map((d) => /^(['"])(.+)\1$/.exec(code.slice(d.moduleRequest.start, d.moduleRequest.end))?.[2]),
  ];
  for (const spec of specs) { const r = spec && resolve(file, spec); if (r) walk(r); }
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
