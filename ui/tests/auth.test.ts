import { getManifest } from '@askrjs/askr/router';
import { describe, expect, it } from 'vite-plus/test';
import {
  loginAdminSession,
  logoutAdminSession,
  resolveAdminSession,
} from '../src/features/auth/admin-session';
import '../src/pages/_routes';

const originalFetch = globalThis.fetch;

function response(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'content-type': 'application/json' },
  });
}

function installAuthMock(status = 200): string[] {
  const requests: string[] = [];
  globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
    const request =
      typeof input === 'string' || input instanceof URL
        ? new Request(input, init)
        : input;
    const url = new URL(request.url, 'http://localhost');
    requests.push(`${request.method} ${url.pathname}`);

    if (url.pathname.endsWith('/auth/session')) {
      return status === 401
        ? response(
            { code: 'Unauthorized', error: 'Authentication required' },
            401
          )
        : response({ mode: 'session', username: 'admin-key' });
    }

    return response({ success: true });
  };
  return requests;
}

describe('admin authentication', () => {
  it('resolves authenticated sessions and uses generated login/logout operations', async () => {
    const requests = installAuthMock();

    try {
      const resolved = await resolveAdminSession({
        signal: new AbortController().signal,
      });
      await loginAdminSession({ username: 'admin-key', password: 'secret' });
      await logoutAdminSession();

      expect(resolved.session?.mode).toBe('session');
      expect(resolved.user?.name).toBe('admin-key');
      expect(requests).toContain('GET /admin/v1/auth/session');
      expect(requests).toContain('POST /admin/v1/auth/login');
      expect(requests).toContain('POST /admin/v1/auth/logout');
    } finally {
      globalThis.fetch = originalFetch;
    }
  });

  it('treats unauthorized resolution as signed out and guards simplified app routes', async () => {
    installAuthMock(401);

    try {
      const resolved = await resolveAdminSession({
        signal: new AbortController().signal,
      });
      const manifest = getManifest();
      const app = manifest.records.find(
        (record) => record.path === '/admin/buckets'
      );
      const bucket = manifest.records.find(
        (record) => record.path === '/admin/buckets/{bucketName}'
      );
      const deepBucket = manifest.records.find(
        (record) => record.path === '/admin/buckets/{bucketName}/*'
      );
      const blob = manifest.records.find(
        (record) => record.path === '/admin/blobs/{bucketName}/{blobId}'
      );
      const login = manifest.records.find((record) => record.path === '/login');
      const logout = manifest.records.find(
        (record) => record.path === '/logout'
      );

      expect(resolved.session).toBe(null);
      expect(app?.options.policies?.length).toBeGreaterThan(0);
      expect(bucket?.options.policies?.length).toBeGreaterThan(0);
      expect(deepBucket?.options.policies?.length).toBeGreaterThan(0);
      expect(blob?.options.policies?.length).toBeGreaterThan(0);
      expect(login).toBeDefined();
      expect(logout?.options.policies).toBeUndefined();
    } finally {
      globalThis.fetch = originalFetch;
    }
  });
});
