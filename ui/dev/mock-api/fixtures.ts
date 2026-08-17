import type {
  MailAddress,
  MessageDetail,
  TextProvider,
} from '../../src/adapters/api.g';
import type {
  MockBucket,
  MockMailMessage,
  MockObject,
  MockState,
  MockTextMessage,
} from './types';

const encoder = new TextEncoder();
const bytes = (value: string) => encoder.encode(value);
const privateAcl = { canned: 'private' as const };
const baseTime = Date.parse('2026-08-17T14:00:00.000Z');
const time = (minutes: number) =>
  new Date(baseTime + minutes * 60_000).toISOString();

function object(
  key: string,
  content: string,
  contentType: string,
  minute: number,
  metadata: Record<string, string> = {}
): MockObject {
  const contentBytes = bytes(content);
  return {
    key,
    bytes: contentBytes,
    size: contentBytes.byteLength,
    content_type: contentType,
    etag: `"mock-${minute}-${contentBytes.byteLength}"`,
    last_modified: time(minute),
    metadata,
    storage_class: 'STANDARD',
    version_id: `v-${minute}`,
    tags: metadata.kind ? { kind: metadata.kind } : {},
    acl: privateAcl,
    versions: [
      {
        key,
        version_id: `v-${minute}`,
        is_latest: true,
        etag: `"mock-${minute}-${contentBytes.byteLength}"`,
        last_modified: time(minute),
        size: contentBytes.byteLength,
      },
    ],
  };
}

function bucket(
  name: string,
  minute: number,
  objects: MockObject[]
): MockBucket {
  return {
    info: {
      name,
      created_at: time(minute),
      versioning_enabled: name === 'versioned-archive',
    },
    objects: new Map(objects.map((item) => [item.key, item])),
    acl: privateAcl,
    uploads: [],
  };
}

const address = (email: string, name?: string): MailAddress => ({
  email,
  name,
});

function mail(
  mailbox: string,
  id: string,
  minute: number,
  source: MessageDetail['source_protocol'],
  state: MessageDetail['delivery_state'],
  subject: string,
  withAttachment = false
): MockMailMessage {
  const from = address(
    `${source}@example.test`,
    `${source.toUpperCase()} sender`
  );
  const to = [address(mailbox, 'Sqrzl demo')];
  const attachment = bytes(`fixture attachment for ${id}\n`);
  const rawText = `From: ${from.email}\r\nTo: ${mailbox}\r\nSubject: ${subject}\r\n\r\nFixture message ${id}.\r\n`;
  return {
    mailbox,
    message_id: id,
    received_at: time(minute),
    source_protocol: source,
    delivery_state: state,
    delivery_detail: state === 'bounced' ? 'Mailbox unavailable' : null,
    subject,
    from,
    to,
    cc: id.endsWith('2') ? [address('observer@example.test')] : [],
    bcc: [],
    headers: { 'x-sqrzl-fixture': id, 'message-id': `<${id}@example.test>` },
    body_text: `Plain text body for ${subject}.`,
    body_html: `<p>HTML body for <strong>${subject}</strong>.</p>`,
    thread_id: `thread-${source}`,
    attachments: withAttachment
      ? [
          {
            filename: 'walkthrough.txt',
            content_type: 'text/plain',
            size: attachment.byteLength,
          },
        ]
      : [],
    raw: bytes(rawText),
    attachmentBytes: withAttachment
      ? new Map([['walkthrough.txt', attachment]])
      : new Map(),
  };
}

function textMessage(
  peer: string,
  id: string,
  minute: number,
  provider: TextProvider,
  direction: 'inbound' | 'outbound',
  state: 'accepted' | 'delivered' | 'failed',
  body: string,
  mms = false,
  external = false
): MockTextMessage {
  const local = '+15557654321';
  const mediaContent = bytes('mock image bytes');
  const media = mms
    ? [
        {
          media_id: `media-${id}`,
          filename: 'squirrel.jpg',
          content_type: 'image/jpeg',
          size: external ? null : mediaContent.byteLength,
          external_url: external ? 'https://example.test/squirrel.jpg' : null,
        },
      ]
    : [];
  const base = {
    peer,
    message_id: id,
    provider_message_id: `${provider}-${id}`,
    provider,
    direction,
    channel: mms ? ('mms' as const) : ('sms' as const),
    from: direction === 'inbound' ? peer : local,
    to: direction === 'inbound' ? local : peer,
    body,
    media,
    delivery_state: state,
    metadata: { fixture: true },
    batch_id: null,
    created_at: time(minute),
    updated_at: time(minute + 1),
    callback_attempts: [],
    mediaBytes: external ? new Map() : new Map([[`media-${id}`, mediaContent]]),
  } satisfies MockTextMessage;
  return base;
}

