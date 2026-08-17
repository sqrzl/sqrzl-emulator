import { describe, expect, it } from 'vite-plus/test';

import {
  createBucket,
  deleteBucketWithContents,
  listBucketPage,
  loadBuckets,
  setBucketVersioning,
} from '@/features/buckets/buckets.query';
import {
  countBucketObjects,
  deleteAllBucketObjects,
  deleteObject,
  downloadObjectContent,
  findObjectByBlobId,
  loadObjectMetadata,
  loadObjectPage,
  loadObjectTags,
  loadObjectVersions,
  putObjectContent,
  putObjectTags,
} from '@/features/objects/objects.query';
import { blobIdFromBlobKey } from '@/shared/routes';

const originalFetch = globalThis.fetch;

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: {
      'content-type': 'application/json',
    },
  });
}

function binaryResponse(
  body: BodyInit,
  contentType = 'application/octet-stream'
): Response {
  return new Response(body, {
    status: 200,
    headers: {
      'content-type': contentType,
    },
  });
}

async function readBodyText(
  body: BodyInit | null | undefined
): Promise<string> {
  if (body === undefined || body === null) {
    return '';
  }

  if (typeof body === 'string') {
    return body;
  }

  if (body instanceof Blob) {
    return body.text();
  }

  if (body instanceof URLSearchParams) {
    return body.toString();
  }

  if (body instanceof ArrayBuffer) {
    return new TextDecoder().decode(body);
  }

  if (ArrayBuffer.isView(body)) {
    return new TextDecoder().decode(body);
  }

  return String(body);
}

