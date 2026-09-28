/**
 * Every docs shot, one file per docs area, so writers of different areas
 * add shots without touching the same file. Order here is capture order.
 */
import type { Shot } from '../shot';
import { ACCESS_SHOTS } from './access';
import { BROWSER_SHOTS } from './browser';
import { JOBS_SHOTS } from './jobs';
import { OBSERVABILITY_SHOTS } from './observability';
import { STORAGE_SHOTS } from './storage';
import { SYSTEM_SHOTS } from './system';

export const SHOTS: Shot[] = [
  ...STORAGE_SHOTS,
  ...ACCESS_SHOTS,
  ...JOBS_SHOTS,
  ...OBSERVABILITY_SHOTS,
  ...SYSTEM_SHOTS,
  ...BROWSER_SHOTS,
];
