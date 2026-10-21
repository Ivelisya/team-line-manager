// 复制到剪贴板。http（非安全上下文）下 navigator.clipboard 不可用，退回 execCommand。
import { toast } from '@/components/ui/toast';

export async function writeClipboard(text: string): Promise<boolean> {
  try {
    if (navigator.clipboard && window.isSecureContext) { await navigator.clipboard.writeText(text); return true; }
  } catch { /* 退回旧方法 */ }
  try {
    const ta = document.createElement('textarea');
    ta.value = text; ta.setAttribute('readonly', ''); ta.style.cssText = 'position:fixed;top:-1000px;opacity:0';
    document.body.appendChild(ta); ta.select();
    const ok = document.execCommand('copy');
    ta.remove();
    return ok;
  } catch { return false; }
}

/** 复制并提示：copyText('1.2.3.4', 'IP') → “已复制 IP” */
export async function copyText(text: string, label = '内容'): Promise<boolean> {
  const ok = await writeClipboard(text);
  if (ok) toast.ok(`已复制${label}`); else toast.err('复制失败，请手动选择文本');
  return ok;
}

/** 复制分享链接：提示与管理页的分享对话框一致（取链接会记审计，链接等同凭据） */
export async function copyShareLink(url: string, label = '分享链接'): Promise<boolean> {
  const ok = await writeClipboard(url);
  if (ok) toast.show({ tone: 'ok', title: `已复制${label}`, text: '已记入审计；链接等同凭据，只发给需要的人' });
  else toast.err('复制失败，请手动选择文本');
  return ok;
}
