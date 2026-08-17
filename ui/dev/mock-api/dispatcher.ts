import type {
  Acl,
  BucketPolicyDocument,
  InboundTextSimulationRequest,
  LifecycleConfiguration,
  TextDeliveryRequest,
} from '@/adapters/api.g';
import { fixtureTime } from './fixtures';
import type {
  MockBucket,
  MockMailMessage,
  MockObject,
  MockRequest,
  MockResponse,
  MockState,
  MockTextMessage,
} from './types';

const SESSION_COOKIE = 'sqrzl_admin_session';
const PAGE_SIZE = 3;
const jsonHeaders = { 'content-type': 'application/json; charset=utf-8' };
const ok = (body: unknown, status = 200): MockResponse => ({
  status,
  headers: jsonHeaders,
  body,
});
const error = (
  status: number,
  code: string,
  message: string,
  details?: string
): MockResponse =>
  ok({ code, error: message, details: details ?? null }, status);
const truth = () => ok(true);

function page<T>(items: T[], url: URL): { items: T[]; next: string | null } {
  const raw = url.searchParams.get('next');
  const offset = raw?.startsWith('mock:') ? Number(raw.slice(5)) : 0;
  const start = Number.isSafeInteger(offset) && offset >= 0 ? offset : 0;
  const sliced = items.slice(start, start + PAGE_SIZE);
  return {
    items: sliced,
    next: start + PAGE_SIZE < items.length ? `mock:${start + PAGE_SIZE}` : null,
  };
}

function search(url: URL): string {
  return (url.searchParams.get('search') ?? '').trim().toLocaleLowerCase();
}

function includes(value: unknown, needle: string): boolean {
  return JSON.stringify(value).toLocaleLowerCase().includes(needle);
}

function binary(
  bytes: Uint8Array,
  type: string,
  filename: string
): MockResponse {
  return {
    status: 200,
    headers: {
      'content-type': type,
      'content-length': String(bytes.byteLength),
      'content-disposition': `attachment; filename="${filename.replace(/"/gu, '')}"`,
    },
    body: bytes,
  };
}

function bucketOr404(
  state: MockState,
  name: string
): MockBucket | MockResponse {
  return (
    state.buckets.get(name) ??
    error(404, 'bucket_not_found', 'Bucket not found', name)
  );
}

function objectOr404(
  bucket: MockBucket,
  key: string
): MockObject | MockResponse {
  return (
    bucket.objects.get(key) ??
    error(404, 'object_not_found', 'Object not found', key)
  );
}

function messageOr404(
  state: MockState,
  mailbox: string,
  id: string
): MockMailMessage | MockResponse {
  return (
    state.mail.get(mailbox)?.get(id) ??
    error(404, 'message_not_found', 'Message not found', id)
  );
}

function textOr404(
  state: MockState,
  peer: string,
  id: string
): MockTextMessage | MockResponse {
  return (
    state.texts.get(peer)?.find((item) => item.message_id === id) ??
    error(404, 'text_message_not_found', 'Text message not found', id)
  );
}

function isResponse(value: unknown): value is MockResponse {
  return Boolean(value && typeof value === 'object' && 'status' in value);
}

function auth(
  request: MockRequest,
  state: MockState
): MockResponse | undefined {
  const token = request.cookies?.[SESSION_COOKIE];
  if (!token || !state.sessions.has(token)) {
    return error(401, 'unauthorized', 'Authentication required');
  }
}

function objectInfo(item: MockObject) {
  const { key, content_type, etag, last_modified, size, storage_class } = item;
  return { key, content_type, etag, last_modified, size, storage_class };
}

