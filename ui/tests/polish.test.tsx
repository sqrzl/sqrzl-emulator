import { cleanupApp, createSPA } from '@askrjs/askr/boot';
import { afterEach, describe, expect, it } from 'vite-plus/test';
import { routeRegistry } from '@/pages/_routes';
import { adminApi } from '@/adapters';

const originalFetch = globalThis.fetch;
let root: HTMLDivElement | undefined;

afterEach(() => {
  if (root) {
    cleanupApp(root);
    root.remove();
    root = undefined;
  }
  globalThis.fetch = originalFetch;
});

async function open(path: string, failData = false): Promise<HTMLDivElement> {
  globalThis.fetch = async (input: RequestInfo | URL) => {
    const url =
      typeof input === 'string'
        ? input
        : input instanceof URL
          ? input.href
          : input.url;
    if (failData && !url.endsWith('/auth/session'))
      return new Response(JSON.stringify({ error: 'Injected failure' }), {
        status: 500,
        headers: { 'content-type': 'application/json' },
      });
    return new Response(
      JSON.stringify(
        url.endsWith('/auth/session')
          ? { mode: 'open', username: null }
          : { items: [], folders: [], next: null }
      ),
      {
        headers: { 'content-type': 'application/json' },
      }
    );
  };
  root = document.createElement('div');
  document.body.appendChild(root);
  window.history.pushState(null, '', path);
  await createSPA({ root, registry: routeRegistry });
  await new Promise((resolve) => setTimeout(resolve, 25));
  return root;
}

describe('production admin journeys', () => {
  it('enters buckets from the home page when runtime authentication is open', async () => {
    const page = await open('/');
    expect(page.textContent).toContain('Search buckets and open one');
    expect(page.querySelector('input[type="password"]')).toBeNull();
  });

  it('renders one main landmark on a bucket page', async () => {
    const page = await open('/admin/buckets');
    expect(page.querySelectorAll('main')).toHaveLength(1);
  });

  it('renders an actionable fallback for an unknown route', async () => {
    const page = await open('/does-not-exist');
    expect(page.textContent).toContain('Page not found');
    expect(page.querySelectorAll('main')).toHaveLength(1);
    expect(page.querySelector('a[href="/"]')).not.toBeNull();
  });

  it('retains the page landmark when a blob cannot be resolved', async () => {
    const page = await open('/admin/blobs/example/unknown');
    expect(page.textContent).toContain('Blob not found');
    expect(page.querySelectorAll('main')).toHaveLength(1);
  });

  it('offers a retry when a bucket list request fails', async () => {
    const page = await open('/admin/buckets', true);
    expect(
      Array.from(page.querySelectorAll('button')).some(
        (button) => button.textContent?.trim() === 'Retry'
      )
    ).toBe(true);
  });

  it('binds SES event operations to encoded paths, bodies and queries', async () => {
    const requests: Array<{ path: string; method: string; body: string }> = [];
    globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
      const request =
        input instanceof Request ? input : new Request(input, init);
      const url = new URL(request.url);
      requests.push({
        path: url.pathname + url.search,
        method: request.method,
        body: await request.text(),
      });
      return request.method === 'DELETE'
        ? new Response(null, { status: 204 })
        : new Response('{}', {
            headers: { 'content-type': 'application/json' },
          });
    };
    await adminApi.getSesEventDestination('events/name');
    await adminApi.putSesEventDestination('events/name', {
      callback_url: 'http://localhost/events',
    });
    await adminApi.deleteSesEventDestination('events/name');
    await adminApi.listSesEvents({ message_id: 'mail id' });
    await adminApi.simulateSesEvent({
      request_id: 'stable-request',
      message_id: 'mail id',
      event_type: 'Delivery',
    });
    expect(requests.map(({ path, method }) => `${method} ${path}`)).toEqual([
      'GET /admin/v1/ses-event-destinations/events%2Fname',
      'PUT /admin/v1/ses-event-destinations/events%2Fname',
      'DELETE /admin/v1/ses-event-destinations/events%2Fname',
      'GET /admin/v1/ses-events?message_id=mail+id',
      'POST /admin/v1/ses-events',
    ]);
    expect(JSON.parse(requests[4].body).request_id).toBe('stable-request');
  });
});