function installBlobApiFetchMock(): void {
  const storedObjects = new Map<
    string,
    { body: string; contentType: string }
  >();
  const bucketObjects = new Map<
    string,
    Array<{
      key: string;
      size: number;
      etag: string;
      last_modified: string;
      content_type: string;
      storage_class: string;
    }>
  >([
    [
      'alpha',
      [
        {
          key: 'docs/readme.txt',
          size: 5,
          etag: 'etag-alpha-1',
          last_modified: '2026-05-25T10:00:00.000Z',
          content_type: 'text/plain',
          storage_class: 'standard',
        },
        {
          key: 'notes.txt',
          size: 18,
          etag: 'etag-alpha-2',
          last_modified: '2026-05-25T10:05:00.000Z',
          content_type: 'text/plain',
          storage_class: 'standard',
        },
      ],
    ],
    [
      'beta',
      [
        {
          key: 'image.png',
          size: 12,
          etag: 'etag-beta-1',
          last_modified: '2026-05-25T08:30:00.000Z',
          content_type: 'image/png',
          storage_class: 'standard',
        },
        {
          key: 'notes.txt',
          size: 18,
          etag: 'etag-beta-2',
          last_modified: '2026-05-25T08:35:00.000Z',
          content_type: 'text/plain',
          storage_class: 'standard',
        },
      ],
    ],
  ]);
  let storedTags: Record<string, string> = {};

  const mockFetch: typeof fetch = async (
    input: RequestInfo | URL,
    init?: RequestInit
  ) => {
    const requestUrl =
      typeof input === 'string' || input instanceof URL
        ? input.toString()
        : input.url;
    const url = new URL(requestUrl, 'http://localhost');
    const method = (
      init?.method ??
      (typeof input === 'string' || input instanceof URL
        ? 'GET'
        : input.method) ??
      'GET'
    ).toUpperCase();
    const headers = new Headers(
      init?.headers ??
        (typeof input === 'string' || input instanceof URL
          ? undefined
          : input.headers)
    );

    if (url.pathname === '/admin/v1/buckets' && method === 'GET') {
      const search = url.searchParams.get('search') ?? undefined;
      const next = url.searchParams.get('next') ?? undefined;
      const items = [
        {
          name: 'alpha',
          created_at: '2026-05-25T09:00:00.000Z',
          versioning_enabled: true,
        },
        {
          name: 'beta',
          created_at: '2026-05-24T09:00:00.000Z',
          versioning_enabled: false,
        },
      ];

      if (search) {
        return jsonResponse({
          items: items.filter((bucket) => bucket.name.includes(search)),
          next: null,
        });
      }

      if (next === 'bucket-page-2') {
        return jsonResponse({ items: [items[1]], next: null });
      }

      return jsonResponse({
        items: [items[0]],
        next: 'bucket-page-2',
      });
    }

    if (url.pathname === '/admin/v1/buckets' && method === 'POST') {
      const body = JSON.parse(await readBodyText(init?.body)) as {
        name: string;
      };
      return jsonResponse(
        {
          name: body.name,
          created_at: '2026-05-25T10:30:00.000Z',
          versioning_enabled: false,
        },
        201
      );
    }

    if (
      url.pathname === '/admin/v1/buckets/alpha/versioning' &&
      method === 'PUT'
    ) {
      const body = JSON.parse(await readBodyText(init?.body)) as {
        enabled: boolean;
      };
      return jsonResponse({ enabled: body.enabled });
    }

    if (
      url.pathname === '/admin/v1/buckets/alpha/objects' &&
      method === 'GET'
    ) {
      const search = url.searchParams.get('search') ?? undefined;
      const next = url.searchParams.get('next') ?? undefined;
      const items = bucketObjects.get('alpha') ?? [];

      if (search) {
        return jsonResponse({
          items: items.filter((object) => object.key.includes(search)),
          next: null,
        });
      }

      if (next === 'object-page-2') {
        return jsonResponse({
          items: items.slice(1),
          next: null,
        });
      }

      return jsonResponse({
        items: items.slice(0, 1),
        next: items.length > 1 ? 'object-page-2' : null,
      });
    }

    if (url.pathname === '/admin/v1/buckets/beta/objects' && method === 'GET') {
      return jsonResponse({
        items: bucketObjects.get('beta') ?? [],
        next: null,
      });
    }

    if (
      url.pathname ===
        '/admin/v1/buckets/alpha/objects/docs%2Freadme.txt/content' &&
      method === 'PUT'
    ) {
      const body = init?.body ?? '';
      const textBody = await readBodyText(body);
      storedObjects.set('docs/readme.txt', {
        body: textBody,
        contentType: headers.get('content-type') ?? 'application/octet-stream',
      });

      return jsonResponse(
        {
          key: 'docs/readme.txt',
          size: textBody.length,
          etag: 'etag-uploaded',
          last_modified: '2026-05-25T11:00:00.000Z',
          content_type: headers.get('content-type'),
          metadata: {
            owner: headers.get('x-amz-meta-owner') ?? '',
          },
          storage_class: 'standard',
          version_id: null,
        },
        201
      );
    }

    if (
      url.pathname ===
        '/admin/v1/buckets/alpha/objects/docs%2Freadme.txt/content' &&
      method === 'GET'
    ) {
      const stored = storedObjects.get('docs/readme.txt');
      return binaryResponse(
        stored?.body ?? '',
        stored?.contentType ?? 'application/octet-stream'
      );
    }

    if (
      url.pathname === '/admin/v1/buckets/alpha/objects/docs%2Freadme.txt' &&
      method === 'DELETE'
    ) {
      storedObjects.delete('docs/readme.txt');
      bucketObjects.set(
        'alpha',
        (bucketObjects.get('alpha') ?? []).filter(
          (object) => object.key !== 'docs/readme.txt'
        )
      );
      return new Response(null, { status: 204 });
    }

    if (
      url.pathname === '/admin/v1/buckets/alpha/objects/notes.txt' &&
      method === 'DELETE'
    ) {
      bucketObjects.set(
        'alpha',
        (bucketObjects.get('alpha') ?? []).filter(
          (object) => object.key !== 'notes.txt'
        )
      );
      return new Response(null, { status: 204 });
    }

    if (url.pathname === '/admin/v1/buckets/alpha' && method === 'DELETE') {
      bucketObjects.set('alpha', []);
      return new Response(null, { status: 204 });
    }

    if (
      url.pathname === '/admin/v1/buckets/alpha/objects/docs%2Freadme.txt' &&
      method === 'GET'
    ) {
      return jsonResponse({
        key: 'docs/readme.txt',
        size: 5,
        etag: 'etag-uploaded',
        last_modified: '2026-05-25T11:00:00.000Z',
        content_type: 'text/plain',
        metadata: { owner: 'alice' },
        storage_class: 'standard',
        version_id: 'v1',
      });
    }

    if (
      url.pathname ===
        '/admin/v1/buckets/alpha/objects/docs%2Freadme.txt/tags' &&
      method === 'PUT'
    ) {
      const body = JSON.parse(await readBodyText(init?.body)) as {
        tags: Record<string, string>;
      };
      storedTags = body.tags;
      return jsonResponse({ tags: storedTags });
    }

    if (
      url.pathname ===
        '/admin/v1/buckets/alpha/objects/docs%2Freadme.txt/tags' &&
      method === 'GET'
    ) {
      return jsonResponse({ tags: storedTags });
    }

    if (
      url.pathname ===
        '/admin/v1/buckets/alpha/objects/docs%2Freadme.txt/versions' &&
      method === 'GET'
    ) {
      return jsonResponse({
        items: [
          {
            key: 'docs/readme.txt',
            version_id: 'v1',
            is_latest: true,
            size: 5,
            etag: 'etag-uploaded',
            last_modified: '2026-05-25T11:00:00.000Z',
          },
        ],
        next: null,
      });
    }

    throw new Error(
      `Unexpected request: ${method} ${url.pathname}${url.search}`
    );
  };

  (globalThis as typeof globalThis & { fetch: typeof fetch }).fetch = mockFetch;
}

