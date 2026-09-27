// /llms.txt — the docs index for LLMs (https://llmstxt.org). See lib/llms.ts.
import type { APIRoute } from 'astro';
import { buildLlmsTxt } from '../lib/llms';

export const GET: APIRoute = () =>
  new Response(buildLlmsTxt(), { headers: { 'Content-Type': 'text/plain; charset=utf-8' } });
