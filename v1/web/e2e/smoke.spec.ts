// 交互冒烟（pnpm e2e）：原 tools/smoke.mjs 的九步，换成 Playwright 跑（mock 模式的生产构建 + vite preview，见 playwright.config.ts）。
// 退役确认流程（默认「移入备用」预览改名，再切到「立即清理」输入显示名）、批量上架（粘贴 → 检查 → 唯一确认 → 作业）、
// 对话框里提示开着时 Esc 的先后、数据保留页的并排表单项。页面有报错（pageerror / console.error）也算失败。
// 选择器按角色 / 文字写，不依赖 .dialog、.seg-item 这类样式类名（组件换底层后类名会变）。mock 数据在内存里，整页跳转会重置，每个用例各自从头开始
import { expect, test, type Page } from '@playwright/test';

const errors: string[] = [];
test.beforeEach(({ page }) => {
  errors.length = 0;
  page.on('pageerror', (e) => errors.push(String(e)));
  page.on('console', (m) => { if (m.type() === 'error') errors.push(m.text()); });
});
test.afterEach(() => { expect(errors, '页面错误').toEqual([]); });

/** 最上层的对话框：页面上可能还有一个非模态的抽屉也是 role=dialog，后挂载的在最上面 */
const topDialog = (page: Page) => page.locator('[role="dialog"]:not([data-slot="toast"])').last();
const dialogCount = (page: Page) => page.locator('[role="dialog"]:not([data-slot="toast"])').count();
/** 点文字包含 text 的 sel（同原脚本的 clickText：取第一个） */
const clickText = (scope: Page | ReturnType<Page['locator']>, sel: string, text: string) => scope.locator(sel, { hasText: text }).first().click();

test('退役：默认移入备用并预览改名；切到立即清理要输入显示名，提交后有提示', async ({ page }) => {
  await page.goto('/nodes?m=m-t1-03&tab=manage', { waitUntil: 'networkidle' });
  await clickText(page, 'button', '退役…');
  const d = topDialog(page);
  await expect(d).toContainText('团队1-04');
  await expect(d, '退役预览含改名').toContainText('团队1-03');
  await page.screenshot({ path: 'shots/smoke-retire-dialog.png' });
  await expect(d.locator('label', { hasText: '请输入' }), '默认去向是移入备用（不要求输入）').toHaveCount(0);
  await clickText(d, 'label', '立即清理并移入回收站');
  await d.getByLabel(/请输入显示名/).fill('团队1-03');
  await clickText(d, 'button', '清理并回收');
  await expect(page.locator('body'), '退役已提交').toContainText('已提交清理作业');
});

test('批量上架：粘贴 → 检查通过 → 唯一确认 → 已创建作业', async ({ page }) => {
  await page.goto('/provision', { waitUntil: 'networkidle' });
  await clickText(page, '[role="radio"], [role="tab"], button', '批量');
  // 等批量表格出来再粘（切模式走路由的 transition，单台表单上也有一个「类型」分段控件）：表格里的是原生下拉
  await page.locator('select[aria-label="类型"]').first().evaluate((input) => {
    (input as HTMLElement).focus();
    const dt = new DataTransfer();
    dt.setData('text', 'VPS 节点\t198.51.100.150\t\t团队2\t\t\t线路机2\nVPS 节点\t198.51.100.151\t\t团队2\t\t\t线路机2\n线路机\t203.0.113.60\t\t\t\t线路机3');
    input.dispatchEvent(new ClipboardEvent('paste', { clipboardData: dt, bubbles: true, cancelable: true }));
  });
  await clickText(page, 'button', '检查');
  await expect(page.locator('body'), '检查通过提示').toContainText('检查通过');
  await page.screenshot({ path: 'shots/smoke-provision-checked.png' });
  await clickText(page, 'button', '开始上架');
  await page.screenshot({ path: 'shots/smoke-provision-confirm.png' });
  await clickText(topDialog(page), 'button', '确认上架');
  await expect(page.locator('body'), '已创建作业').toContainText('已开始上架');
  await page.screenshot({ path: 'shots/smoke-provision-created.png' });
});

test('对话框里提示开着时按 Esc：第一下只收起提示，第二下才关对话框', async ({ page }) => {
  await page.goto('/routers', { waitUntil: 'networkidle' });
  await clickText(page, 'button', '接入软路由');
  await expect(topDialog(page)).toBeVisible();
  await page.keyboard.type('smoke-esc');
  await topDialog(page).locator('[role="img"][tabindex="0"]').first().focus();
  const tip = page.locator('[data-slot="tooltip-content"][data-open]');
  await expect(tip, '键盘聚焦「(i)」出现提示').toHaveCount(1);
  await page.keyboard.press('Escape');
  await expect(tip, '第一下 Esc 收起提示').toHaveCount(0);
  expect(await dialogCount(page), '对话框还在').toBe(1);
  await expect(topDialog(page).locator('input').first(), '已填的内容还在').toHaveValue('smoke-esc');
  await page.keyboard.press('Escape');
  await expect.poll(() => dialogCount(page), { message: '第二下 Esc 关掉对话框' }).toBe(0);
});

test('数据保留：两个天数字段在同一行（表单项不撑满整行）', async ({ page }) => {
  await page.goto('/settings?section=retention', { waitUntil: 'networkidle' });
  const tops = await page.locator('[data-slot="field"]').evaluateAll((fs) => fs.slice(0, 2).map((f) => Math.round(f.getBoundingClientRect().top)));
  expect(tops).toHaveLength(2);
  expect(tops[0], '两个天数字段并排').toBe(tops[1]);
});

test('地址参数打开：?silence= 打开新建静默、?host= 预填宿主机，参数用过就从地址里删掉', async ({ page }) => {
  await page.goto('/incidents?silence=new', { waitUntil: 'networkidle' });
  await expect(topDialog(page)).toContainText('静默');
  await expect(page).not.toHaveURL(/silence=/);
  await page.keyboard.press('Escape');
  await expect(topDialog(page)).toHaveCount(0);
  await page.goto('/provision?host=m-t1-03', { waitUntil: 'networkidle' });
  await expect(page.getByRole('switch', { name: '加在已有机器上' })).toBeChecked();
  await expect(page).not.toHaveURL(/host=/);
  await expect(page).toHaveURL(/mode=single/);
});
