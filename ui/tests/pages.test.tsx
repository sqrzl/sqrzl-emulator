import { cleanupApp, createIsland } from '@askrjs/askr/boot';
import { describe, expect, it } from 'vite-plus/test';
import AppLayout from '../src/pages/app/_layout';
import LoginPage from '../src/pages/auth/login';
import Home from '../src/pages/app/buckets';
import BucketPage from '../src/pages/app/bucket';
import BlobPage from '../src/pages/app/blob';
import { blobIdFromBlobKey } from '../src/shared/routes';

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

function mount(component: any): HTMLDivElement {
  const root = document.createElement('div');
  document.body.appendChild(root);
  createIsland({ root, component });
  return root;
}

function click(element: Element): void {
  if (element instanceof HTMLElement) {
    element.click();
    return;
  }

  element.dispatchEvent(new MouseEvent('click', { bubbles: true }));
}

function rowButton(
  root: HTMLElement,
  rowText: string
): HTMLButtonElement | undefined {
  const row = Array.from(root.querySelectorAll('tr')).find((candidate) =>
    candidate.textContent?.includes(rowText)
  );

  return row?.querySelector('button') as HTMLButtonElement | undefined;
}

describe('simplified page flows', () => {
  it('renders the app header with nav links and theme toggle', async () => {
    const root = mount(() => (
      <AppLayout>
        <p>content</p>
      </AppLayout>
    ));

    try {
      await flush();
      expect(root.textContent).toContain('Peas');
      expect(root.textContent).not.toContain('Buckets');
      expect(root.querySelector('[aria-label="Toggle theme"]')).toBeTruthy();
      expect(root.querySelector('a[href="/logout"]')).toBeTruthy();
    } finally {
      cleanupApp(root);
      root.remove();
    }
  });

  it('renders login and shows auth errors after submit', async () => {
    globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
      const request =
        typeof input === 'string' || input instanceof URL
          ? new Request(input, init)
          : input;
      const url = new URL(request.url, 'http://localhost');

      if (url.pathname === '/admin/v1/auth/login') {
        return jsonResponse(
          { code: 'Unauthorized', error: 'Bad credentials' },
          401
        );
      }

      throw new Error(`Unexpected request: ${request.method} ${url.pathname}`);
    };

    const root = mount(() => <LoginPage />);

    try {
      await flush();
      expect(root.textContent).toContain('Sign in');

      const username = root.querySelector('#username') as HTMLInputElement;
      const password = root.querySelector('#password') as HTMLInputElement;
      const form = root.querySelector('form') as HTMLFormElement;

      username.value = 'admin';
      username.dispatchEvent(new Event('input', { bubbles: true }));
      password.value = 'wrong';
      password.dispatchEvent(new Event('input', { bubbles: true }));
      form.dispatchEvent(
        new Event('submit', { bubbles: true, cancelable: true })
      );

      await flush();
      expect(root.textContent).toContain('Bad credentials');
    } finally {
      cleanupApp(root);
      root.remove();
      globalThis.fetch = originalFetch;
    }
  });

  it('renders buckets and creates a bucket from the home dialog', async () => {
    let created = false;

    globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
      const request =
        typeof input === 'string' || input instanceof URL
          ? new Request(input, init)
          : input;
      const url = new URL(request.url, 'http://localhost');

      if (url.pathname === '/admin/v1/buckets' && request.method === 'GET') {
        return jsonResponse({
          items: created
            ? [
                {
                  name: 'alpha',
                  created_at: '2026-05-25T09:00:00.000Z',
                  versioning_enabled: false,
                },
              ]
            : [],
          next: null,
        });
      }

      if (
        url.pathname === '/admin/v1/buckets/alpha/objects' &&
        request.method === 'GET'
      ) {
        return jsonResponse({ items: [], next: null });
      }

      if (url.pathname === '/admin/v1/buckets' && request.method === 'POST') {
        created = true;
        return jsonResponse(
          {
            name: 'alpha',
            created_at: '2026-05-25T09:00:00.000Z',
            versioning_enabled: false,
          },
          201
        );
      }

      throw new Error(`Unexpected request: ${request.method} ${url.pathname}`);
    };

    const root = mount(() => <Home />);

    try {
      await flush();
      expect(root.textContent).toContain('No buckets yet');

      const addButton = Array.from(root.querySelectorAll('button')).find(
        (button) => button.textContent?.includes('Add bucket')
      );
      click(addButton!);
      await flush();

      const input = document.querySelector('#bucket-name') as HTMLInputElement;
      const form = document.querySelector('form') as HTMLFormElement;
      input.value = 'alpha';
      input.dispatchEvent(new Event('input', { bubbles: true }));
      form.dispatchEvent(
        new Event('submit', { bubbles: true, cancelable: true })
      );

      await flush();
      await flush();
      expect(created).toBe(true);
      expect(document.body.textContent).toContain('Buckets');
    } finally {
      cleanupApp(root);
      root.remove();
      globalThis.fetch = originalFetch;
    }
  });

  it('renders bucket search, pagination controls, and delete acknowledgement', async () => {
    let deletedBucket = false;
    const deletedObjects: string[] = [];
    let bucketObjects = [
      {
        key: 'one.txt',
        size: 1,
        etag: 'etag-1',
        last_modified: '2026-05-25T10:00:00.000Z',
        content_type: 'text/plain',
        storage_class: 'standard',
      },
      {
        key: 'two.txt',
        size: 1,
        etag: 'etag-2',
        last_modified: '2026-05-25T10:01:00.000Z',
        content_type: 'text/plain',
        storage_class: 'standard',
      },
    ];

    globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
      const request =
        typeof input === 'string' || input instanceof URL
          ? new Request(input, init)
          : input;
      const url = new URL(request.url, 'http://localhost');
      const next = url.searchParams.get('next');
      const search = url.searchParams.get('search');

      if (url.pathname === '/admin/v1/buckets' && request.method === 'GET') {
        if (search === 'alpha') {
          return jsonResponse({
            items: deletedBucket
              ? []
              : [
                  {
                    name: 'alpha',
                    created_at: '2026-05-25T09:00:00.000Z',
                    versioning_enabled: false,
                  },
                ],
            next: null,
          });
        }

        if (next === 'page-2') {
          return jsonResponse({
            items: [
              {
                name: 'beta',
                created_at: '2026-05-24T09:00:00.000Z',
                versioning_enabled: false,
              },
            ],
            next: null,
          });
        }

        return jsonResponse({
          items: [
            {
              name: 'alpha',
              created_at: '2026-05-25T09:00:00.000Z',
              versioning_enabled: false,
            },
          ],
          next: 'page-2',
        });
      }

      if (
        url.pathname === '/admin/v1/buckets/alpha/objects' &&
        request.method === 'GET'
      ) {
        return jsonResponse({
          items: deletedBucket ? [] : bucketObjects,
          next: null,
        });
      }

      if (
        url.pathname === '/admin/v1/buckets/alpha/objects/one.txt' &&
        request.method === 'DELETE'
      ) {
        deletedObjects.push('one.txt');
        bucketObjects = bucketObjects.filter(
          (object) => object.key !== 'one.txt'
        );
        return new Response(null, { status: 204 });
      }

      if (
        url.pathname === '/admin/v1/buckets/alpha/objects/two.txt' &&
        request.method === 'DELETE'
      ) {
        deletedObjects.push('two.txt');
        bucketObjects = bucketObjects.filter(
          (object) => object.key !== 'two.txt'
        );
        return new Response(null, { status: 204 });
      }

      if (
        url.pathname === '/admin/v1/buckets/alpha' &&
        request.method === 'DELETE'
      ) {
        deletedBucket = true;
        return new Response(null, { status: 204 });
      }

      throw new Error(
        `Unexpected request: ${request.method} ${url.pathname}${url.search}`
      );
    };

    const root = mount(() => <Home />);

    try {
      await flush();
      expect(root.textContent).toContain('alpha');

      const searchInput = root.querySelector(
        '#bucket-search'
      ) as HTMLInputElement;
      expect(root.textContent).toContain('Next');
      expect(searchInput).toBeTruthy();
      expect(root.textContent).toContain('Search buckets');

      const deleteButton = rowButton(root, 'alpha');
      click(deleteButton!);
      await flush();
      await flush();
      expect(document.body.textContent).toContain(
        'You are going to delete 2 blobs from alpha.'
      );

      const confirmDelete = Array.from(
        document.querySelectorAll('button')
      ).find((button) =>
        button.textContent?.includes('Delete bucket and 2 blobs')
      );
      click(confirmDelete!);
      await flush();
      await flush();
      await flush();

      expect(deletedObjects).toEqual(['one.txt', 'two.txt']);
      expect(deletedBucket).toBe(true);
    } finally {
      cleanupApp(root);
      root.remove();
      globalThis.fetch = originalFetch;
    }
  });

  it('renders blobs and uploads a blob from the bucket dialog', async () => {
    let uploaded = false;

    globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
      const request =
        typeof input === 'string' || input instanceof URL
          ? new Request(input, init)
          : input;
      const url = new URL(request.url, 'http://localhost');

      if (
        url.pathname === '/admin/v1/buckets/alpha/objects' &&
        request.method === 'GET'
      ) {
        return jsonResponse({
          items: uploaded
            ? [
                {
                  key: 'docs/readme.txt',
                  size: 5,
                  etag: 'etag-uploaded',
                  last_modified: '2026-05-25T11:00:00.000Z',
                  content_type: 'text/plain',
                  storage_class: 'standard',
                },
              ]
            : [],
          next: null,
        });
      }

      if (
        url.pathname ===
          '/admin/v1/buckets/alpha/objects/docs%2Freadme.txt/content' &&
        request.method === 'PUT'
      ) {
        uploaded = true;
        return jsonResponse(
          {
            key: 'docs/readme.txt',
            size: 5,
            etag: 'etag-uploaded',
            last_modified: '2026-05-25T11:00:00.000Z',
            content_type: 'text/plain',
            metadata: {},
            storage_class: 'standard',
            version_id: null,
          },
          201
        );
      }

      throw new Error(`Unexpected request: ${request.method} ${url.pathname}`);
    };

    const root = mount(() => <BucketPage bucketName="alpha" />);

    try {
      await flush();
      expect(root.textContent).toContain('No blobs in this bucket');

      const addButton = Array.from(root.querySelectorAll('button')).find(
        (button) => button.textContent?.includes('Add blob')
      );
      click(addButton!);
      await flush();

      const keyInput = document.querySelector('#blob-key') as HTMLInputElement;
      const fileInput = document.querySelector(
        '#blob-file'
      ) as HTMLInputElement;
      const form = document.querySelector('form') as HTMLFormElement;
      const file = new File(['hello'], 'readme.txt', { type: 'text/plain' });

      keyInput.value = 'docs/readme.txt';
      keyInput.dispatchEvent(new Event('input', { bubbles: true }));
      Object.defineProperty(fileInput, 'files', {
        configurable: true,
        value: [file],
      });
      fileInput.dispatchEvent(new Event('input', { bubbles: true }));
      form.dispatchEvent(
        new Event('submit', { bubbles: true, cancelable: true })
      );

      await flush();
      await flush();
      expect(uploaded).toBe(true);
      expect(document.body.textContent).toContain('Blobs');
    } finally {
      cleanupApp(root);
      root.remove();
      globalThis.fetch = originalFetch;
    }
  });

  it('renders blob search, pagination controls, richer sizes, and delete', async () => {
    let deleted = false;

    globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
      const request =
        typeof input === 'string' || input instanceof URL
          ? new Request(input, init)
          : input;
      const url = new URL(request.url, 'http://localhost');
      const next = url.searchParams.get('next');
      const search = url.searchParams.get('search');

      if (
        url.pathname === '/admin/v1/buckets/alpha/objects' &&
        request.method === 'GET'
      ) {
        if (search === 'notes') {
          return jsonResponse({
            items: deleted
              ? []
              : [
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

        if (next === 'page-2') {
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

      if (
        url.pathname === '/admin/v1/buckets/alpha/objects/image.png' &&
        request.method === 'DELETE'
      ) {
        deleted = true;
        return new Response(null, { status: 204 });
      }

      throw new Error(
        `Unexpected request: ${request.method} ${url.pathname}${url.search}`
      );
    };

    const root = mount(() => <BucketPage bucketName="alpha" />);

    try {
      await flush();
      expect(root.textContent).toContain('image.png');
      expect(root.textContent).toContain('12 bytes');
      expect(root.textContent).toContain('Search blobs');
      expect(root.textContent).toContain('Next');

      const deleteButton = rowButton(root, 'image.png');
      click(deleteButton!);
      await flush();
      expect(document.body.textContent).toContain(
        'Delete image.png from alpha.'
      );

      const confirmDelete = Array.from(
        document.querySelectorAll('button')
      ).find((button) => button.textContent === 'Delete blob');
      click(confirmDelete!);
      await flush();
      await flush();
      await flush();

      expect(deleted).toBe(true);
    } finally {
      cleanupApp(root);
      root.remove();
      globalThis.fetch = originalFetch;
    }
  });

  it('renders blob metadata', async () => {
    globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
      const request =
        typeof input === 'string' || input instanceof URL
          ? new Request(input, init)
          : input;
      const url = new URL(request.url, 'http://localhost');

      if (
        url.pathname === '/admin/v1/buckets/alpha/objects' &&
        request.method === 'GET'
      ) {
        return jsonResponse({
          items: [
            {
              key: 'docs/readme.txt',
              size: 5,
              etag: 'etag-uploaded',
              last_modified: '2026-05-25T11:00:00.000Z',
              content_type: 'text/plain',
              storage_class: 'standard',
            },
          ],
          next: null,
        });
      }

      if (
        url.pathname === '/admin/v1/buckets/alpha/objects/docs%2Freadme.txt' &&
        request.method === 'GET'
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

      throw new Error(`Unexpected request: ${request.method} ${url.pathname}`);
    };

    const root = mount(() => (
      <BlobPage
        bucketName="alpha"
        blobId={blobIdFromBlobKey('docs/readme.txt')}
      />
    ));

    try {
      await flush();
      expect(root.textContent).toContain('docs/readme.txt');
      expect(root.textContent).toContain('owner');
      expect(root.textContent).toContain('alice');
    } finally {
      cleanupApp(root);
      root.remove();
      globalThis.fetch = originalFetch;
    }
  });

  it('downloads a blob from the detail page', async () => {
    const originalCreateObjectURL = URL.createObjectURL;
    const originalRevokeObjectURL = URL.revokeObjectURL;
    const createdUrls: string[] = [];

    URL.createObjectURL = ((blob: Blob) => {
      void blob;
      createdUrls.push('blob:download');
      return 'blob:download';
    }) as typeof URL.createObjectURL;
    URL.revokeObjectURL = (() => undefined) as typeof URL.revokeObjectURL;

    globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
      const request =
        typeof input === 'string' || input instanceof URL
          ? new Request(input, init)
          : input;
      const url = new URL(request.url, 'http://localhost');

      if (
        url.pathname === '/admin/v1/buckets/alpha/objects' &&
        request.method === 'GET'
      ) {
        return jsonResponse({
          items: [
            {
              key: 'docs/readme.txt',
              size: 5,
              etag: 'etag-uploaded',
              last_modified: '2026-05-25T11:00:00.000Z',
              content_type: 'text/plain',
              storage_class: 'standard',
            },
          ],
          next: null,
        });
      }

      if (
        url.pathname === '/admin/v1/buckets/alpha/objects/docs%2Freadme.txt' &&
        request.method === 'GET'
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
          '/admin/v1/buckets/alpha/objects/docs%2Freadme.txt/content' &&
        request.method === 'GET'
      ) {
        return new Response('hello', {
          status: 200,
          headers: { 'content-type': 'text/plain' },
        });
      }

      throw new Error(`Unexpected request: ${request.method} ${url.pathname}`);
    };

    const root = mount(() => (
      <BlobPage
        bucketName="alpha"
        blobId={blobIdFromBlobKey('docs/readme.txt')}
      />
    ));

    try {
      await flush();
      const downloadButton = Array.from(root.querySelectorAll('button')).find(
        (button) => button.textContent?.includes('Download blob')
      );
      click(downloadButton!);
      await flush();
      await flush();

      expect(createdUrls).toEqual(['blob:download']);
    } finally {
      cleanupApp(root);
      root.remove();
      URL.createObjectURL = originalCreateObjectURL;
      URL.revokeObjectURL = originalRevokeObjectURL;
      globalThis.fetch = originalFetch;
    }
  });
});
