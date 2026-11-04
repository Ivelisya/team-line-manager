import { Info } from 'lucide-react';
import { cn } from '@/lib/utils';

/** 「(i)」：悬停 / 聚焦时显示的补充说明（走全站 data-tip，由 TipHost 弹出）。界面上只留必须知道的短字，解释放这里（用户：说明性的小字太多）。
 *  单独成文件：客户页只要它，不必为此带上 form.tsx 里的输入组、复选框、开关 */
export function InfoTip({ text, className }: { text: string; className?: string }) {
  return <span className={cn('info-tip', className)} tabIndex={0} role="img" aria-label={text} data-tip={text}><Info /></span>;
}
