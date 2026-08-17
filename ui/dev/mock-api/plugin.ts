import type { IncomingMessage, ServerResponse } from 'node:http';
import type { Plugin } from 'vite';
import { dispatchMockRequest } from './dispatcher';
import { createFixtureState } from './fixtures';

function cookies(header: string | undefined): Record<string, string> {
  return Object.fromEntries(
    (header ?? '')
      .split(';')
      .map((part) => part.trim())
      .filter(Boolean)
      .map((part) => {
        const separator = part.indexOf('=');
        return [
          decodeURIComponent(part.slice(0, separator)),
          decodeURIComponent(part.slice(separator + 1)),
        ];
      })
  );
}

async function readBody(request: IncomingMessage): Promise<Uint8Array> {
  const chunks: Uint8Array[] = [];
  for await (const chunk of request)
    chunks.push(
      typeof chunk === 'string' ? new TextEncoder().encode(chunk) : chunk
    );
  const size = chunks.reduce((total, chunk) => total + chunk.byteLength, 0);
  const body = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) {
    body.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return body;
}

function send(
  response: ServerResponse,
  status: number,
  headers: Record<string, string> | undefined,
  body: unknown | Uint8Array | undefined
) {
  response.statusCode = status;
  Object.entries(headers ?? {}).forEach(([name, value]) =>
    response.setHeader(name, value)
  );
  if (body instanceof Uint8Array) response.end(body);
  else if (body === undefined) response.end();
  else response.end(JSON.stringify(body));
}

export function sqrzlMockApi(): Plugin {
  const state = createFixtureState();
  return {
    name: 'sqrzl-mock-api',
    enforce: 'pre',
    configureServer(server) {
      server.middlewares.use(async (request, response, next) => {
        if (!request.url?.startsWith('/admin/v1')) {
          next();
          return;
        }
        try {
          const body = await readBody(request);
          const headers = Object.fromEntries(
            Object.entries(request.headers).map(([name, value]) => [
              name,
              Array.isArray(value) ? value.join(', ') : value,
            ])
          );
          const contentType = request.headers['content-type'] ?? '';
          const json =
            body.byteLength && contentType.includes('application/json')
              ? JSON.parse(new TextDecoder().decode(body))
              : undefined;
          const result = dispatchMockRequest(
            {
              method: request.method ?? 'GET',
              url: request.url,
              headers,
              cookies: cookies(request.headers.cookie),
              json,
              body,
            },
            state
          );
          send(response, result.status, result.headers, result.body);
        } catch (caught) {
          send(
            response,
            500,
            { 'content-type': 'application/json; charset=utf-8' },
            {
              code: 'mock_internal_error',
              error:
                caught instanceof Error
                  ? caught.message
                  : 'Mock request failed',
              details: null,
            }
          );
        }
      });
    },
  };
}
