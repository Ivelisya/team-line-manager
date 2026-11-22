// Playwright（pnpm e2e）：只用 Chromium。被测的是 mock 模式的生产构建（vite build --mode mock → dist-mock/，浏览器内 mock 顶替后端），
// 由 webServer 先构建再起 vite preview；本机已经开着同一端口的预览时直接用它（CI 上每次都重新构建）。
// 性能测量（tools/perf.mjs）另起自己的预览，不走这里。
import { defineConfig, devices } from '@playwright/test';

const PORT = 4318;

export default defineConfig({
  testDir: 'e2e',
  workers: 1,
  forbidOnly: !!process.env.CI,
  reporter: [['list']],
  outputDir: 'test-results',
  use: { ...devices['Desktop Chrome'], baseURL: `http://127.0.0.1:${PORT}`, viewport: { width: 1440, height: 1000 }, trace: 'retain-on-failure' },
  webServer: {
    command: `pnpm build:mock && pnpm exec vite preview --mode mock --host 127.0.0.1 --port ${PORT} --strictPort`,
    url: `http://127.0.0.1:${PORT}`,
    reuseExistingServer: !process.env.CI,
    timeout: 180_000,
  },
});
