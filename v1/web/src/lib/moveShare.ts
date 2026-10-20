// 换线路机完成后，客户端要换成新的分享链接：按作业记下的新入口 id 取链接并复制。
import { get } from './api';
import { fetchShare } from './queries';
import { copyText } from './clipboard';
import type { Job } from './types';
import { toast } from '@/components/ui/toast';

export async function copyNewShareLink(jobId: string) {
  try {
    const j = await get<Job>(`/jobs/${jobId}`);
    const id = j.result?.new_entry_id ?? (j.steps.find((s) => s.key === 'attach_new')?.output?.entry_id as string | undefined);
    if (!id) throw new Error('找不到这次换线路机建的新入口');
    const r = await fetchShare(id);
    await copyText(r.url, '新分享链接');
  } catch (e) { toast.err(e); }
}
