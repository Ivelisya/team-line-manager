import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import path from 'node:path';

// 开发代理：/api 与 /echo 转发到中控
const target = 'http://192.168.31.60:8080';

export default defineConfig({
  plugins: [react()],
  resolve: { alias: { '@': path.resolve(__dirname, 'src') } },
  server: {
    port: 5173,
    proxy: {
      '/api': { target, changeOrigin: true },
      '/echo': { target, changeOrigin: true },
    },
  },
  build: {
    sourcemap: false,
    // 数组形式的 manualChunks 会把依赖也一并拉进该块（clsx、react-is 这类共用小包被塞进 charts，登录页也得下载 400KB 图表）。
    // 这里按包名精确划分：图表相关包只进 charts（只有抽屉的图表页签动态加载它），框架核心进 vendor，其余照默认。
    rollupOptions: {
      output: {
        manualChunks(id) {
          const m = id.match(/.*node_modules\/((?:@[^/]+\/)?[^/]+)/);
          if (!m) return undefined;
          const pkg = m[1];
          if (['recharts', 'recharts-scale', 'victory-vendor', 'internmap', 'decimal.js-light', 'lodash', 'react-smooth', 'fast-equals', 'react-transition-group', 'dom-helpers', 'eventemitter3'].includes(pkg) || pkg.startsWith('d3-')) return 'charts';
          if (['clsx', 'react', 'react-dom', 'scheduler', 'react-router', 'react-router-dom', '@remix-run/router', '@tanstack/react-query', '@tanstack/query-core', '@tanstack/react-table', '@tanstack/table-core'].includes(pkg)) return 'vendor';
          return undefined;
        },
      },
    },
  },
});
