import { cleanupApp, createSPA } from '@askrjs/askr/boot';
import { createRouteRegistry, route } from '@askrjs/askr/router';
import { afterEach, expect, it } from 'vite-plus/test';
import SesEventsPanel from '@/features/messages/components/ses-events-panel';

const originalFetch = globalThis.fetch;
let root: HTMLDivElement;
afterEach(() => {
  if (root) {
    cleanupApp(root);
    root.remove();
  }
  globalThis.fetch = originalFetch;
});
const flush = () => new Promise((resolve) => setTimeout(resolve, 25));
const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), {
    status,
    headers: { 'content-type': 'application/json' },
  });
async function mount() {
  root = document.createElement('div');
  document.body.appendChild(root);
  window.history.pushState(null, '', '/');
  await createSPA({
    root,
    registry: createRouteRegistry(() =>
      route('/', () => (
        <SesEventsPanel messageId="ses-1" configurationSet="test-events" />
      ))
    ),
  });
  await flush();
}
function button(label: string) {
  const found = Array.from(root.querySelectorAll('button')).find(
    (b) => b.textContent?.trim() === label
  );
  expect(found).toBeTruthy();
  return found!;
}
it('shows pending callback uncertainty and never automatically resends it', async () => {
  let writes = 0;
  globalThis.fetch = async (input, init) => {
    const request = new Request(input, init);
    if (request.method !== 'GET') writes++;
    return request.url.includes('/ses-events?')
      ? json({
          items: [
            {
              event_id: 'intent-1',
              message_id: 'ses-1',
              event_type: 'Delivery',
              state: 'pending',
              created_at: '2026-11-14T12:00:00Z',
              callback: {
                state: 'pending',
                url: 'http://localhost:9999',
                attempted_at: '2026-11-14T12:00:00Z',
                request_body: '{}',
                request_headers: {},
              },
              notification: {},
              record: {},
            },
          ],
        })
      : json({ error: 'No destination' }, 404);
  };
  await mount();
  expect(root.textContent).toContain('Delivery status is uncertain');
  expect(root.textContent).toContain('intent-1');
  button('Refresh events').click();
  await flush();
  expect(writes).toBe(0);
});
it('retains the same request identity after a lost simulation response', async () => {
  const requests: Record<string, unknown>[] = [];
  globalThis.fetch = async (input, init) => {
    const request = new Request(input, init);
    if (request.method === 'POST') {
      requests.push(await request.json());
      if (requests.length === 1) throw new TypeError('Lost response');
      return json({ event_id: 'event-1', state: 'completed' });
    }
    return request.url.includes('/ses-events?')
      ? json({ items: [] })
      : json({ error: 'No destination' }, 404);
  };
  await mount();
  button('Simulate event').click();
  await flush();
  expect(root.textContent).toContain('Lost response');
  button('Retry same event').click();
  await flush();
  expect(requests).toHaveLength(2);
  expect(requests[0]).toEqual(requests[1]);
  expect(requests[0].request_id).toBeTruthy();
  expect(requests[0].message_id).toBe('ses-1');
});

it('loads the next history page without retaining old payloads and can return', async () => {
  const queried: string[] = [];
  globalThis.fetch = async (input, init) => {
    const request = new Request(input, init);
    if (!request.url.includes('/ses-events?'))
      return json({ error: 'No destination' }, 404);
    const next = new URL(request.url).searchParams.get('next');
    queried.push(next ?? 'first');
    return json({
      items: [
        {
          event_id: next ? 'second-event' : 'first-event',
          message_id: 'ses-1',
          event_type: 'Delivery',
          state: 'completed',
          created_at: '2026-11-14T12:00:00Z',
          record: {},
          notification: {},
        },
      ],
      next: next ? null : 'opaque-next',
    });
  };
  await mount();
  expect(root.textContent).toContain('first-event');
  button('Next events').click();
  await flush();
  expect(root.textContent).toContain('second-event');
  expect(root.textContent).not.toContain('first-event');
  button('Previous events').click();
  await flush();
  expect(root.textContent).toContain('first-event');
  expect(queried).toEqual(['first', 'opaque-next', 'first']);
});
