// @vitest-environment node
import { describe, expect, test } from 'vitest';
import { dispatchMockRequest } from '../dev/mock-api/dispatcher';
import { createFixtureState } from '../dev/mock-api/fixtures';
import type { MockRequest } from '../dev/mock-api/types';

function harness() {
  const state = createFixtureState();
  let cookie = '';
  const call = (
    method: string,
    url: string,
    options: Partial<MockRequest> = {}
  ) => {
    const result = dispatchMockRequest(
      {
        method,
        url,
        cookies: cookie ? { sqrzl_admin_session: cookie } : {},
        ...options,
      },
      state
    );
    const issued = result.headers?.['set-cookie']?.match(
      /^sqrzl_admin_session=([^;]*)/u
    )?.[1];
    if (issued !== undefined) cookie = issued;
    return result;
  };
  const login = () =>
    call('POST', '/admin/v1/auth/login', {
      json: { username: 'admin', password: 'sqrzl-secret' },
    });
  return { state, call, login, cookie: () => cookie };
}

describe('mock admin API', () => {
  test('rejects invalid login, issues a session cookie, protects access, and logs out', () => {
    const api = harness();
    expect(api.call('GET', '/admin/v1/buckets').status).toBe(401);
    expect(
      api.call('POST', '/admin/v1/auth/login', {
        json: { username: 'admin', password: 'wrong' },
      }).status
    ).toBe(401);
    expect(api.login().headers?.['set-cookie']).toContain(
      'sqrzl_admin_session='
    );
    expect(api.call('GET', '/admin/v1/auth/session').body).toEqual({
      mode: 'session',
      username: 'admin',
    });
    expect(api.call('POST', '/admin/v1/auth/logout').status).toBe(200);
    expect(api.call('GET', '/admin/v1/buckets').status).toBe(401);
  });

  test('paginates and searches with opaque cursors and projects nested folders', () => {
    const api = harness();
    api.login();
    const first = api.call('GET', '/admin/v1/buckets').body as {
      items: unknown[];
      next: string;
    };
    expect(first.items).toHaveLength(3);
    expect(first.next).toBe('mock:3');
    expect(
      (
        api.call(
          'GET',
          `/admin/v1/buckets?next=${encodeURIComponent(first.next)}`
        ).body as { items: unknown[] }
      ).items
    ).toHaveLength(2);
    expect(
      (
        api.call('GET', '/admin/v1/buckets?search=archive').body as {
          items: Array<{ name: string }>;
        }
      ).items.map((x) => x.name)
    ).toEqual(['versioned-archive']);
    const root = api.call('GET', '/admin/v1/buckets/assets/objects').body as {
      folders: Array<{ prefix: string }>;
    };
    expect(root.folders.map((x) => x.prefix)).toContain('brand/');
    const nested = api.call(
      'GET',
      '/admin/v1/buckets/assets/objects?prefix=brand%2F'
    ).body as {
      folders: Array<{ prefix: string }>;
      items: Array<{ key: string }>;
    };
    expect(nested.items.map((x) => x.key)).toContain('brand/logo.svg');
    expect(nested.folders.map((x) => x.prefix)).toContain('brand/icons/');
  });

  test('creates, uploads, overwrites, downloads, and deletes storage state', () => {
    const api = harness();
    api.login();
    expect(
      api.call('POST', '/admin/v1/buckets', { json: { name: 'uploads' } })
        .status
    ).toBe(201);
    expect(
      api.call('POST', '/admin/v1/buckets', { json: { name: 'uploads' } })
        .status
    ).toBe(409);
    const path = '/admin/v1/buckets/uploads/objects/folder%2Fhello.txt/content';
    expect(
      api.call('PUT', path, {
        headers: { 'content-type': 'text/plain', 'x-amz-meta-owner': 'mock' },
        body: new TextEncoder().encode('hello'),
      }).status
    ).toBe(201);
    const downloaded = api.call('GET', path);
    expect(new TextDecoder().decode(downloaded.body as Uint8Array)).toBe(
      'hello'
    );
    expect(downloaded.headers?.['content-type']).toBe('text/plain');
    expect(
      api.call('PUT', path, {
        headers: { 'content-type': 'text/plain' },
        body: new TextEncoder().encode('updated'),
      }).status
    ).toBe(200);
    expect(api.call('DELETE', '/admin/v1/buckets/uploads').status).toBe(409);
    expect(
      api.call('DELETE', '/admin/v1/buckets/uploads/objects/folder%2Fhello.txt')
        .status
    ).toBe(200);
    expect(api.call('DELETE', '/admin/v1/buckets/uploads').status).toBe(200);
    expect(api.call('GET', '/admin/v1/buckets/uploads').status).toBe(404);
  });

  test('downloads and deletes mail content and attachments', () => {
    const api = harness();
    api.login();
    const raw = api.call(
      'GET',
      '/admin/v1/mailboxes/demo%40sqrzl.test/messages/smtp-1/content'
    );
    expect(raw.headers?.['content-type']).toBe('message/rfc822');
    expect(new TextDecoder().decode(raw.body as Uint8Array)).toContain(
      'Welcome to Sqrzl'
    );
    const attachment = api.call(
      'GET',
      '/admin/v1/mailboxes/demo%40sqrzl.test/messages/smtp-1/attachments/walkthrough.txt'
    );
    expect(attachment.body).toBeInstanceOf(Uint8Array);
    expect(
      api.call(
        'DELETE',
        '/admin/v1/mailboxes/demo%40sqrzl.test/messages/smtp-1'
      ).status
    ).toBe(200);
    expect(
      api.call('GET', '/admin/v1/mailboxes/demo%40sqrzl.test/messages/smtp-1')
        .status
    ).toBe(404);
  });

  test('simulates inbound text, stores destinations, transitions, retries, downloads media, and deletes conversations', () => {
    const api = harness();
    api.login();
    expect(
      api.call('PUT', '/admin/v1/text-destinations/twilio/%2B15557654321', {
        json: { callback_url: 'http://127.0.0.1:8080/texts' },
      }).status
    ).toBe(201);
    const created = api.call('POST', '/admin/v1/text-simulations/inbound', {
      json: {
        provider: 'twilio',
        from: '+15550009999',
        to: '+15557654321',
        body: 'hello',
        media: [
          {
            filename: 'note.txt',
            content_type: 'text/plain',
            content_base64: 'bm90ZQ==',
          },
        ],
      },
    }).body as {
      message_id: string;
      callback_attempts: Array<{ attempt_id: string }>;
      media: Array<{ media_id: string }>;
    };
    expect(created.callback_attempts).toHaveLength(1);
    const attempt = api.call(
      'POST',
      `/admin/v1/text-callback-attempts/${created.callback_attempts[0].attempt_id}/retry`
    );
    expect((attempt.body as { retry_of: string }).retry_of).toBe(
      created.callback_attempts[0].attempt_id
    );
    const media = api.call(
      'GET',
      `/admin/v1/text-conversations/%2B15550009999/messages/${created.message_id}/media/${created.media[0].media_id}`
    );
    expect(new TextDecoder().decode(media.body as Uint8Array)).toBe('note');
    expect(
      api.call('POST', '/admin/v1/text-messages/txt-2/delivery', {
        json: { state: 'delivered' },
      }).status
    ).toBe(200);
    expect(
      api.call('DELETE', '/admin/v1/text-conversations/%2B15550009999').status
    ).toBe(200);
  });

  test('returns a structured 404 for every unknown mock API route', () => {
    const api = harness();
    api.login();
    const response = api.call('GET', '/admin/v1/not-a-real-route');
    expect(response.status).toBe(404);
    expect(response.body).toEqual(
      expect.objectContaining({ code: 'mock_route_not_found' })
    );
  });
});