export function createFixtureState(): MockState {
  const buckets = [
    bucket('assets', 0, [
      object(
        'brand/logo.svg',
        '<svg aria-label="Sqrzl"/>',
        'image/svg+xml',
        2,
        { kind: 'brand' }
      ),
      object('brand/icons/acorn.txt', 'acorn', 'text/plain', 3),
      object(
        'documents/readme.txt',
        'Welcome to the Sqrzl mock.\n',
        'text/plain',
        4,
        { owner: 'demo' }
      ),
      object('images/squirrel.jpg', 'jpeg fixture bytes', 'image/jpeg', 5),
    ]),
    bucket('customer-exports', 10, [
      object('august/report.csv', 'id,total\n1,42\n', 'text/csv', 11),
    ]),
    bucket('logs', 20, [
      object(
        '2026/08/17/app.json',
        '{"level":"info"}\n',
        'application/json',
        21
      ),
    ]),
    bucket('versioned-archive', 30, [
      object('records/one.json', '{"id":1}', 'application/json', 31),
    ]),
    bucket('empty-staging', 40, []),
  ];

  const mails = [
    mail(
      'demo@sqrzl.test',
      'smtp-1',
      50,
      'smtp',
      'delivered',
      'Welcome to Sqrzl',
      true
    ),
    mail(
      'demo@sqrzl.test',
      'sendgrid-2',
      51,
      'sendgrid',
      'accepted',
      'Your weekly digest'
    ),
    mail('demo@sqrzl.test', 'ses-3', 52, 'ses', 'bounced', 'Delivery notice'),
    mail(
      'alerts@sqrzl.test',
      'acs-4',
      53,
      'acs',
      'rejected',
      'Security alert',
      true
    ),
  ];

  const texts = [
    textMessage(
      '+15551230001',
      'txt-1',
      60,
      'twilio',
      'inbound',
      'delivered',
      'Can I get an update?',
      true
    ),
    textMessage(
      '+15551230001',
      'txt-2',
      61,
      'twilio',
      'outbound',
      'accepted',
      'Your order is ready.'
    ),
    textMessage(
      '+15551230002',
      'txt-3',
      62,
      'sns',
      'outbound',
      'failed',
      'Maintenance notice'
    ),
    textMessage(
      '+15551230003',
      'txt-4',
      63,
      'aws-sms-voice-v2',
      'outbound',
      'delivered',
      'Verification code: 4242'
    ),
    textMessage(
      '+15551230004',
      'txt-5',
      64,
      'acs',
      'inbound',
      'delivered',
      'External photo',
      true,
      true
    ),
  ];

  return {
    sessions: new Set(),
    buckets: new Map(buckets.map((item) => [item.info.name, item])),
    mail: new Map(
      mails.reduce<Array<[string, Map<string, MockMailMessage>]>>(
        (groups, item) => {
          let group = groups.find(([name]) => name === item.mailbox);
          if (!group) {
            group = [item.mailbox, new Map()];
            groups.push(group);
          }
          group[1].set(item.message_id, item);
          return groups;
        },
        []
      )
    ),
    texts: new Map(
      texts.reduce<Array<[string, MockTextMessage[]]>>((groups, item) => {
        let group = groups.find(([peer]) => peer === item.peer);
        if (!group) {
          group = [item.peer, []];
          groups.push(group);
        }
        group[1].push(item);
        return groups;
      }, [])
    ),
    destinations: new Map(),
    sequence: 100,
  };
}

export const fixtureBytes = bytes;
export const fixtureTime = time;
