// 单元测试（pnpm test）：测试放在 tests/ 下而不是 src/ 里——admin.css 的 @source 扫整个 src，测试里的字符串会被当成工具类打进样式。
// 不带 vite.config.ts 的 React / Tailwind 插件：测的都是纯逻辑，只要 @/ 别名
import path from 'node:path';
import { defineConfig } from 'vitest/config';

export default defineConfig({
  resolve: { alias: { '@': path.resolve(import.meta.dirname, 'src') } },
  test: { include: ['tests/**/*.test.ts'], environment: 'node' },
});
