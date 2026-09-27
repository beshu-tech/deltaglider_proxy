// /docs.md — the docs landing (docs/product/README.md) as markdown.
import type { APIRoute } from 'astro';
import { docMarkdown } from '../lib/llms';

export const GET: APIRoute = () =>
  new Response(docMarkdown('README'), { headers: { 'Content-Type': 'text/markdown; charset=utf-8' } });