function restoreFetch(): void {
  (globalThis as typeof globalThis & { fetch: typeof fetch }).fetch =
    originalFetch;
}

describe('generated admin feature workflows', () => {
  it('loads bucket overview through generated operations', async () => {
    installBlobApiFetchMock();

    try {
      const created = await createBucket({ name: 'gamma' });
      const versioning = await setBucketVersioning({
        bucketName: 'alpha',
        enabled: true,
      });
      const snapshot = await loadBuckets({
        signal: new AbortController().signal,
      });

      expect(created.name).toBe('gamma');
      expect(created.createdAt).toBe('2026-05-25T10:30:00.000Z');
      expect(versioning.enabled).toBe(true);
      expect(snapshot.totalBuckets).toBe(2);
      expect(snapshot.versioningEnabledBuckets).toBe(1);
      expect(snapshot.totalObjects).toBe(4);
      expect(snapshot.buckets[0].name).toBe('alpha');
      expect(snapshot.buckets[0].objectCount).toBe(2);
    } finally {
      restoreFetch();
    }
  });

  it('supports bucket and object paging/search through generated list queries', async () => {
    installBlobApiFetchMock();
    const signal = new AbortController().signal;

    try {
      const bucketPage1 = await listBucketPage({ signal });
      const bucketPage2 = await listBucketPage({
        next: bucketPage1.next ?? undefined,
        signal,
      });
      const bucketSearch = await listBucketPage({ search: 'alpha', signal });

      const objectPage1 = await loadObjectPage({ bucketName: 'alpha', signal });
      const objectPage2 = await loadObjectPage({
        bucketName: 'alpha',
        next: objectPage1.next ?? undefined,
        signal,
      });
      const objectSearch = await loadObjectPage({
        bucketName: 'alpha',
        search: 'notes',
        signal,
      });

      expect(bucketPage1.items[0]?.name).toBe('alpha');
      expect(bucketPage1.next).toBe('bucket-page-2');
      expect(bucketPage2.items[0]?.name).toBe('beta');
      expect(bucketSearch.items).toHaveLength(1);
      expect(bucketSearch.items[0]?.name).toBe('alpha');

      expect(objectPage1.items[0]?.key).toBe('docs/readme.txt');
      expect(objectPage1.next).toBe('object-page-2');
      expect(objectPage2.items[0]?.key).toBe('notes.txt');
      expect(objectSearch.items).toHaveLength(1);
      expect(objectSearch.items[0]?.key).toBe('notes.txt');
    } finally {
      restoreFetch();
    }
  });

  it('applies path prefix when loading objects and resolves blob ids through paged lookup', async () => {
    const listCalls: Array<{
      prefix: string | null;
      search: string | null;
      next: string | null;
    }> = [];

    globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
      const request =
        typeof input === 'string' || input instanceof URL
          ? new Request(input, init)
          : input;
      const url = new URL(request.url, 'http://localhost');
      const method = request.method.toUpperCase();

      if (
        url.pathname === '/admin/v1/buckets/alpha/objects' &&
        method === 'GET'
      ) {
        const prefix = url.searchParams.get('prefix');
        const search = url.searchParams.get('search');
        const next = url.searchParams.get('next');
        listCalls.push({ prefix, search, next });

        if (prefix === 'docs/') {
          return jsonResponse({
            folders: [{ name: 'api/', prefix: 'docs/api/' }],
            items: [
              {
                key: 'docs/readme.txt',
                size: 5,
                etag: 'etag-readme',
                last_modified: '2026-05-25T10:00:00.000Z',
                content_type: 'text/plain',
                storage_class: 'standard',
              },
            ],
            next: null,
          });
        }

        return jsonResponse({
          folders: [],
          items: [
            {
              key: 'notes.txt',
              size: 18,
              etag: 'etag-notes',
              last_modified: '2026-05-25T11:20:00.000Z',
              content_type: 'text/plain',
              storage_class: 'standard',
            },
            {
              key: 'readme.txt',
              size: 12,
              etag: 'etag-readme',
              last_modified: '2026-05-25T11:25:00.000Z',
              content_type: 'text/plain',
              storage_class: 'standard',
            },
          ],
          next: null,
        });
      }

      if (
        url.pathname === '/admin/v1/buckets/alpha/objects/docs%2Freadme.txt' &&
        method === 'GET'
      ) {
        return jsonResponse({
          key: 'docs/readme.txt',
          size: 5,
          etag: 'etag-readme',
          last_modified: '2026-05-25T10:00:00.000Z',
          content_type: 'text/plain',
          metadata: {},
          storage_class: 'standard',
          version_id: null,
        });
      }

      throw new Error(
        `Unexpected request: ${request.method} ${url.pathname}${url.search}`
      );
    };

    const signal = new AbortController().signal;

    try {
      const folderPage = await loadObjectPage({
        bucketName: 'alpha',
        pathPrefix: 'docs/',
        signal,
      });
      const resolved = await findObjectByBlobId({
        bucketName: 'alpha',
        blobId: blobIdFromBlobKey('docs/readme.txt'),
        pathPrefix: 'docs/',
        signal,
      });

      expect(folderPage.folders).toEqual([
        { name: 'api/', prefix: 'docs/api/' },
      ]);
      expect(folderPage.items).toHaveLength(1);
      expect(folderPage.items[0]?.key).toBe('docs/readme.txt');
      expect(listCalls[0]).toEqual({
        prefix: 'docs/',
        search: null,
        next: null,
      });
      expect(listCalls).toHaveLength(2);
      expect(resolved?.key).toBe('docs/readme.txt');
    } finally {
      globalThis.fetch = originalFetch;
    }
  });

  it('deletes every object from mixed paged folder trees', async () => {
    const listCalls: Array<{
      prefix: string | null;
      next: string | null;
    }> = [];
    const deleted: string[] = [];

    function objectRow(key: string) {
      return {
        key,
        size: 1,
        etag: `etag-${key}`,
        last_modified: '2026-05-25T10:00:00.000Z',
        content_type: 'application/octet-stream',
        storage_class: 'standard',
      };
    }

    const pages = new Map([
      [
        ':',
        {
          folders: [
            { name: 'docs/', prefix: 'docs/' },
            { name: 'media/', prefix: 'media/' },
          ],
          items: [objectRow('root.txt')],
          next: 'root-page-2',
        },
      ],
      [
        ':root-page-2',
        {
          folders: [{ name: 'logs/', prefix: 'logs/' }],
          items: [objectRow('root-2.bin')],
          next: null,
        },
      ],
      [
        'docs/:',
        {
          folders: [{ name: 'api/', prefix: 'docs/api/' }],
          items: [objectRow('docs/readme.txt')],
          next: null,
        },
      ],
      [
        'docs/api/:',
        {
          folders: [],
          items: [objectRow('docs/api/openapi.json')],
          next: null,
        },
      ],
      [
        'media/:',
        {
          folders: [],
          items: [objectRow('media/image.png')],
          next: null,
        },
      ],
      [
        'logs/:',
        {
          folders: [{ name: '2026/', prefix: 'logs/2026/' }],
          items: [],
          next: null,
        },
      ],
      [
        'logs/2026/:',
        {
          folders: [],
          items: [objectRow('logs/2026/a.log')],
          next: 'logs-page-2',
        },
      ],
      [
        'logs/2026/:logs-page-2',
        {
          folders: [],
          items: [objectRow('logs/2026/b.log')],
          next: null,
        },
      ],
    ]);

    globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
      const request =
        typeof input === 'string' || input instanceof URL
          ? new Request(input, init)
          : input;
      const url = new URL(request.url, 'http://localhost');
      const method = request.method.toUpperCase();

      if (
        url.pathname === '/admin/v1/buckets/alpha/objects' &&
        method === 'GET'
      ) {
        const prefix = url.searchParams.get('prefix');
        const next = url.searchParams.get('next');
        listCalls.push({ prefix, next });
        const page = pages.get(`${prefix ?? ''}:${next ?? ''}`);
        if (!page) {
          throw new Error(`Unexpected page request: ${url.search}`);
        }

        return jsonResponse(page);
      }

      if (
        url.pathname.startsWith('/admin/v1/buckets/alpha/objects/') &&
        method === 'DELETE'
      ) {
        deleted.push(
          decodeURIComponent(
            url.pathname.replace('/admin/v1/buckets/alpha/objects/', '')
          )
        );
        return new Response(null, { status: 204 });
      }

      throw new Error(
        `Unexpected request: ${request.method} ${url.pathname}${url.search}`
      );
    };

    try {
      const count = await deleteAllBucketObjects({
        bucketName: 'alpha',
        signal: new AbortController().signal,
      });

      expect(count).toBe(7);
      expect(listCalls).toEqual([
        { prefix: null, next: null },
        { prefix: null, next: 'root-page-2' },
        { prefix: 'docs/', next: null },
        { prefix: 'media/', next: null },
        { prefix: 'logs/', next: null },
        { prefix: 'docs/api/', next: null },
        { prefix: 'logs/2026/', next: null },
        { prefix: 'logs/2026/', next: 'logs-page-2' },
      ]);
      expect(deleted.sort()).toEqual(
        [
          'docs/api/openapi.json',
          'docs/readme.txt',
          'logs/2026/a.log',
          'logs/2026/b.log',
          'media/image.png',
          'root-2.bin',
          'root.txt',
        ].sort()
      );
    } finally {
      globalThis.fetch = originalFetch;
    }
  });

  it('uploads, downloads, and deletes object content through fgrzl/fetch', async () => {
    installBlobApiFetchMock();

    try {
      const upload = await putObjectContent({
        bucketName: 'alpha',
        objectKey: 'docs/readme.txt',
        content: new Blob(['hello'], { type: 'text/plain' }),
        contentType: 'text/plain',
        metadata: { owner: 'alice' },
      });

      const download = await downloadObjectContent({
        bucketName: 'alpha',
        objectKey: 'docs/readme.txt',
      });

      await deleteObject({ bucketName: 'alpha', objectKey: 'docs/readme.txt' });

      expect(upload.key).toBe('docs/readme.txt');
      expect(upload.content_type).toBe('text/plain');
      expect(upload.metadata.owner).toBe('alice');
      expect(upload.size).toBe(5);
      expect(download.fileName).toBe('readme.txt');
      expect(download.contentType).toBe('text/plain');
      expect(await download.blob.text()).toBe('hello');
    } finally {
      restoreFetch();
    }
  });

  it('loads metadata and edits tags and versions through generated operations', async () => {
    installBlobApiFetchMock();
    const signal = new AbortController().signal;

    try {
      const metadata = await loadObjectMetadata({
        bucketName: 'alpha',
        objectKey: 'docs/readme.txt',
        signal,
      });
      await putObjectTags({
        bucketName: 'alpha',
        objectKey: 'docs/readme.txt',
        tags: { env: 'test' },
      });
      const tags = await loadObjectTags({
        bucketName: 'alpha',
        objectKey: 'docs/readme.txt',
        signal,
      });
      const versions = await loadObjectVersions({
        bucketName: 'alpha',
        objectKey: 'docs/readme.txt',
        signal,
      });

      expect(metadata.metadata.owner).toBe('alice');
      expect(tags.tags.env).toBe('test');
      expect(versions.items[0].version_id).toBe('v1');
    } finally {
      restoreFetch();
    }
  });

  it('deletes bucket contents before deleting the bucket', async () => {
    installBlobApiFetchMock();
    const signal = new AbortController().signal;

    try {
      expect(await countBucketObjects({ bucketName: 'alpha', signal })).toBe(2);

      const deletedObjects = await deleteAllBucketObjects({
        bucketName: 'alpha',
      });
      expect(deletedObjects).toBe(2);

      const result = await deleteBucketWithContents({ bucketName: 'alpha' });
      expect(result.deletedObjects).toBe(0);
      expect(await countBucketObjects({ bucketName: 'alpha', signal })).toBe(0);
    } finally {
      restoreFetch();
    }
  });
});
