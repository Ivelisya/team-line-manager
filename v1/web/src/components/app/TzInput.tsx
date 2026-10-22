// 时区输入（订阅的账单时区、供应商时区共用）：下拉提示 /meta 的常用时区，也能直接填别的 IANA 名（中控按时区库校验）
import * as React from 'react';
import { Input } from '@/components/ui/form';
import { meta } from '@/lib/derive';

export function TzInput({ value, onChange, placeholder, 'aria-label': ariaLabel }: { value: string; onChange: (v: string) => void; placeholder?: string; 'aria-label'?: string }) {
  const id = React.useId();
  return (
    <>
      <Input list={id} value={value} onChange={(e) => onChange(e.target.value.trim())} placeholder={placeholder} aria-label={ariaLabel} spellCheck={false} autoComplete="off" />
      <datalist id={id}>{meta().timezones.map((z) => <option key={z} value={z} />)}</datalist>
    </>
  );
}
