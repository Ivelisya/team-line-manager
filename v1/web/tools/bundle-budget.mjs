// 包体守卫：在内存里跑一遍 vite build（不写 dist/），按 import 图算三份「静态闭包」的体积（原始 / gzip）：
//   管理端首屏（index.html → main → adminEntry 及其静态依赖）、客户页（main → customerEntry）、第一次打开节点抽屉多下载的块（NodeDrawer 的闭包减去首屏已有的）。
// 用法：node tools/bundle-budget.mjs          打印三份闭包的体积与客户页闭包里的源文件
//       node tools/bundle-budget.mjs --check  客户页的 JS 闭包里出现管理端专用模块、或超出体积预算、或正式构建里有浏览器内 mock，就退出码 1（pnpm check:customer）
// 客户页样式另由 customer-sources.mjs 守着；这里只管 JS。预算比实测略高一点，涨到超出说明有东西把管理端的代码拖进来了，先查是谁 import 的。
import path from 'node:path';
import zlib from 'node:zlib';
import { fileURLToPath } from 'node:url';
import { build } from 'vite';

const web = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
// 客户页 JS 闭包（含 main 入口与 vendor）的 gzip 预算，单位字节：实测值上浮约 5%（React、路由、react-query 的 vendor 约占 90 KB）
const CUSTOMER_GZIP_BUDGET = 154 * 1024; // 拆出 Panel 后实测约 146 KB（gzip -9）
// 客户页不该带上的源文件（相对 src/）与依赖包：管理端的数据层、实时流、抽屉、动效、Base UI 的 toast 等
const ADMIN_ONLY = [
  /^lib\/(queries|derive|flags|live|stream|store|drawer|nodeActions|routers|motion|motionFeatures|palette|terminal|jobs|nav|title|theme)\.tsx?$/,
  /^components\/app\/primitives\.tsx$/,
  /^components\/ui\/(toast|dialog|sheet|command|date-field|calendar)\.tsx$/,
  /^(layout|features\/(?!billing\.tsx$))/,
  /^pages\/(?!Customer\.tsx$)/,
  /^mock\//,
];
const ADMIN_ONLY_PKGS = ['motion', 'framer-motion', 'motion-dom', 'cmdk', 'react-day-picker', 'date-fns', '@xterm/xterm', 'd3-scale', 'd3-shape', 'd3-array'];

const out = await build({ root: web, mode: 'production', logLevel: 'silent', build: { write: false, reportCompressedSize: false } });
const bundle = (Array.isArray(out) ? out : [out]).flatMap((o) => o.output);
const byName = new Map(bundle.map((f) => [f.fileName, f]));
const chunks = bundle.filter((f) => f.type === 'chunk');
const size = (f) => { const b = Buffer.from(f.type === 'chunk' ? f.code : f.source); return [b.length, zlib.gzipSync(b, { level: 9 }).length]; };
const posix = (id) => id.split(path.sep).join('/');
// 动态入口所在的块：块被合并时 facadeModuleId 可能是空的，就按块里含不含这个模块找
const facade = (re) => {
  const c = chunks.find((f) => f.facadeModuleId && re.test(posix(f.facadeModuleId))) ?? chunks.find((f) => f.isDynamicEntry && Object.keys(f.modules).some((id) => re.test(posix(id))));
  if (!c) throw new Error(`找不到入口块 ${re}`);
  return c;
};

/** 从若干块出发沿静态 import 走到底；同时收集它们引用的 CSS */
function closure(starts) {
  const seen = new Set();
  const walk = (name) => {
    if (seen.has(name)) return;
    seen.add(name);
    const c = byName.get(name);
    if (c?.type !== 'chunk') return;
    for (const i of c.imports) walk(i);
    for (const css of c.viteMetadata?.importedCss ?? []) seen.add(css);
  };
  for (const s of starts) walk(s.fileName);
  return seen;
}
const total = (files) => {
  const t = { js: [0, 0], css: [0, 0], n: files.size };
  for (const n of files) { const f = byName.get(n); const [r, g] = size(f); const k = n.endsWith('.css') ? 'css' : 'js'; t[k][0] += r; t[k][1] += g; }
  return t;
};

