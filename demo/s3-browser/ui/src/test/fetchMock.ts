// Route-table fetch stub: tests mock HTTP at the network boundary, not the
// modules that call it. Unmatched requests fail the test loudly.
import { vi } from 'vitest';

export interface RecordedRequest {
  method: string;
  path: string;
  body: unknown;
  /** Request headers, names lower-cased. */
  headers: Record<string, string>;
}

type Reply = Response | (() => Response | Promise<Response>);
type Route = { method: string; path: string | RegExp; reply: (req: RecordedRequest) => Reply };

export function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } });
}

export function mockFetch() {
  const routes: Route[] = [];
  const calls: RecordedRequest[] = [];
  const spy = vi.fn(async (input: RequestInfo | URL, init: RequestInit = {}) => {
    const url = typeof input === 'string' ? input : input instanceof URL ? input.href : input.url;
    const path = url.replace(/^https?:\/\/[^/]+/, '');
    const method = (init.method ?? 'GET').toUpperCase();
    let body: unknown = init.body;
    if (typeof body === 'string') {
      try {
        body = JSON.parse(body);
      } catch {
        /* raw text body */
      }
    }
    const headers: Record<string, string> = {};
    new Headers(init.headers).forEach((v, k) => {
      headers[k] = v;
    });
    const req = { method, path, body, headers };
    calls.push(req);
    // Newest route first, so a test can override a default.
    for (let i = routes.length - 1; i >= 0; i--) {
      const r = routes[i];
      const hit = typeof r.path === 'string' ? r.path === path.split('?')[0] : r.path.test(path);
      if (r.method === method && hit) {
        const reply = r.reply(req);
        return typeof reply === 'function' ? reply() : reply.clone();
      }
    }
    throw new Error(`unmocked fetch: ${method} ${path}`);
  });
  vi.stubGlobal('fetch', spy);
  return {
    calls,
    on(method: string, path: string | RegExp, reply: Reply | ((req: RecordedRequest) => Reply)) {
      routes.push({
        method: method.toUpperCase(),
        path,
        reply: (req) => (typeof reply === 'function' && reply.length > 0 ? (reply as (r: RecordedRequest) => Reply)(req) : (reply as Reply)),
      });
      return this;
    },
    callsTo(method: string, path: string) {
      return calls.filter((c) => c.method === method.toUpperCase() && c.path.split('?')[0] === path);
    },
  };
}
