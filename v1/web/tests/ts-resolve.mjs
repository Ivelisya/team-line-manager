// node 直接跑 TS 测试（--experimental-strip-types 默认开）时，源码里的相对导入不带扩展名（按 vite / tsc bundler 解析）：补上 .ts 再找
import { registerHooks } from 'node:module';
registerHooks({
  resolve(spec, ctx, next) {
    try { return next(spec, ctx); } catch (e) { if (spec.startsWith('.') && !/\.\w+$/.test(spec)) return next(`${spec}.ts`, ctx); throw e; }
  },
});
