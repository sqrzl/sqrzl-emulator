#!/usr/bin/env node

const DEFAULT_ADMIN_URL = 'http://127.0.0.1:9001/admin/v1';
const DEFAULT_USERNAME = 'admin';
const DEFAULT_PASSWORD = 'sqrzl-secret';

const sampleBuckets = ['sqrzl-demo', 'sqrzl-logs', 'sqrzl-archive'];

const sampleObjects = [
  {
    bucket: 'sqrzl-demo',
    key: 'readme.md',
    contentType: 'text/markdown; charset=utf-8',
    metadata: {
      owner: 'platform',
      environment: 'local',
      purpose: 'ui-demo',
    },
    body: `# Sqrzl demo bucket

This bucket gives the local admin UI enough objects to browse.

- Nested docs exercise folder navigation.
- Reports include JSON and CSV content types.
- Objects include sample custom metadata.
`,
  },
  {
    bucket: 'sqrzl-demo',
    key: 'docs/admin-api.md',
    contentType: 'text/markdown; charset=utf-8',
    metadata: {
      owner: 'storage',
      environment: 'local',
      document: 'admin-api',
    },
    body: `# Admin API notes

The UI uses the versioned /admin/v1 surface for bucket and object workflows.
This file exists so the browser has a nested markdown object to inspect.
`,
  },
  {
    bucket: 'sqrzl-demo',
    key: 'docs/changelog-2026-06.md',
    contentType: 'text/markdown; charset=utf-8',
    metadata: {
      owner: 'release',
      environment: 'local',
      month: '2026-06',
    },
    body: `# June 2026 local changelog

- Added local development admin UI auth bypass.
- Added focus-safe storage forms.
- Added sample data for storage UI review.
`,
  },
  {
    bucket: 'sqrzl-demo',
    key: 'reports/2026/q2/storage-summary.csv',
    contentType: 'text/csv; charset=utf-8',
    metadata: {
      owner: 'analytics',
      environment: 'local',
      report: 'storage-summary',
    },
    body: `bucket,objects,total_bytes,last_checked
sqrzl-demo,6,4200,2026-06-28T14:30:00Z
sqrzl-logs,3,1180,2026-06-28T14:30:00Z
sqrzl-archive,3,2600,2026-06-28T14:30:00Z
`,
  },
  {
    bucket: 'sqrzl-demo',
    key: 'reports/2026/q2/inventory.json',
    contentType: 'application/json; charset=utf-8',
    metadata: {
      owner: 'analytics',
      environment: 'local',
      report: 'inventory',
    },
    body: JSON.stringify(
      {
        generated_at: '2026-06-28T14:30:00Z',
        buckets: [
          { name: 'sqrzl-demo', objects: 6, status: 'active' },
          { name: 'sqrzl-logs', objects: 3, status: 'active' },
          { name: 'sqrzl-archive', objects: 3, status: 'cold' },
        ],
      },
      null,
      2
    ),
  },
  {
    bucket: 'sqrzl-demo',
    key: 'images/sqrzl-mark.svg',
    contentType: 'image/svg+xml; charset=utf-8',
    metadata: {
      owner: 'design',
      environment: 'local',
      asset: 'mark',
    },
    body: `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 160 96" role="img" aria-label="Sqrzl sample mark">
  <rect width="160" height="96" rx="8" fill="#f8fafc"/>
  <path d="M24 50c21-28 56-34 112-18" fill="none" stroke="#334155" stroke-width="8" stroke-linecap="round"/>
  <circle cx="46" cy="54" r="12" fill="#16a34a"/>
  <circle cx="82" cy="42" r="12" fill="#0ea5e9"/>
  <circle cx="118" cy="36" r="12" fill="#f59e0b"/>
</svg>
`,
  },
  {
    bucket: 'sqrzl-logs',
    key: '2026/06/28/startup.log',
    contentType: 'text/plain; charset=utf-8',
    metadata: {
      owner: 'runtime',
      environment: 'local',
      stream: 'startup',
    },
    body: `2026-06-28T14:20:02Z INFO sqrzl-emulator starting
2026-06-28T14:20:02Z INFO api listening on 0.0.0.0:9000
2026-06-28T14:20:02Z INFO ui listening on 0.0.0.0:9001
`,
  },
  {
    bucket: 'sqrzl-logs',
    key: '2026/06/28/admin-access.log',
    contentType: 'text/plain; charset=utf-8',
    metadata: {
      owner: 'runtime',
      environment: 'local',
      stream: 'admin',
    },
    body: `2026-06-28T14:21:17Z GET /admin/v1/auth/session 200
2026-06-28T14:22:04Z GET /admin/v1/buckets 200
2026-06-28T14:22:18Z GET /admin/v1/buckets/sqrzl-demo/objects 200
`,
  },
  {
    bucket: 'sqrzl-logs',
    key: '2026/06/27/lifecycle.log',
    contentType: 'text/plain; charset=utf-8',
    metadata: {
      owner: 'runtime',
      environment: 'local',
      stream: 'lifecycle',
    },
    body: `2026-06-27T23:00:00Z INFO lifecycle sweep started
2026-06-27T23:00:00Z INFO lifecycle sweep finished objects_scanned=12 objects_expired=0
`,
  },
  {
    bucket: 'sqrzl-archive',
    key: 'backups/2026-06-28/manifest.json',
    contentType: 'application/json; charset=utf-8',
    metadata: {
      owner: 'backup',
      environment: 'local',
      retention: '30-days',
    },
    body: JSON.stringify(
      {
        backup_id: 'local-2026-06-28',
        generated_at: '2026-06-28T14:25:00Z',
        objects: [
          'readme.md',
          'docs/admin-api.md',
          'reports/2026/q2/inventory.json',
        ],
      },
      null,
      2
    ),
  },
  {
    bucket: 'sqrzl-archive',
    key: 'backups/2026-06-28/checksums.txt',
    contentType: 'text/plain; charset=utf-8',
    metadata: {
      owner: 'backup',
      environment: 'local',
      retention: '30-days',
    },
    body: `5e884898da28047151d0e56f8dc62927 readme.md
4e07408562bedb8b60ce05c1decfe3ad docs/admin-api.md
ef2d127de37b942baad06145e54b0c61 inventory.json
`,
  },
  {
    bucket: 'sqrzl-archive',
    key: 'exports/users.csv',
    contentType: 'text/csv; charset=utf-8',
    metadata: {
      owner: 'ops',
      environment: 'local',
      data_class: 'synthetic',
    },
    body: `id,name,role,last_active
1,alex,admin,2026-06-28
2,casey,developer,2026-06-27
3,jordan,observer,2026-06-26
`,
  },
];

