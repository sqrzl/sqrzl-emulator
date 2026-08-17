import { cleanupApp, createSPA } from '@askrjs/askr/boot';
import { describe, expect, it } from 'vite-plus/test';
import {
  createRouteRegistry,
  route,
  resolveRouteRequest,
  type RouteComponent,
} from '@askrjs/askr/router';
import type { AuthContext } from '@askrjs/auth';
import { routeRegistry } from '../src/pages/_routes';
import BucketPage from '../src/pages/app/buckets/bucket';
import {
  adminBucketsPath,
  blobIdFromBlobKey,
  blobPath,
  bucketFolderPath,
  bucketPath,
  adminMailboxesPath,
  adminTextsPath,
  mailMessagePath,
  mailboxPath,
  textConversationPath,
  textMessagePath,
  loginPath,
  logoutPath,
} from '../src/shared/routes';

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

async function resolvePathname(pathname: string) {
  const authContext: AuthContext = {
    authenticated: true,
    principal: {
      id: 'admin',
      name: 'admin',
      mode: 'password',
    },
    session: {
      id: 'admin',
      subject: 'admin',
      mode: 'password',
    },
    tenant: null,
  };

  return resolveRouteRequest(pathname, {
    registry: routeRegistry,
    authContext,
  });
}

type RouteComponentRoute = RouteComponent<Record<string, string>>;

async function mount(
  component: RouteComponentRoute,
  path: string
): Promise<HTMLDivElement> {
  const root = document.createElement('div');
  const registry = createRouteRegistry(() => {
    route(path, component);
  });

  document.body.appendChild(root);
  window.history.pushState(null, '', path);
  await createSPA({ root, registry });
  return root;
}

function normalizeWildcardParam(value: string | undefined): string | undefined {
  const stripped = value?.replace(/^\/+/, '');
  if (!stripped) {
    return stripped;
  }

  try {
    return decodeURIComponent(stripped);
  } catch {
    return stripped;
  }
}