function handleBuckets(
  request: MockRequest,
  state: MockState,
  url: URL,
  segments: string[]
): MockResponse | undefined {
  if (segments[0] !== 'buckets') return;
  if (segments.length === 1 && request.method === 'GET') {
    const needle = search(url);
    const items = [...state.buckets.values()]
      .map((item) => item.info)
      .filter((item) => !needle || includes(item, needle))
      .sort((a, b) => b.created_at.localeCompare(a.created_at));
    return ok(page(items, url));
  }
  if (segments.length === 1 && request.method === 'POST') {
    const name = String(
      (request.json as { name?: unknown } | undefined)?.name ?? ''
    ).trim();
    if (!name)
      return error(400, 'invalid_bucket_name', 'Bucket name is required');
    if (state.buckets.has(name))
      return error(409, 'bucket_exists', 'Bucket already exists', name);
    const info = {
      name,
      created_at: fixtureTime(++state.sequence),
      versioning_enabled: false,
    };
    state.buckets.set(name, {
      info,
      objects: new Map(),
      acl: { canned: 'private' },
      uploads: [],
    });
    return ok(info, 201);
  }
  const name = segments[1];
  if (!name) return;
  const resolved = bucketOr404(state, name);
  if (isResponse(resolved)) return resolved;
  if (segments.length === 2) {
    if (request.method === 'GET') return ok(resolved.info);
    if (request.method === 'DELETE') {
      if (resolved.objects.size)
        return error(409, 'bucket_not_empty', 'Bucket is not empty', name);
      state.buckets.delete(name);
      return truth();
    }
  }
  const resource = segments[2];
  if (resource === 'versioning' && segments.length === 3) {
    if (request.method === 'GET')
      return ok({ enabled: resolved.info.versioning_enabled });
    if (request.method === 'PUT') {
      resolved.info.versioning_enabled = Boolean(
        (request.json as { enabled?: boolean })?.enabled
      );
      return ok({ enabled: resolved.info.versioning_enabled });
    }
  }
  if (resource === 'acl' && segments.length === 3) {
    if (request.method === 'GET') return ok(resolved.acl);
    if (request.method === 'PUT')
      return ok((resolved.acl = request.json as Acl));
  }
  if (resource === 'policy' && segments.length === 3) {
    if (request.method === 'GET')
      return resolved.policy
        ? ok(resolved.policy)
        : error(404, 'policy_not_found', 'Bucket policy not found');
    if (request.method === 'PUT')
      return ok((resolved.policy = request.json as BucketPolicyDocument));
    if (request.method === 'DELETE') {
      resolved.policy = undefined;
      return truth();
    }
  }
  if (resource === 'lifecycle' && segments.length === 3) {
    if (request.method === 'GET')
      return resolved.lifecycle
        ? ok(resolved.lifecycle)
        : error(
            404,
            'lifecycle_not_found',
            'Lifecycle configuration not found'
          );
    if (request.method === 'PUT')
      return ok((resolved.lifecycle = request.json as LifecycleConfiguration));
    if (request.method === 'DELETE') {
      resolved.lifecycle = undefined;
      return truth();
    }
  }
  if (resource === 'multipart-uploads') {
    if (segments.length === 3 && request.method === 'GET')
      return ok(page(resolved.uploads, url));
    const upload = resolved.uploads.find(
      (item) => item.upload_id === segments[3]
    );
    if (!upload)
      return error(
        404,
        'upload_not_found',
        'Multipart upload not found',
        segments[3]
      );
    if (request.method === 'GET') return ok(upload);
    if (request.method === 'DELETE') {
      resolved.uploads = resolved.uploads.filter((item) => item !== upload);
      return truth();
    }
  }
  if (resource !== 'objects') return;
  if (segments.length === 3 && request.method === 'GET') {
    const prefix = url.searchParams.get('prefix') ?? '';
    const needle = search(url);
    const candidates = [...resolved.objects.values()].filter((item) =>
      item.key.startsWith(prefix)
    );
    const folders = new Map<string, { name: string; prefix: string }>();
    const items = candidates.filter((item) => {
      const remainder = item.key.slice(prefix.length);
      const slash = remainder.indexOf('/');
      if (slash >= 0) {
        const folderName = remainder.slice(0, slash);
        folders.set(folderName, {
          name: folderName,
          prefix: `${prefix}${folderName}/`,
        });
        return false;
      }
      return !needle || includes(item, needle);
    });
    const result = page(items.map(objectInfo), url);
    return ok({
      ...result,
      folders: [...folders.values()].filter(
        (folder) => !needle || includes(folder, needle)
      ),
    });
  }
  const key = segments[3];
  if (!key) return;
  if (segments[4] === 'content' && request.method === 'PUT') {
    const existing = resolved.objects.get(key);
    const content = request.body ?? new Uint8Array();
    const metadata = Object.fromEntries(
      Object.entries(request.headers ?? {})
        .filter(([header]) => header.toLowerCase().startsWith('x-amz-meta-'))
        .map(([header, value]) => [header.slice(11), value ?? ''])
    );
    const now = fixtureTime(++state.sequence);
    const item: MockObject = {
      key,
      bytes: content,
      size: content.byteLength,
      content_type:
        request.headers?.['content-type'] ?? 'application/octet-stream',
      etag: `"mock-${state.sequence}-${content.byteLength}"`,
      last_modified: now,
      metadata,
      storage_class: 'STANDARD',
      version_id: `v-${state.sequence}`,
      tags: existing?.tags ?? {},
      acl: existing?.acl ?? { canned: 'private' },
      versions: existing?.versions ?? [],
    };
    item.versions = [
      {
        key,
        version_id: item.version_id!,
        is_latest: true,
        etag: item.etag,
        last_modified: now,
        size: item.size,
      },
      ...item.versions.map((v) => ({ ...v, is_latest: false })),
    ];
    resolved.objects.set(key, item);
    return ok(item, existing ? 200 : 201);
  }
  const item = objectOr404(resolved, key);
  if (isResponse(item)) return item;
  if (segments.length === 4) {
    if (request.method === 'GET') return ok(item);
    if (request.method === 'DELETE') {
      resolved.objects.delete(key);
      return truth();
    }
  }
  if (segments[4] === 'content' && request.method === 'GET') {
    const keyParts = key.split('/');
    return binary(
      item.bytes,
      item.content_type ?? 'application/octet-stream',
      keyParts[keyParts.length - 1] ?? key
    );
  }
  if (segments[4] === 'tags') {
    if (request.method === 'GET') return ok({ tags: item.tags });
    if (request.method === 'PUT') {
      item.tags = (request.json as { tags: Record<string, string> }).tags;
      return ok({ tags: item.tags });
    }
  }
  if (segments[4] === 'acl') {
    if (request.method === 'GET') return ok(item.acl);
    if (request.method === 'PUT') return ok((item.acl = request.json as Acl));
  }
  if (segments[4] === 'versions') {
    if (segments.length === 5 && request.method === 'GET')
      return ok(page(item.versions, url));
    if (request.method === 'DELETE') {
      item.versions = item.versions.filter(
        (version) => version.version_id !== segments[5]
      );
      return truth();
    }
  }
}