let sessionCookie = '';

function normalizeAdminUrl(rawUrl) {
  const url = new URL(rawUrl || DEFAULT_ADMIN_URL);
  url.pathname = url.pathname.replace(/\/+$/, '');

  if (!url.pathname.endsWith('/admin/v1')) {
    url.pathname = `${url.pathname}/admin/v1`.replace(/\/{2,}/g, '/');
  }

  return url.toString().replace(/\/$/, '');
}

const adminUrl = normalizeAdminUrl(process.env.SQRZL_ADMIN_URL);

function adminEndpoint(path) {
  return `${adminUrl}${path.startsWith('/') ? path : `/${path}`}`;
}

function adminCredentials() {
  return {
    username:
      process.env.SQRZL_ADMIN_USERNAME ||
      process.env.SQRZL_ACCESS_KEY_ID ||
      DEFAULT_USERNAME,
    password:
      process.env.SQRZL_ADMIN_PASSWORD ||
      process.env.SQRZL_SECRET_ACCESS_KEY ||
      DEFAULT_PASSWORD,
  };
}

function cookieFromHeaders(headers) {
  const setCookies =
    typeof headers.getSetCookie === 'function' ? headers.getSetCookie() : [];
  const rawCookie = setCookies[0] || headers.get('set-cookie') || '';
  return rawCookie.split(';')[0] || '';
}

async function request(path, options = {}) {
  const headers = new Headers(options.headers);

  if (sessionCookie) {
    headers.set('cookie', sessionCookie);
  }

  return fetch(adminEndpoint(path), {
    ...options,
    headers,
  });
}

async function responseMessage(response) {
  const text = await response.text();

  if (!text) {
    return `${response.status} ${response.statusText}`;
  }

  try {
    const parsed = JSON.parse(text);
    return [parsed.error, parsed.details].filter(Boolean).join(': ') || text;
  } catch {
    return text;
  }
}

async function expectStatus(response, context, statuses) {
  if (statuses.includes(response.status)) {
    return;
  }

  throw new Error(
    `${context} failed (${response.status}): ${await responseMessage(response)}`
  );
}

async function ensureAdminSession() {
  const session = await request('/auth/session');

  if (session.ok) {
    const body = await session.json();
    console.log(`Admin session: ${body.mode}`);
    return;
  }

  if (session.status !== 401) {
    throw new Error(
      `Admin session check failed (${session.status}): ${await responseMessage(
        session
      )}`
    );
  }

  const credentials = adminCredentials();
  const login = await request('/auth/login', {
    method: 'POST',
    headers: {
      'content-type': 'application/json',
    },
    body: JSON.stringify(credentials),
  });

  await expectStatus(login, 'Admin login', [200]);
  sessionCookie = cookieFromHeaders(login.headers);

  if (!sessionCookie) {
    throw new Error(
      'Admin login succeeded but no session cookie was returned.'
    );
  }

  console.log(`Admin session: authenticated as ${credentials.username}`);
}

async function createBucket(name) {
  const response = await request('/buckets', {
    method: 'POST',
    headers: {
      'content-type': 'application/json',
    },
    body: JSON.stringify({ name }),
  });

  if (response.status === 409) {
    console.log(`= bucket ${name}`);
    return;
  }

  await expectStatus(response, `Create bucket ${name}`, [201]);
  console.log(`+ bucket ${name}`);
}

async function putObject(object) {
  const headers = new Headers({
    'content-type': object.contentType,
  });

  for (const [name, value] of Object.entries(object.metadata)) {
    headers.set(`x-amz-meta-${name}`, value);
  }

  const response = await request(
    `/buckets/${encodeURIComponent(object.bucket)}/objects/${encodeURIComponent(
      object.key
    )}/content`,
    {
      method: 'PUT',
      headers,
      body: object.body,
    }
  );

  await expectStatus(
    response,
    `Upload ${object.bucket}/${object.key}`,
    [200, 201]
  );

  console.log(
    `${response.status === 201 ? '+' : '='} object ${object.bucket}/${object.key}`
  );
}

async function main() {
  console.log(`Seeding sample data into ${adminUrl}`);
  await ensureAdminSession();

  for (const bucket of sampleBuckets) {
    await createBucket(bucket);
  }

  for (const object of sampleObjects) {
    await putObject(object);
  }

  console.log(
    `Seeded ${sampleBuckets.length} buckets and ${sampleObjects.length} objects.`
  );
}

main().catch((error) => {
  console.error(error instanceof Error ? error.message : error);
  process.exitCode = 1;
});
