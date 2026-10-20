// fleet.js — 页面读 fleetd（新后台，fleet/API.md）的小助手：可用性、GET、写、SSE 变化订阅、“新后台未上线”空状态。
// 数据层（deploy/v2/live/boot.js 的 TEAMOPS.fleet）在加载时探测 fleetd；原型模式与 fleetd 未上线时 available 为 false。
import { esc } from './ui/dom.js';
import { empty, note } from './ui/layout.js';

const F = () => globalThis.TEAMOPS?.fleet ?? null;
export const fleetAvailable = () => !!F()?.available;
/** GET /v2/api/v1/<path> → data（fleetd 的 {data} 已拆开）；不可用时抛错。 */
export async function fleetGet(path, query) {
  const f = F();
  if (!f?.available) throw new Error('新后台未上线');
  return f.get(path, query);
}
/** fleetd 写（确认告警、静默、维护窗口）：X-Fleet-Request: 1。 */
export const fleetWrite = (method, path, body) => F().write(method, path, body);
/** fn({resource}) — SSE 推来 alerts / notify / silences 变化，或 'fleet-down'（回落到旧接口）。 */
export const onFleet = (fn) => F()?.on?.(fn) ?? (() => {});
/** 旧后端的只读 GET（当前数据源：fleetd 的 /v2/api/legacy/ 或 /api/team-ops/），给 API 密钥页这类不在快照里的列表。 */
export async function legacyGet(path, query) {
  const src = globalThis.TEAMOPS?.source;
  if (!src) throw new Error('需要实时数据');
  return src.get(path, query, { priority: 0 });
}

/** fleetd 不在时整页的空状态（新页面：事项与告警、通知、套餐与配额）。 */
export function fleetAbsent(what) {
  return `<section class="panel"><div class="panel-body">${empty({ icon: 'server-cog', text: '新后台未上线' })}
    ${note({ tone: 'info', title: `${what}由新后台提供`, body: esc('新后台（fleetd）上线后这里自动有数据，不用刷新构建；在那之前请以原版后台为准。') })}</div></section>`;
}
