// /llms-full.txt — every doc's markdown in one file. See lib/llms.ts.
import type { APIRoute } from 'astro';
import { buildLlmsFull } from '../lib/llms';

export const GET: APIRoute = () =>
  new Response(buildLlmsFull(), { headers: { 'Content-Type': 'text/plain; charset=utf-8' } });
