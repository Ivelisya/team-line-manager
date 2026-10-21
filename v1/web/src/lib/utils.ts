import { clsx } from 'clsx';

// 组件的基础类都是自定义类（btn、menu-item…），调用处只补少量宽度类，不会和基础类冲突：用不着 tailwind-merge
export const cn = clsx;
