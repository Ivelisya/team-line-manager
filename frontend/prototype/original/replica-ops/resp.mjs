// 响应式补充截图：390 / 900 宽。
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { launch } from './chrome.mjs';
const here = path.dirname(fileURLToPath(import.meta.url));
const BASE = 'https://127.0.0.1:5198';
const b = await launch({ port: 9348 });
for (const [w, h, url, name] of [[390, 844, '/admin/nodes', 'resp-390-nodes'], [390, 844, '/admin/dashboard', 'resp-390-overview'], [900, 900, '/admin/nodes', 'resp-900-nodes'], [900, 900, '/admin/lines', 'resp-900-lines']]) {
  await b.viewport(w, h); await b.media(false);
  await b.go(BASE + '/komari-admin/manifest.json', 300);
  await b.eval(`localStorage.setItem('appearance','"light"');localStorage.setItem('ops-density','comfortable');sessionStorage.clear();true`);
  await b.go(BASE + url, 5000);
  await b.shot(path.join(here, 'renders', name + '.png'));
  console.log('shot', name);
}
await b.close();
