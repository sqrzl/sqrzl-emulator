import type {
  Acl,
  BucketInfo,
  BucketPolicyDocument,
  LifecycleConfiguration,
  MessageDetail,
  MultipartUpload,
  ObjectMetadata,
  ObjectVersionInfo,
  TextDestination,
  TextMessageDetail,
} from '../../src/adapters/api.g';

export type MockRequest = {
  method: string;
  url: string;
  headers?: Record<string, string | undefined>;
  cookies?: Record<string, string>;
  json?: unknown;
  body?: Uint8Array;
};

export type MockResponse = {
  status: number;
  headers?: Record<string, string>;
  body?: unknown | Uint8Array;
};

export type MockObject = ObjectMetadata & {
  bytes: Uint8Array;
  tags: Record<string, string>;
  acl: Acl;
  versions: ObjectVersionInfo[];
};

export type MockBucket = {
  info: BucketInfo;
  objects: Map<string, MockObject>;
  acl: Acl;
  policy?: BucketPolicyDocument;
  lifecycle?: LifecycleConfiguration;
  uploads: MultipartUpload[];
};

export type MockMailMessage = MessageDetail & {
  raw: Uint8Array;
  attachmentBytes: Map<string, Uint8Array>;
};

export type MockTextMessage = TextMessageDetail & {
  mediaBytes: Map<string, Uint8Array>;
};

export type MockState = {
  sessions: Set<string>;
  buckets: Map<string, MockBucket>;
  mail: Map<string, Map<string, MockMailMessage>>;
  texts: Map<string, MockTextMessage[]>;
  destinations: Map<string, TextDestination>;
  sequence: number;
};
