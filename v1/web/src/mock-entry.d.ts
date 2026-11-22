// 浏览器内 mock 的入口（src/mock/server.ts）给管理端程序看的类型：main.tsx 按 @mock/server 引它，vite.config.ts 的别名指向真文件。
// mock 由别人维护，按它自己的 tsconfig.mock.json 检查（不开 noUncheckedIndexedAccess）；这样主程序的严格选项不会顺着动态 import 查进 src/mock
declare module '@mock/server' {
  export function installMock(): void;
}
