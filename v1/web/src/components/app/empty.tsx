import * as React from 'react';
import { Inbox } from 'lucide-react';
import { Empty as EmptyRoot, EmptyContent, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle } from '@/components/ui/empty';

// 单独成文件：客户页只要它，不必为此带上 primitives 里的一大堆（primitives 重新导出它，其余页面照旧从那里取）
/** 空状态（shadcn Empty 的薄封装）：图标放在柔和的圆底里，一句话说明现状；hint 是一行小字；children 放下一步操作 */
export function Empty({ icon, text, hint, children }: { icon?: React.ReactNode; text: string; hint?: React.ReactNode; children?: React.ReactNode }) {
  return (
    <EmptyRoot>
      <EmptyHeader>
        <EmptyMedia variant="icon">{icon ?? <Inbox size={22} />}</EmptyMedia>
        <EmptyTitle>{text}</EmptyTitle>
        {hint && <EmptyDescription>{hint}</EmptyDescription>}
      </EmptyHeader>
      {children && <EmptyContent>{children}</EmptyContent>}
    </EmptyRoot>
  );
}