const main = chunks.find((f) => f.isEntry); // index.html 的入口块（main.tsx）
const admin = closure([main, facade(/\/src\/adminEntry\.tsx$/)]);
const customer = closure([main, facade(/\/src\/customerEntry\.tsx$/)]);
const drawerAll = closure([facade(/\/src\/features\/NodeDrawer\.tsx$/)]);
const drawer = new Set([...drawerAll].filter((n) => !admin.has(n)));

const kb = (n) => `${(n / 1024).toFixed(1)} KB`;
const row = (label, files) => {
  const t = total(files);
  return `${label.padEnd(18)} ${String(t.n).padStart(3)} 个文件  JS ${kb(t.js[0]).padStart(9)} / gzip ${kb(t.js[1]).padStart(8)}   CSS ${kb(t.css[0]).padStart(8)} / gzip ${kb(t.css[1]).padStart(7)}`;
};
console.log(row('管理端首屏', admin));
console.log(row('客户页 /c/:token', customer));
console.log(row('打开节点抽屉另加', drawer));
if (process.argv.includes('--verbose')) for (const [label, set] of [['打开节点抽屉另加', drawer], ['客户页', customer]]) {
  console.log(`\n${label}：`);
  for (const n of [...set].sort()) console.log(`  ${n.padEnd(48)} ${kb(size(byName.get(n))[0]).padStart(9)}`);
}

// 客户页闭包里的源模块（按块里实际打进去的模块算，tree-shake 掉的不算）
const rel = (id) => {
  const p = id.split(path.sep).join('/').replace(/\?.*$/, '');
  const nm = p.match(/.*node_modules\/((?:@[^/]+\/)?[^/]+)/);
  if (nm) return { pkg: nm[1] };
  const i = p.indexOf('/src/');
  return i >= 0 ? { src: p.slice(i + 5) } : {};
};
const customerMods = [...customer].filter((n) => n.endsWith('.js')).flatMap((n) => Object.entries(byName.get(n).modules).filter(([, m]) => m.renderedLength > 0).map(([id]) => rel(id)));
const srcs = [...new Set(customerMods.map((m) => m.src).filter(Boolean))].sort();
const pkgs = [...new Set(customerMods.map((m) => m.pkg).filter(Boolean))].sort();
if (process.argv.includes('--verbose')) console.log(`\n客户页源文件：\n  ${srcs.join('\n  ')}\n客户页依赖包：\n  ${pkgs.join('\n  ')}`);

if (process.argv.includes('--check')) {
  const bad = [...srcs.filter((s) => ADMIN_ONLY.some((re) => re.test(s))), ...pkgs.filter((p) => ADMIN_ONLY_PKGS.includes(p))];
  const gz = total(customer).js[1];
  const problems = [];
  // 浏览器内 mock（src/mock，VITE_MOCK=1 / --mode mock 才装）不能进正式构建：哪个块里都不该有它的模块，也不该有它的字样
  const mockIn = chunks.filter((c) => Object.keys(c.modules).some((id) => /\/src\/mock\//.test(posix(id))) || c.code.includes('mock 未实现'));
  if (mockIn.length) problems.push(`正式构建里带上了浏览器内 mock：${mockIn.map((c) => c.fileName).join('、')}（main.tsx 的 import.meta.env.VITE_MOCK 判断是不是被改了）`);
  if (bad.length) problems.push(`客户页的 JS 闭包带上了管理端专用模块：${bad.join('、')}（查是谁 import 了它们，拆成单独的小模块，参考 components/app/empty.tsx、panel.tsx）`);
  if (gz > CUSTOMER_GZIP_BUDGET) problems.push(`客户页 JS 闭包 gzip ${kb(gz)}，超出预算 ${kb(CUSTOMER_GZIP_BUDGET)}`);
  // 说明写到 stderr：部署脚本会丢掉 stdout
  if (problems.length) { for (const p of problems) console.error(p); process.exit(1); }
  console.log(`客户页 JS 闭包未带管理端专用模块，gzip ${kb(gz)} ≤ 预算 ${kb(CUSTOMER_GZIP_BUDGET)}`);
}
