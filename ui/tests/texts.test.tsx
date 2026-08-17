import { cleanupApp, createSPA } from '@askrjs/askr/boot';
import { createRouteRegistry, route } from '@askrjs/askr/router';
import { describe, expect, it } from 'vite-plus/test';
import TextMessagePage from '@/pages/app/texts/message';
import TextConversationPage from '@/pages/app/texts/conversation';
import TextsPage from '@/pages/app/texts';

const originalFetch = globalThis.fetch;

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'content-type': 'application/json' },
  });
}

async function flush(): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, 0));
  await new Promise((resolve) => setTimeout(resolve, 0));
}

async function mount(component: any, path = '/'): Promise<HTMLDivElement> {
  const registry = createRouteRegistry(() => route(path, component));
  const root = document.createElement('div');
  document.body.appendChild(root);
  window.history.pushState(null, '', path);
  await createSPA({ root, registry });
  return root;
}

describe('texts UI', () => {
  it('expands text message details beneath the selected row', async () => {
    const message = {
      message_id: 'txt-inline',
      provider_message_id: 'SM-inline',
      batch_id: null,
      provider: 'twilio',
      direction: 'inbound',
      channel: 'sms',
      from: '+15550000002',
      to: '+15550000001',
      peer: '+15550000002',
      body: 'Inline text body',
      media: [],
      metadata: {},
      delivery_state: 'delivered',
      created_at: '2026-08-17T14:00:00Z',
      updated_at: '2026-08-17T14:00:00Z',
    };
    globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
      const request =
        typeof input === 'string' || input instanceof URL
          ? new Request(input, init)
          : input;
      const path = new URL(request.url, 'http://localhost').pathname;
      return path.endsWith('/messages')
        ? jsonResponse({ items: [message], next: null })
        : jsonResponse({ ...message, callback_attempts: [] });
    };
    const root = await mount(() => (
      <TextConversationPage peer="+15550000002" />
    ));
    try {
      await flush();
      const expand = root.querySelector(
        '[aria-label="Expand text message txt-inline"]'
      ) as HTMLButtonElement;
      expect(expand).toBeTruthy();
      expand.click();
      await flush();
      expect(
        root.querySelector('#text-message-inline-txt-inline')
      ).toBeTruthy();
      expect(root.textContent).toContain('Provider message ID');
      expect(
        root
          .querySelector('[aria-label="Collapse text message txt-inline"]')
          ?.getAttribute('aria-expanded')
      ).toBe('true');
    } finally {
      cleanupApp(root);
      root.remove();
      globalThis.fetch = originalFetch;
    }
  });

  it('renders populated conversations and opens simulation and destructive dialogs', async () => {
    globalThis.fetch = async () =>
      jsonResponse({
        items: [
          {
            peer: '+15551234567',
            message_count: 2,
            last_message_at: '2026-08-07T10:00:00Z',
            last_message_body: 'Hello from Twilio',
            last_direction: 'inbound',
            provider: 'twilio',
          },
        ],
        next: null,
      });
    const root = await mount(() => <TextsPage />);
    try {
      await flush();
      expect(root.textContent).toContain('+15551234567');
      expect(root.textContent).toContain('Hello from Twilio');
      const simulateButton = Array.from(root.querySelectorAll('button')).find(
        (button) => button.textContent?.includes('Simulate inbound')
      ) as HTMLButtonElement;
      simulateButton.click();
      await flush();
      expect(document.body.textContent).toContain('Simulate inbound text');
      const cancelButton = Array.from(
        document.body.querySelectorAll('button')
      ).find(
        (button) => button.textContent?.trim() === 'Cancel'
      ) as HTMLButtonElement;
      cancelButton.click();
      await flush();

      const deleteButton = root.querySelector(
        '[aria-label="Delete text conversation +15551234567"]'
      ) as HTMLButtonElement;
      deleteButton.click();
      await flush();
      expect(document.body.textContent).toContain('Delete conversation');
    } finally {
      cleanupApp(root);
      root.remove();
      globalThis.fetch = originalFetch;
    }
  });

  it('renders delivery controls, media choices, and failed callback retry detail', async () => {
    globalThis.fetch = async () =>
      jsonResponse({
        message_id: 'txt-1',
        provider_message_id: 'SM1',
        batch_id: null,
        provider: 'twilio',
        direction: 'outbound',
        channel: 'mms',
        from: '+15550000001',
        to: '+15550000002',
        peer: '+15550000002',
        body: 'Picture attached',
        media: [
          {
            media_id: 'media-local',
            filename: 'photo.jpg',
            content_type: 'image/jpeg',
            size: 3,
          },
          {
            media_id: 'media-external',
            filename: 'remote.jpg',
            content_type: 'image/jpeg',
            external_url: 'https://example.invalid/remote.jpg',
          },
        ],
        metadata: {},
        delivery_state: 'accepted',
        created_at: '2026-08-07T10:00:00Z',
        updated_at: '2026-08-07T10:00:00Z',
        callback_attempts: [
          {
            attempt_id: 'attempt-1',
            message_id: 'txt-1',
            kind: 'delivery',
            provider: 'twilio',
            url: 'http://127.0.0.1:8080/status',
            request_headers: {},
            request_body: 'MessageStatus=failed',
            state: 'failed',
            error: 'connection refused',
            attempted_at: '2026-08-07T10:01:00Z',
          },
        ],
      });
    const root = await mount(
      () => <TextMessagePage peer="+15550000002" messageId="txt-1" />,
      '/admin/text/%2B15550000002/txt-1'
    );
    try {
      await flush();
      expect(root.textContent).toContain('Mark delivered');
      expect(root.textContent).toContain('Mark failed');
      expect(root.textContent).toContain('photo.jpg');
      expect(root.textContent).toContain('Open external media');
      expect(root.textContent).toContain('connection refused');
      expect(root.textContent).toContain('Retry');
    } finally {
      cleanupApp(root);
      root.remove();
      globalThis.fetch = originalFetch;
    }
  });
});
