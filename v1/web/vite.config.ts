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
    rollupOptions: { output: { manualChunks: { charts: ['recharts'], vendor: ['react', 'react-dom', 'react-router-dom', '@tanstack/react-query', '@tanstack/react-table'] } } },
  },
});