describe('shared route helpers', () => {
  it('builds deterministic uuid-style blob ids from blob keys', () => {
    const blobKey = 'dir1/dir2/blobkey.png';
    const nestedBlobId = blobIdFromBlobKey(blobKey);

    expect(nestedBlobId).toMatch(
      /^[0-9a-f]{8}-[0-9a-f]{4}-5[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i
    );
    expect(nestedBlobId).toBe(blobIdFromBlobKey('dir1/dir2/blobkey.png'));
    expect(nestedBlobId).not.toBe(blobIdFromBlobKey('blobkey.png'));
    expect(blobPath('demo-bucket', blobKey)).toBe(
      `/admin/blobs/demo-bucket/${nestedBlobId}`
    );
    expect(blobPath('demo-bucket', blobKey)).not.toContain('%2F');
    expect(blobPath('demo-bucket', blobKey, blobKey)).toBe(
      `/admin/blobs/demo-bucket/${nestedBlobId}?key=${encodeURIComponent(
        blobKey
      )}`
    );
  });

  it('points the canonical ui routes at the admin surface', () => {
    expect(adminBucketsPath()).toBe('/admin/buckets');
    expect(adminMailboxesPath()).toBe('/admin/mailboxes');
    expect(adminTextsPath()).toBe('/admin/texts');
    expect(bucketPath('demo bucket')).toBe('/admin/buckets/demo%20bucket');
    expect(bucketFolderPath('demo bucket', 'dir one/child/')).toBe(
      '/admin/buckets/demo%20bucket/dir%20one%2Fchild'
    );
    expect(mailboxPath('team@example.com')).toBe(
      '/admin/mailboxes/team%40example.com'
    );
    expect(mailMessagePath('team@example.com', 'msg-1')).toBe(
      '/admin/mail/team%40example.com/msg-1'
    );
    expect(textConversationPath('+1555%01')).toBe('/admin/texts/%2B1555%2501');
    expect(textMessagePath('+1555', 'txt-1')).toBe('/admin/text/%2B1555/txt-1');
    expect(loginPath()).toBe('/login');
    expect(logoutPath()).toBe('/logout');
  });

  it('registers the reserved blob route and catch-all bucket fallback', () => {
    const paths = routeRegistry.routes.map((route) => route.path);

    expect(paths).toContain('/admin/blobs/{bucketName}/{blobId}');
    expect(paths).toContain('/admin/buckets/{bucketName}');
    expect(paths).toContain('/admin/mailboxes');
    expect(paths).toContain('/admin/mailboxes/{mailboxName}');
    expect(paths).toContain('/admin/mail/{mailboxName}/{messageId}');
    expect(paths).toContain('/admin/texts');
    expect(paths).toContain('/admin/texts/{peer}');
    expect(paths).toContain('/admin/text/{peer}/{messageId}');
    expect(paths).toContain('/admin/buckets/{bucketName}/*');
    expect(paths).not.toContain('/admin/buckets/{bucketName}/blob/{blobId}');
    expect(paths).not.toContain('/admin/buckets/{bucketName}/_blob/{blobId}');
  });

  it('resolves deep bucket folder routes through the wildcard bucket route', async () => {
    const deepPrefix = Array.from(
      { length: 70 },
      (_, index) => `dir${index}`
    ).join('/');

    globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
      const request =
        typeof input === 'string' || input instanceof URL
          ? new Request(input, init)
          : input;
      const url = new URL(request.url, 'http://localhost');

      if (
        url.pathname === '/admin/v1/auth/session' &&
        request.method === 'GET'
      ) {
        return jsonResponse({
          username: 'admin',
          mode: 'password',
        });
      }

      if (
        url.pathname === '/admin/v1/buckets/demo/objects' &&
        request.method === 'GET'
      ) {
        expect(url.searchParams.get('prefix')).toBe(`${deepPrefix}/`);
        expect(url.searchParams.get('search')).toBeNull();
        return jsonResponse({
          items: [
            {
              key: `${deepPrefix}/openapi.json`,
              size: 17,
              etag: 'etag-openapi',
              last_modified: '2026-05-25T11:15:00.000Z',
              content_type: 'application/json',
              storage_class: 'standard',
            },
          ],
          next: null,
        });
      }

      throw new Error(
        `Unexpected request: ${request.method} ${url.pathname}${url.search}`
      );
    };

    try {
      const resolved = await resolvePathname(
        bucketFolderPath('demo', deepPrefix)
      );
      expect(resolved?.kind).toBe('render');
      if (resolved?.kind === 'render') {
        expect(normalizeWildcardParam(resolved?.params['*'])).toBe(
          `${deepPrefix}`
        );
      }
    } finally {
      globalThis.fetch = originalFetch;
    }
  });

  it('pushes blob search keystrokes through the routed bucket page', async () => {
    const searchRequests: string[] = [];

    globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
      const request =
        typeof input === 'string' || input instanceof URL
          ? new Request(input, init)
          : input;
      const url = new URL(request.url, 'http://localhost');
      const search = url.searchParams.get('search');

      if (
        url.pathname === '/admin/v1/buckets/demo/objects' &&
        request.method === 'GET'
      ) {
        searchRequests.push(url.search);

        if (search === 'notes') {
          return jsonResponse({
            items: [
              {
                key: 'notes.txt',
                size: 18,
                etag: 'etag-notes',
                last_modified: '2026-05-25T08:35:00.000Z',
                content_type: 'text/plain',
                storage_class: 'standard',
              },
            ],
            next: null,
          });
        }

        return jsonResponse({
          items: [
            {
              key: 'image.png',
              size: 12,
              etag: 'etag-image',
              last_modified: '2026-05-25T08:30:00.000Z',
              content_type: 'image/png',
              storage_class: 'standard',
            },
          ],
          next: 'page-2',
        });
      }

      throw new Error(
        `Unexpected request: ${request.method} ${url.pathname}${url.search}`
      );
    };

    const originalUrl = `${window.location.pathname}${window.location.search}${window.location.hash}`;
    let mounted: HTMLDivElement | undefined;

    try {
      const component: RouteComponentRoute = () =>
        BucketPage({ bucketName: 'demo' });
      mounted = await mount(component, '/admin/buckets/demo');

      await flush();

      expect(mounted.textContent).toContain('image.png');
      expect(mounted.textContent).toContain('Next');

      const searchInput = mounted.querySelector(
        '#blob-search'
      ) as HTMLInputElement;
      searchInput.focus();
      expect(document.activeElement).toBe(searchInput);
      searchInput.value = 'notes';
      searchInput.dispatchEvent(
        new InputEvent('input', {
          bubbles: true,
          data: 'notes',
          inputType: 'insertText',
        })
      );
      await flush();
      expect(document.activeElement).toBe(searchInput);

      await flush();
      expect(window.location.search).toContain('search=notes');
      expect(
        searchRequests.some((entry) => entry.includes('search=notes'))
      ).toBe(true);
      expect(mounted.textContent).toContain('notes.txt');
      expect(mounted.textContent).not.toContain('image.png');
    } finally {
      if (mounted) {
        cleanupApp(mounted);
        mounted.remove();
      }
      window.history.pushState(null, '', originalUrl || '/');
      globalThis.fetch = originalFetch;
    }
  });

  it('keeps folder browsing working for keys that begin with blob', async () => {
    const folderPrefix = 'blob/notes';

    globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
      const request =
        typeof input === 'string' || input instanceof URL
          ? new Request(input, init)
          : input;
      const url = new URL(request.url, 'http://localhost');

      if (
        url.pathname === '/admin/v1/auth/session' &&
        request.method === 'GET'
      ) {
        return jsonResponse({
          username: 'admin',
          mode: 'password',
        });
      }

      if (
        url.pathname === '/admin/v1/buckets/demo/objects' &&
        request.method === 'GET'
      ) {
        expect(url.searchParams.get('prefix')).toBe(`${folderPrefix}/`);
        return jsonResponse({
          items: [
            {
              key: 'blob/notes/openapi.json',
              size: 17,
              etag: 'etag-openapi',
              last_modified: '2026-05-25T11:15:00.000Z',
              content_type: 'application/json',
              storage_class: 'standard',
            },
          ],
          next: null,
        });
      }

      throw new Error(
        `Unexpected request: ${request.method} ${url.pathname}${url.search}`
      );
    };

    try {
      const resolved = await resolvePathname(
        bucketFolderPath('demo', folderPrefix)
      );

      expect(resolved?.kind).toBe('render');
      if (resolved?.kind === 'render') {
        expect(normalizeWildcardParam(resolved?.params['*'])).toBe(
          'blob/notes'
        );
      }
    } finally {
      globalThis.fetch = originalFetch;
    }
  });

  it('does not treat legacy blob-looking bucket keys as blob routes', async () => {
    const blobId = blobIdFromBlobKey('blob/notes.txt');
    const resolved = await resolvePathname(
      bucketFolderPath('demo', `blob/${blobId}`)
    );

    expect(resolved?.kind).toBe('render');
    if (resolved?.kind === 'render') {
      expect(normalizeWildcardParam(resolved?.params['*'])).toBe(
        `blob/${blobId}`
      );
    }
  });
});
