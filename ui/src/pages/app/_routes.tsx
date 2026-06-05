import { route } from '@askrjs/askr/router';
import Buckets from './buckets';
import BucketPage from './bucket';
import BlobPage from './blob';
import { adminBucketsPath } from '../../shared/routes';

export function registerAppRoutes(): void {
  route(adminBucketsPath(), Buckets);
  route(`${adminBucketsPath()}/{bucketName}`, (params) => (
    <BucketPage bucketName={params.bucketName ?? ''} />
  ));
  route(`${adminBucketsPath()}/{bucketName}/blob/{blobId}`, (params) => (
    <BlobPage
      bucketName={params.bucketName ?? ''}
      blobId={params.blobId ?? ''}
    />
  ));
  route(`${adminBucketsPath()}/{bucketName}/*`, (params) => (
    <BucketPage
      bucketName={params.bucketName ?? ''}
      pathPrefix={params['*'] ?? ''}
    />
  ));
}
