import { cleanupApp, createSPA } from '@askrjs/askr/boot';
import { getManifest, getRoutes } from '@askrjs/askr/router';
import { describe, expect, it } from 'vite-plus/test';
import '../src/pages/_routes';
import {
  adminBucketsPath,
  blobIdFromBlobKey,
  blobPath,
  bucketFolderPath,
  bucketPath,
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

async function resolveRouteRequest(pathname: string) {
  const router = await import('../node_modules/@askrjs/askr/dist/router/route.js');
  return (router as any).resolveRouteRequest(pathname, {
    manifest: getManifest(),
  });
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
    expect(bucketPath('demo bucket')).toBe('/admin/buckets/demo%20bucket');
    expect(bucketFolderPath('demo bucket', 'dir one/child/')).toBe(
      '/admin/buckets/demo%20bucket/dir%20one/child'
    );
    expect(loginPath()).toBe('/login');
    expect(logoutPath()).toBe('/logout');
  });

  it('registers the reserved blob route and catch-all bucket fallback', () => {
    const paths = getRoutes().map((route) => route.path);

    expect(paths).toContain('/admin/blobs/{bucketName}/{blobId}');
    expect(paths).toContain('/admin/buckets/{bucketName}');
    expect(paths).toContain('/admin/buckets/{bucketName}/*');
    expect(paths).not.toContain('/admin/buckets/{bucketName}/blob/{blobId}');
    expect(paths).not.toContain('/admin/buckets/{bucketName}/_blob/{blobId}');
  });

  it('resolves deep bucket folder routes through the wildcard bucket route', async () => {
    const deepPrefix = Array.from({ length: 70 }, (_, index) => `dir${index}`)
      .join('/');

    globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
      const request =
        typeof input === 'string' || input instanceof URL
          ? new Request(input, init)
          : input;
      const url = new URL(request.url, 'http://localhost');

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
      const resolved = await resolveRouteRequest(
        `/admin/buckets/demo/${deepPrefix}`
      );

      expect(resolved?.kind).toBe('render');
      expect(resolved?.params['*']).toBe(`${deepPrefix}`);
    } finally {
      globalThis.fetch = originalFetch;
    }
  });

  it('sends blob search terms through the routed bucket page', async () => {
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
    const root = document.createElement('div');
    document.body.appendChild(root);

    try {
      const routes = getRoutes();
      window.history.pushState(null, '', '/admin/buckets/demo');

      await createSPA({ root, routes });
      await flush();

      expect(root.textContent).toContain('image.png');
      expect(root.textContent).toContain('Next');

      const searchInput = root.querySelector(
        '#blob-search'
      ) as HTMLInputElement;
      searchInput.value = 'notes';
      searchInput.dispatchEvent(new Event('input', { bubbles: true }));
      const submitButton = Array.from(root.querySelectorAll('button')).find(
        (button) => button.textContent?.trim() === 'Search'
      );
      submitButton?.dispatchEvent(
        new MouseEvent('click', { bubbles: true, cancelable: true })
      );

      await flush();
      expect(
        searchRequests.some((entry) => entry.includes('search=notes'))
      ).toBe(true);
      expect(root.textContent).toContain('notes.txt');
      expect(root.textContent).not.toContain('image.png');
    } finally {
      cleanupApp(root);
      root.remove();
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
      const resolved = await resolveRouteRequest(
        `/admin/buckets/demo/${folderPrefix}`
      );

      expect(resolved?.kind).toBe('render');
      expect(resolved?.params['*']).toBe('blob/notes');
    } finally {
      globalThis.fetch = originalFetch;
    }
  });

  it('does not treat legacy blob-looking bucket keys as blob routes', async () => {
    const blobId = blobIdFromBlobKey('blob/notes.txt');
    const resolved = await resolveRouteRequest(
      `/admin/buckets/demo/blob/${blobId}`
    );

    expect(resolved?.kind).toBe('render');
    expect(resolved?.params['*']).toBe(`blob/${blobId}`);
  });
});