function handleMail(
  request: MockRequest,
  state: MockState,
  url: URL,
  segments: string[]
): MockResponse | undefined {
  if (segments[0] !== 'mailboxes') return;
  if (segments.length === 1 && request.method === 'GET') {
    const needle = search(url);
    const items = [...state.mail.entries()]
      .map(([address, messages]) => {
        const latest = [...messages.values()].sort((a, b) =>
          b.received_at.localeCompare(a.received_at)
        )[0];
        return {
          address,
          message_count: messages.size,
          last_received_at: latest?.received_at ?? null,
        };
      })
      .filter((item) => !needle || includes(item, needle));
    return ok(page(items, url));
  }
  const mailbox = segments[1];
  const messages = state.mail.get(mailbox);
  if (!messages)
    return error(404, 'mailbox_not_found', 'Mailbox not found', mailbox);
  if (segments.length === 2 && request.method === 'DELETE') {
    state.mail.delete(mailbox);
    return truth();
  }
  if (segments[2] !== 'messages') return;
  if (segments.length === 3 && request.method === 'GET') {
    const needle = search(url);
    const items = [...messages.values()]
      .filter((item) => !needle || includes(item, needle))
      .sort((a, b) => b.received_at.localeCompare(a.received_at))
      .map(
        ({ message_id, received_at, delivery_state, subject, from, to }) => ({
          message_id,
          received_at,
          delivery_state,
          subject,
          from,
          to,
        })
      );
    return ok(page(items, url));
  }
  const item = messageOr404(state, mailbox, segments[3]);
  if (isResponse(item)) return item;
  if (segments.length === 4) {
    if (request.method === 'GET') return ok(item);
    if (request.method === 'DELETE') {
      messages.delete(item.message_id);
      return truth();
    }
  }
  if (segments[4] === 'content' && request.method === 'GET')
    return binary(item.raw, 'message/rfc822', `${item.message_id}.eml`);
  if (segments[4] === 'attachments' && request.method === 'GET') {
    const content = item.attachmentBytes.get(segments[5]);
    if (!content)
      return error(
        404,
        'attachment_not_found',
        'Attachment not found',
        segments[5]
      );
    const type =
      item.attachments.find((a) => a.filename === segments[5])?.content_type ??
      'application/octet-stream';
    return binary(content, type, segments[5]);
  }
}

