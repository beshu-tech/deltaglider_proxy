// /docs/<slug>.md — one doc's markdown, next to its HTML page. See lib/llms.ts.
import type { APIRoute, GetStaticPaths } from 'astro';
import { DOCS } from '../../lib/docs';
import { docMarkdown } from '../../lib/llms';

export const getStaticPaths: GetStaticPaths = () =>
  DOCS.filter((d) => d.slug !== '').map((d) => ({ params: { slug: d.slug }, props: { path: d.path } }));

export const GET: APIRoute = ({ props }) =>
  new Response(docMarkdown((props as { path: string }).path), {
    headers: { 'Content-Type': 'text/markdown; charset=utf-8' },
  });
