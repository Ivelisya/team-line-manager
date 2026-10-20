// page-tools.js — 各页页头右侧的工具（鲜度 · 视图或密度 · 主操作 · 次操作 · ↻）。
// 外壳的首屏骨架和真实页面用同一份定义：数据到达、骨架换成真实页面时，页头工具不再从无到有地插进来把“更新于”挤走（craft #6）。
// 规则：页头里所有“新增 / 新建”按钮都前置 Plus 14（consistency #2）；刷新一律 rotate-cw、36 高图标按钮。

import { freshness } from './ui/layout.js';
import { segmented, button, iconButton } from './ui/controls.js';

export const HOME_VIEWS = [{ value: 'large', label: '大卡片' }, { value: 'small', label: '小卡片' }, { value: 'compact', label: '紧凑' }];
export const DENSITY_ITEMS = [{ value: 'comfortable', label: '宽松' }, { value: 'compact', label: '紧凑' }];

export const densitySeg = (value) => segmented({ name: 'density', items: DENSITY_ITEMS, value, label: '密度' });
export const refreshBtn = () => iconButton({ icon: 'rotate-cw', label: '刷新', size: 'lg', attrs: { 'data-act': 'refresh' } });

/** HEAD_TOOLS[page]({ density, view }) → 页头工具 HTML。 */
export const HEAD_TOOLS = {
  home: ({ view }) => freshness() + segmented({ name: 'view', role: 'tablist', items: HOME_VIEWS, value: view, label: '卡片视图' }),
  dashboard: ({ density }) => freshness() + densitySeg(density)
    + button({ label: '新增家宽节点', variant: 'primary', icon: 'plus', attrs: { 'data-act': 'enroll' } })
    + refreshBtn(),
  nodes: ({ density }) => freshness() + densitySeg(density)
    + button({ label: '新增家宽节点', variant: 'primary', icon: 'plus', attrs: { 'data-act': 'add' } })
    + button({ label: '导出', icon: 'download', attrs: { 'data-act': 'export' } })
    + refreshBtn(),
  lines: ({ density }) => freshness() + densitySeg(density)
    + button({ label: '新增线路机', variant: 'primary', icon: 'plus', attrs: { 'data-act': 'add-relay' } })
    + button({ label: '新增家宽节点', icon: 'plus', attrs: { 'data-act': 'add-node' } })
    + refreshBtn(),
  behavior: ({ density }) => freshness() + densitySeg(density)
    + button({ label: '导出', icon: 'download', attrs: { 'data-act': 'export' } })
    + button({ label: '更多', caret: true, attrs: { 'data-act': 'more', 'aria-haspopup': 'menu', 'aria-expanded': 'false' } })
    + refreshBtn(),
  onboarding: ({ density }) => freshness() + densitySeg(density)
    + button({ label: '新建批次', icon: 'plus', variant: 'primary', attrs: { 'data-act': 'new', 'aria-keyshortcuts': 'n' } })
    + refreshBtn(),
};