function handleTexts(
  request: MockRequest,
  state: MockState,
  url: URL,
  segments: string[]
): MockResponse | undefined {
  if (segments[0] === 'text-conversations') {
    if (segments.length === 1 && request.method === 'GET') {
      const needle = search(url);
      const items = [...state.texts.entries()]
        .map(([peer, messages]) => {
          const sorted = [...messages].sort((a, b) =>
            b.created_at.localeCompare(a.created_at)
          );
          const latest = sorted[0];
          return {
            peer,
            provider: latest.provider,
            message_count: messages.length,
            last_message_at: latest.created_at,
            last_message_body: latest.body,
            last_direction: latest.direction,
          };
        })
        .filter((item) => !needle || includes(item, needle))
        .sort((a, b) => b.last_message_at.localeCompare(a.last_message_at));
      return ok(page(items, url));
    }
    const peer = segments[1];
    const messages = state.texts.get(peer);
    if (!messages)
      return error(
        404,
        'conversation_not_found',
        'Text conversation not found',
        peer
      );
    if (segments.length === 2 && request.method === 'DELETE') {
      state.texts.delete(peer);
      return truth();
    }
    if (segments[2] !== 'messages') return;
    if (segments.length === 3 && request.method === 'GET') {
      const needle = search(url);
      return ok(
        page(
          messages
            .filter((item) => !needle || includes(item, needle))
            .sort((a, b) => b.created_at.localeCompare(a.created_at)),
          url
        )
      );
    }
    const item = textOr404(state, peer, segments[3]);
    if (isResponse(item)) return item;
    if (segments.length === 4) {
      if (request.method === 'GET') return ok(item);
      if (request.method === 'DELETE') {
        state.texts.set(
          peer,
          messages.filter((message) => message !== item)
        );
        return truth();
      }
    }
    if (segments[4] === 'media' && request.method === 'GET') {
      const content = item.mediaBytes.get(segments[5]);
      if (!content)
        return error(
          404,
          'media_not_found',
          'Local media not found',
          segments[5]
        );
      const media = item.media.find(
        (candidate) => candidate.media_id === segments[5]
      );
      return binary(
        content,
        media?.content_type ?? 'application/octet-stream',
        media?.filename ?? segments[5]
      );
    }
  }
  if (segments[0] === 'text-destinations' && segments.length === 3) {
    const key = `${segments[1]}:${segments[2]}`;
    if (request.method === 'GET')
      return state.destinations.has(key)
        ? ok(state.destinations.get(key))
        : error(404, 'destination_not_found', 'Text destination not found');
    if (request.method === 'PUT') {
      const old = state.destinations.get(key);
      const now = fixtureTime(++state.sequence);
      const destination = {
        provider: segments[1],
        local_number: segments[2],
        callback_url: String(
          (request.json as { callback_url?: unknown })?.callback_url ?? ''
        ),
        created_at: old?.created_at ?? now,
        updated_at: now,
      };
      state.destinations.set(key, destination as never);
      return ok(destination, old ? 200 : 201);
    }
    if (request.method === 'DELETE') {
      state.destinations.delete(key);
      return truth();
    }
  }
  if (
    segments[0] === 'text-simulations' &&
    segments[1] === 'inbound' &&
    request.method === 'POST'
  ) {
    const input = request.json as InboundTextSimulationRequest;
    if (!input?.from || !input.to || !input.provider)
      return error(
        400,
        'invalid_simulation',
        'From, to, and provider are required'
      );
    const id = `sim-${++state.sequence}`;
    const now = fixtureTime(state.sequence);
    const mediaBytes = new Map<string, Uint8Array>();
    const media = (input.media ?? []).map((entry, index) => {
      const mediaId = `media-${id}-${index + 1}`;
      const content = Uint8Array.from(atob(entry.content_base64), (char) =>
        char.charCodeAt(0)
      );
      mediaBytes.set(mediaId, content);
      return {
        media_id: mediaId,
        filename: entry.filename,
        content_type: entry.content_type,
        size: content.byteLength,
        external_url: null,
      };
    });
    const destination = state.destinations.get(`${input.provider}:${input.to}`);
    const attempts = destination
      ? [
          {
            attempt_id: `attempt-${++state.sequence}`,
            attempted_at: fixtureTime(state.sequence),
            kind: 'inbound' as const,
            message_id: id,
            provider: input.provider,
            request_body: JSON.stringify(input),
            request_headers: { 'content-type': 'application/json' },
            response_body: null,
            response_status: null,
            retry_of: null,
            state: 'failed' as const,
            error: 'Mock mode does not make callback requests',
            url: destination.callback_url,
          },
        ]
      : [];
    const item: MockTextMessage = {
      peer: input.from,
      message_id: id,
      provider_message_id: `${input.provider}-${id}`,
      provider: input.provider,
      direction: 'inbound',
      channel: media.length ? 'mms' : 'sms',
      from: input.from,
      to: input.to,
      body: input.body,
      media,
      delivery_state: 'delivered',
      metadata: input.metadata ?? {},
      batch_id: null,
      created_at: now,
      updated_at: now,
      callback_attempts: attempts,
      mediaBytes,
    };
    state.texts.set(input.from, [...(state.texts.get(input.from) ?? []), item]);
    return ok(item, 201);
  }
  if (
    segments[0] === 'text-messages' &&
    segments[2] === 'delivery' &&
    request.method === 'POST'
  ) {
    const item = [...state.texts.values()]
      .flat()
      .find((candidate) => candidate.message_id === segments[1]);
    if (!item)
      return error(
        404,
        'text_message_not_found',
        'Text message not found',
        segments[1]
      );
    if (item.direction !== 'outbound')
      return error(
        409,
        'invalid_delivery_transition',
        'Only outbound messages can transition'
      );
    item.delivery_state = (request.json as TextDeliveryRequest).state;
    item.updated_at = fixtureTime(++state.sequence);
    return ok(item);
  }
  if (
    segments[0] === 'text-callback-attempts' &&
    segments[2] === 'retry' &&
    request.method === 'POST'
  ) {
    for (const item of [...state.texts.values()].flat()) {
      const original = item.callback_attempts.find(
        (attempt) => attempt.attempt_id === segments[1]
      );
      if (original) {
        const retry = {
          ...original,
          attempt_id: `attempt-${++state.sequence}`,
          attempted_at: fixtureTime(state.sequence),
          retry_of: original.attempt_id,
        };
        item.callback_attempts.push(retry);
        return ok(retry, 201);
      }
    }
    return error(
      404,
      'callback_attempt_not_found',
      'Callback attempt not found',
      segments[1]
    );
  }
}

