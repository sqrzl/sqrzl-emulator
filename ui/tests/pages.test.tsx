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

function storageDialogFooter(): HTMLElement {
  const footer = document.querySelector(
    '[data-sqrzl-slot="storage-dialog-footer"]'
  );
  expect(footer).toBeTruthy();
  return footer as HTMLElement;
}

function storageDialogFooterButtonLabels(): string[] {
  return Array.from(storageDialogFooter().querySelectorAll('button')).map(
    (button) => button.textContent?.trim() ?? ''
  );
}

function storageDialogTitleText(): string {
  const title = document.querySelector(
    '[data-sqrzl-slot="storage-dialog-title"]'
  );
  expect(title).toBeTruthy();
  return title?.textContent?.trim() ?? '';
}

function storageDialogFormSequence(): string[] {
  const form = document.querySelector('form');
  expect(form).toBeTruthy();

  return Array.from(
    form!.querySelectorAll(
      '[role="alert"], [data-sqrzl-slot="storage-dialog-footer"]'
    )
  ).map((element) =>
    element.getAttribute('role') === 'alert' ? 'error' : 'footer'
  );
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
      expect(root.textContent).toContain('Sqrzl');
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

      expect(storageDialogTitleText()).toBe('Add bucket');
      expect(storageDialogFooterButtonLabels()).toEqual([
        'Cancel',
        'Create bucket',
      ]);
      expect(
        document.querySelector(
          '[data-sqrzl-slot="storage-dialog-footer"] [data-slot="button-group"]'
        )
      ).toBeNull();

      const emptyForm = document.querySelector('form') as HTMLFormElement;
      emptyForm.dispatchEvent(
        new Event('submit', { bubbles: true, cancelable: true })
      );
      await flush();
      expect(document.body.textContent).toContain('Bucket name is required.');
      expect(storageDialogFormSequence()).toEqual(['error', 'footer']);

      const input = document.querySelector('#bucket-name') as HTMLInputElement;
      input.focus();
      expect(document.activeElement).toBe(input);
      input.value = 'alpha';
      input.dispatchEvent(new Event('input', { bubbles: true }));
      await flush();
      expect(document.activeElement).toBe(input);
      const submitButton = Array.from(
        document.querySelectorAll(
          '[data-sqrzl-slot="storage-dialog-footer"] button'
        )
      ).find((button) => button.textContent?.trim() === 'Create bucket');
      click(submitButton!);

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
      expect(storageDialogTitleText()).toBe('Delete bucket');
      expect(storageDialogFooterButtonLabels()).toEqual([
        'Cancel',
        'Delete bucket and 2 blobs',
      ]);
      expect(
        document.querySelector(
          '[data-sqrzl-slot="storage-dialog-footer"] [data-slot="button-group"]'
        )
      ).toBeNull();

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

      expect(storageDialogTitleText()).toBe('Add blob');
      expect(storageDialogFooterButtonLabels()).toEqual([
        'Cancel',
        'Upload blob',
      ]);
      expect(
        document.querySelector(
          '[data-sqrzl-slot="storage-dialog-footer"] [data-slot="button-group"]'
        )
      ).toBeNull();

      const emptyForm = document.querySelector('form') as HTMLFormElement;
      emptyForm.dispatchEvent(
        new Event('submit', { bubbles: true, cancelable: true })
      );
      await flush();
      expect(document.body.textContent).toContain('Choose a file to upload.');
      expect(storageDialogFormSequence()).toEqual(['error', 'footer']);

      const keyInput = document.querySelector('#blob-key') as HTMLInputElement;
      const fileInput = document.querySelector(
        '#blob-file'
      ) as HTMLInputElement;
      const file = new File(['hello'], 'readme.txt', { type: 'text/plain' });

      keyInput.value = 'docs/readme.txt';
      keyInput.dispatchEvent(new Event('input', { bubbles: true }));
      Object.defineProperty(fileInput, 'files', {
        configurable: true,
        value: [file],
      });
      fileInput.dispatchEvent(new Event('input', { bubbles: true }));
      const submitButton = Array.from(
        document.querySelectorAll(
          '[data-sqrzl-slot="storage-dialog-footer"] button'
        )
      ).find((button) => button.textContent?.trim() === 'Upload blob');
      click(submitButton!);

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

  it('renders slash-delimited blob keys as folders and current path blobs', async () => {
    globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
      const request =
        typeof input === 'string' || input instanceof URL
          ? new Request(input, init)
          : input;
      const url = new URL(request.url, 'http://localhost');

      if (
        url.pathname === '/admin/v1/buckets/foldered/objects' &&
        request.method === 'GET'
      ) {
        const prefix = url.searchParams.get('prefix');
        if (prefix === 'docs/') {
          return jsonResponse({
            folders: [{ name: 'api/', prefix: 'docs/api/' }],
            items: [
              {
                key: 'docs/readme.txt',
                size: 5,
                etag: 'etag-readme',
                last_modified: '2026-05-25T11:00:00.000Z',
                content_type: 'text/plain',
                storage_class: 'standard',
              },
            ],
            next: null,
          });
        }

        if (prefix === 'docs/api/') {
          return jsonResponse({
            folders: [],
            items: [
              {
                key: 'docs/api/openapi.json',
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

        return jsonResponse({
          folders: [{ name: 'docs/', prefix: 'docs/' }],
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
          next: null,
        });
      }

      throw new Error(`Unexpected request: ${request.method} ${url.pathname}`);
    };

    const root = mount(() => <BucketPage bucketName="foldered" />);

    try {
      await flush();
      expect(root.textContent).toContain('docs/');
      expect(root.textContent).toContain('Folder');
      expect(root.textContent).toContain('image.png');
      expect(root.textContent).not.toContain('readme.txt');
      expect(
        root.querySelector('a[href="/admin/buckets/foldered/docs"]')
      ).toBeTruthy();
    } finally {
      cleanupApp(root);
      root.remove();
    }

    const nestedRoot = mount(() => (
      <BucketPage bucketName="foldered" pathPrefix="docs" />
    ));

    try {
      await flush();
      expect(nestedRoot.textContent).toContain('readme.txt');
      expect(nestedRoot.textContent).toContain('api/');
      expect(nestedRoot.textContent).toContain('foldered');
      expect(nestedRoot.textContent).toContain('docs');
      expect(nestedRoot.textContent).not.toContain('image.png');
      expect(nestedRoot.textContent).not.toContain('openapi.json');
    } finally {
      cleanupApp(nestedRoot);
      nestedRoot.remove();
    }

    const deepRoot = mount(() => (
      <BucketPage bucketName="foldered" pathPrefix="docs/api" />
    ));

    try {
      await flush();
      expect(deepRoot.textContent).toContain('openapi.json');
      expect(deepRoot.textContent).toContain('foldered');
      expect(deepRoot.textContent).toContain('docs');
      expect(deepRoot.textContent).toContain('api');
      expect(deepRoot.textContent).not.toContain('readme.txt');
      expect(deepRoot.textContent).not.toContain('image.png');
    } finally {
      cleanupApp(deepRoot);
      deepRoot.remove();
      globalThis.fetch = originalFetch;
    }
  });

  it('uploads new blobs into the current bucket path by default', async () => {
    let uploadedKey = '';

    globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
      const request =
        typeof input === 'string' || input instanceof URL
          ? new Request(input, init)
          : input;
      const url = new URL(request.url, 'http://localhost');

      if (
        url.pathname === '/admin/v1/buckets/nested/objects' &&
        request.method === 'GET'
      ) {
        return jsonResponse({ items: [], next: null });
      }

      if (
        url.pathname ===
          '/admin/v1/buckets/nested/objects/docs%2Freadme.txt/content' &&
        request.method === 'PUT'
      ) {
        uploadedKey = 'docs/readme.txt';
        return jsonResponse(
          {
            key: uploadedKey,
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

    const root = mount(() => (
      <BucketPage bucketName="nested" pathPrefix="docs" />
    ));

    try {
      await flush();

      const addButton = Array.from(root.querySelectorAll('button')).find(
        (button) => button.textContent?.includes('Add blob')
      );
      click(addButton!);
      await flush();
      expect(document.body.textContent).toContain(
        'Without a key, the file name is placed in docs/.'
      );

      const fileInput = document.querySelector(
        '#blob-file'
      ) as HTMLInputElement;
      const form = document.querySelector('form') as HTMLFormElement;
      const file = new File(['hello'], 'readme.txt', { type: 'text/plain' });

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
      expect(uploadedKey).toBe('docs/readme.txt');
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
      expect(storageDialogTitleText()).toBe('Delete blob');
      expect(storageDialogFooterButtonLabels()).toEqual([
        'Cancel',
        'Delete blob',
      ]);
      expect(
        document.querySelector(
          '[data-sqrzl-slot="storage-dialog-footer"] [data-slot="button-group"]'
        )
      ).toBeNull();

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

  it('keeps blob search focused while syncing keystrokes to the url', async () => {
    const originalUrl = `${window.location.pathname}${window.location.search}${window.location.hash}`;
    const objectListRequests: string[] = [];

    window.history.pushState(null, '', '/admin/buckets/alpha');
    globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
      const request =
        typeof input === 'string' || input instanceof URL
          ? new Request(input, init)
          : input;
      const url = new URL(request.url, 'http://localhost');
      const search = url.searchParams.get('search');

      if (
        url.pathname === '/admin/v1/buckets/alpha/objects' &&
        request.method === 'GET'
      ) {
        objectListRequests.push(url.search);
        return jsonResponse({
          folders: [],
          items:
            search === 'notes'
              ? [
                  {
                    key: 'notes.txt',
                    size: 18,
                    etag: 'etag-notes',
                    last_modified: '2026-05-25T08:35:00.000Z',
                    content_type: 'text/plain',
                    storage_class: 'standard',
                  },
                ]
              : [
                  {
                    key: 'image.png',
                    size: 12,
                    etag: 'etag-image',
                    last_modified: '2026-05-25T08:30:00.000Z',
                    content_type: 'image/png',
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

    const root = mount(() => <BucketPage bucketName="alpha" />);

    try {
      await flush();
      const searchInput = root.querySelector(
        '#blob-search'
      ) as HTMLInputElement;
      expect(searchInput).toBeTruthy();

      searchInput.focus();
      searchInput.value = 'notes';
      searchInput.dispatchEvent(new Event('input', { bubbles: true }));

      await flush();
      await flush();

      expect(document.activeElement).toBe(searchInput);
      expect(searchInput.value).toBe('notes');
      expect(window.location.search).toBe('?search=notes');
      expect(
        objectListRequests.some((request) => request.includes('search=notes'))
      ).toBe(true);
      expect(root.textContent).toContain('notes.txt');
    } finally {
      cleanupApp(root);
      root.remove();
      window.history.pushState(null, '', originalUrl || '/');
      globalThis.fetch = originalFetch;
    }
  });

  it('renders blob metadata', async () => {
    const objectListRequests: string[] = [];

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
        objectListRequests.push(url.search);
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
      expect(objectListRequests).toHaveLength(1);
    } finally {
      cleanupApp(root);
      root.remove();
      globalThis.fetch = originalFetch;
    }
  });

  it('resolves blob detail directly from a key hint and avoids object listing', async () => {
    const blobKey = 'docs/readme.txt';
    const blobId = blobIdFromBlobKey(blobKey);
    let objectsListed = false;

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
        objectsListed = true;
        throw new Error(
          'Object collection should not be requested with key hint'
        );
      }

      if (
        url.pathname === '/admin/v1/buckets/alpha/objects/docs%2Freadme.txt' &&
        request.method === 'GET'
      ) {
        return jsonResponse({
          key: blobKey,
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

    const originalUrl = `${window.location.pathname}${window.location.search}${window.location.hash}`;
    window.history.pushState(
      null,
      '',
      `/admin/blobs/alpha/${blobId}?key=${encodeURIComponent(blobKey)}`
    );

    const root = mount(() => <BlobPage bucketName="alpha" blobId={blobId} />);

    try {
      await flush();
      expect(objectsListed).toBe(false);
      expect(root.textContent).toContain('docs/readme.txt');
      expect(root.textContent).toContain('owner');
    } finally {
      cleanupApp(root);
      root.remove();
      window.history.pushState(null, '', originalUrl || '/');
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
