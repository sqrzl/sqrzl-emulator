import { describe, expect, it } from 'vite-plus/test';
import {
  adminBucketsPath,
  blobIdFromBlobKey,
  blobPath,
  bucketPath,
  loginPath,
  logoutPath,
} from '../src/shared/routes';

describe('shared route helpers', () => {
  it('builds deterministic uuid-style blob ids from blob keys', () => {
    const nestedBlobId = blobIdFromBlobKey('dir1/dir2/blobkey.png');

    expect(nestedBlobId).toMatch(
      /^[0-9a-f]{8}-[0-9a-f]{4}-5[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i
    );
    expect(nestedBlobId).toBe(blobIdFromBlobKey('dir1/dir2/blobkey.png'));
    expect(nestedBlobId).not.toBe(blobIdFromBlobKey('blobkey.png'));
    expect(blobPath('demo-bucket', 'dir1/dir2/blobkey.png')).toBe(
      `${bucketPath('demo-bucket')}/blob/${nestedBlobId}`
    );
    expect(blobPath('demo-bucket', 'dir1/dir2/blobkey.png')).not.toContain(
      '%2F'
    );
  });

  it('points the canonical ui routes at the admin surface', () => {
    expect(adminBucketsPath()).toBe('/admin/buckets');
    expect(loginPath()).toBe('/login');
    expect(logoutPath()).toBe('/logout');
  });
});
