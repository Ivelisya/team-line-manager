import { defineConfig, type ProxyOptions } from 'vite';
import react from '@vitejs/plugin-react';
import tailwindcss from '@tailwindcss/vite';
import path from 'node:path';

// 开发代理：/api（含实时流 WebSocket）与 /echo 转发到中控
const target = 'http://192.168.31.60:8080';

/** 模块 id 所在的包名（node_modules 之外为 undefined） */
const pkgOf = (id: string) => /.*node_modules[\\/]((?:@[^\\/]+[\\/])?[^\\/]+)/.exec(id)?.[1]?.replace('\\', '/');
const VENDOR = new Set(['clsx', 'react', 'react-dom', 'scheduler', 'react-router', 'cookie', 'set-cookie-parser', '@tanstack/react-query', '@tanstack/query-core']);

// --mode mock：浏览器内 mock 顶替后端（这里直接把 import.meta.env.VITE_MOCK 定成 "1"；不用 .env.mock，仓库根的 .gitignore 忽略 .env.*，CI 上拿不到）。不配代理（dev 与 preview 都不连中控），构建输出到 dist-mock/，不覆盖正式的 dist/
export default defineConfig(({ mode }) => {
  const mock = mode === 'mock';
  const proxy: Record<string, ProxyOptions> = mock ? {} : {
    // 实时流用会话 Cookie 连接时中控要求 Origin 等于它的 public_url（安全会话第 21 轮），代理把 Origin 改成中控的（ws 升级请求同样生效）
    '/api': { target, changeOrigin: true, ws: true, headers: { origin: target } },
    '/echo': { target, changeOrigin: true },
  };
  return {
    plugins: [react(), tailwindcss()],
    define: mock ? { 'import.meta.env.VITE_MOCK': JSON.stringify('1') } : {},
    // @mock/server：浏览器内 mock 的入口（main.tsx 在 mock 模式下动态引入；类型见 src/mock-entry.d.ts）
    resolve: { alias: { '@mock/server': path.resolve(import.meta.dirname, 'src/mock/server.ts'), '@': path.resolve(import.meta.dirname, 'src') } },
    server: { port: 5173, proxy },
    preview: { proxy },
    build: {
      outDir: mock ? 'dist-mock' : 'dist',
      sourcemap: false,
      // 按包名精确划分（Vite 8 / Rolldown 的 codeSplitting.groups，取代原来的 manualChunks 函数）：图表相关包只进 charts（只有抽屉的图表页签动态加载它），
      // 网页终端的 xterm 单独成块（features/Terminal 懒加载），框架核心进 vendor，其余照默认。
      // 原来写函数而不写数组的原因（数组形式会把依赖也拉进该块，clsx、react-is 这类共用小包被塞进 charts，登录页也得下载 400KB 图表）在这里同样成立：
      // 组的 test 按包名精确匹配；includeDependenciesRecursively 默认开着（避免循环块），这三组的依赖都在组内（d3 系、react 系），不会把共用小包拖进来——改分组后用 pnpm size 核对三份闭包
      rolldownOptions: {
        output: {
          codeSplitting: {
            groups: [
              { name: 'charts', test: (id) => /^(internmap|d3-.*)$/.test(pkgOf(id) ?? ''), priority: 30 }, // bklit 用到的 d3-array / d3-scale / d3-shape 及其依赖；motion 是全站共用的，不进这一块
              { name: 'xterm', test: (id) => (pkgOf(id) ?? '').startsWith('@xterm/'), priority: 20 },
              { name: 'vendor', test: (id) => VENDOR.has(pkgOf(id) ?? ''), priority: 10 },
            ],
          },
        },
      },
    },
  };
});