export function dispatchMockRequest(
  request: MockRequest,
  state: MockState
): MockResponse {
  const method = request.method.toUpperCase();
  const normalized = { ...request, method };
  const url = new URL(request.url, 'http://mock.sqrzl.test');
  if (!url.pathname.startsWith('/admin/v1'))
    return error(
      404,
      'mock_route_not_found',
      'Mock API route not found',
      url.pathname
    );
  const segments = url.pathname
    .slice('/admin/v1/'.length)
    .split('/')
    .filter(Boolean)
    .map(decodeURIComponent);
  if (segments[0] === 'auth') {
    if (segments[1] === 'login' && method === 'POST') {
      const input = request.json as
        | { username?: string; password?: string }
        | undefined;
      if (input?.username !== 'admin' || input.password !== 'sqrzl-secret')
        return error(
          401,
          'invalid_credentials',
          'Invalid username or password'
        );
      const token = `mock-session-${++state.sequence}`;
      state.sessions.add(token);
      return {
        ...ok({ success: true }),
        headers: {
          ...jsonHeaders,
          'set-cookie': `${SESSION_COOKIE}=${token}; Path=/; HttpOnly; SameSite=Lax`,
        },
      };
    }
    if (segments[1] === 'session' && method === 'GET') {
      const denied = auth(normalized, state);
      return denied ?? ok({ mode: 'session', username: 'admin' });
    }
    if (segments[1] === 'logout' && method === 'POST') {
      const denied = auth(normalized, state);
      if (denied) return denied;
      const token = request.cookies?.[SESSION_COOKIE];
      if (token) state.sessions.delete(token);
      return {
        ...ok({ success: true }),
        headers: {
          ...jsonHeaders,
          'set-cookie': `${SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0`,
        },
      };
    }
  }
  const denied = auth(normalized, state);
  if (denied) return denied;
  return (
    handleBuckets(normalized, state, url, segments) ??
    handleMail(normalized, state, url, segments) ??
    handleTexts(normalized, state, url, segments) ??
    error(
      404,
      'mock_route_not_found',
      'Mock API route not found',
      `${method} ${url.pathname}`
    )
  );
}
