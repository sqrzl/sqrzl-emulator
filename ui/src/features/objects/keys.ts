export function blobListKey(bucketName: string): string {
  return `blobs:${bucketName}`;
}
