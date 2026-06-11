const textEncoder = new TextEncoder();
const blobIdCache = new Map<string, string>();
const blobIdCacheLimit = 4096;

function setWithEviction<K, V>(cache: Map<K, V>, key: K, value: V, limit: number): void {
  if (!cache.has(key) && cache.size >= limit) {
    const oldestKey = cache.keys().next().value as K | undefined;
    if (oldestKey !== undefined) {
      cache.delete(oldestKey);
    }
  }

  cache.set(key, value);
}

function rotateLeft(value: number, bits: number): number {
  return ((value << bits) | (value >>> (32 - bits))) >>> 0;
}

function sha1(bytes: Uint8Array): Uint8Array {
  const messageLength = bytes.length;
  const totalLength = ((messageLength + 9 + 63) >> 6) << 6;
  const padded = new Uint8Array(totalLength);
  padded.set(bytes);
  padded[messageLength] = 0x80;

  const view = new DataView(padded.buffer);
  const bitLength = messageLength * 8;
  view.setUint32(totalLength - 8, Math.floor(bitLength / 2 ** 32), false);
  view.setUint32(totalLength - 4, bitLength >>> 0, false);

  let h0 = 0x67452301;
  let h1 = 0xefcdab89;
  let h2 = 0x98badcfe;
  let h3 = 0x10325476;
  let h4 = 0xc3d2e1f0;
  const words = new Uint32Array(80);

  for (let chunkStart = 0; chunkStart < totalLength; chunkStart += 64) {
    for (let index = 0; index < 16; index += 1) {
      words[index] = view.getUint32(chunkStart + index * 4, false);
    }

    for (let index = 16; index < 80; index += 1) {
      words[index] = rotateLeft(
        words[index - 3] ^
          words[index - 8] ^
          words[index - 14] ^
          words[index - 16],
        1
      );
    }

    let a = h0;
    let b = h1;
    let c = h2;
    let d = h3;
    let e = h4;

    for (let index = 0; index < 80; index += 1) {
      let f: number;
      let k: number;

      if (index < 20) {
        f = (b & c) | (~b & d);
        k = 0x5a827999;
      } else if (index < 40) {
        f = b ^ c ^ d;
        k = 0x6ed9eba1;
      } else if (index < 60) {
        f = (b & c) | (b & d) | (c & d);
        k = 0x8f1bbcdc;
      } else {
        f = b ^ c ^ d;
        k = 0xca62c1d6;
      }

      const temp = (rotateLeft(a, 5) + f + e + k + words[index]) >>> 0;
      e = d;
      d = c;
      c = rotateLeft(b, 30);
      b = a;
      a = temp;
    }

    h0 = (h0 + a) >>> 0;
    h1 = (h1 + b) >>> 0;
    h2 = (h2 + c) >>> 0;
    h3 = (h3 + d) >>> 0;
    h4 = (h4 + e) >>> 0;
  }

  const digest = new Uint8Array(20);
  const digestView = new DataView(digest.buffer);
  digestView.setUint32(0, h0, false);
  digestView.setUint32(4, h1, false);
  digestView.setUint32(8, h2, false);
  digestView.setUint32(12, h3, false);
  digestView.setUint32(16, h4, false);
  return digest;
}

function formatUuid(bytes: Uint8Array): string {
  const hex = Array.from(bytes, (byte) =>
    byte.toString(16).padStart(2, '0')
  ).join('');
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20, 32)}`;
}

export function loginPath(): string {
  return '/login';
}

export function logoutPath(): string {
  return '/logout';
}

export function adminBucketsPath(): string {
  return '/admin/buckets';
}

export function homePath(): string {
  return adminBucketsPath();
}

export function bucketPath(bucketName: string): string {
  return `${adminBucketsPath()}/${encodeURIComponent(bucketName)}`;
}

export function bucketFolderPath(
  bucketName: string,
  pathPrefix: string
): string {
  const normalized = pathPrefix.trim().replace(/^\/+|\/+$/g, '');
  if (!normalized) {
    return bucketPath(bucketName);
  }

  const encodedPath = normalized
    .split('/')
    .filter(Boolean)
    .map((segment) => encodeURIComponent(segment))
    .join('/');

  return `${bucketPath(bucketName)}/${encodedPath}`;
}

export function blobIdFromBlobKey(blobKey: string): string {
  const cached = blobIdCache.get(blobKey);
  if (cached) {
    return cached;
  }

  const digest = sha1(textEncoder.encode(blobKey));
  digest[6] = (digest[6] & 0x0f) | 0x50;
  digest[8] = (digest[8] & 0x3f) | 0x80;
  const blobId = formatUuid(digest.subarray(0, 16));
  setWithEviction(blobIdCache, blobKey, blobId, blobIdCacheLimit);
  return blobId;
}

export function blobPath(
  bucketName: string,
  blobKey: string,
  objectKey?: string
): string {
  const blobHref = `/admin/blobs/${encodeURIComponent(bucketName)}/${blobIdFromBlobKey(
    blobKey
  )}`;
  if (!objectKey) {
    return blobHref;
  }

  return `${blobHref}?key=${encodeURIComponent(objectKey)}`;
}
