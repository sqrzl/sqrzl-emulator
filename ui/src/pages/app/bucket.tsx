import { Link } from '@askrjs/askr/router';
import { Button } from '@askrjs/themes/controls';
import { Inline, Stack } from '@askrjs/themes/layouts';
import BlobModal from '../../components/storage/blob-modal';
import BlobTable from '../../components/storage/blob-table';
import { adminBucketsPath, bucketFolderPath } from '../../shared/routes';

function normalizePathPrefix(routePath: string): string {
  const normalized = routePath
    .split('/')
    .filter(Boolean)
    .map((segment) => decodeURIComponent(segment))
    .join('/');
  if (!normalized) {
    return '';
  }

  return normalized.endsWith('/') ? normalized : `${normalized}/`;
}

function parentPrefix(pathPrefix: string): string {
  const trimmed = pathPrefix.replace(/\/$/, '');
  const lastSlash = trimmed.lastIndexOf('/');
  if (lastSlash < 0) {
    return '';
  }

  return `${trimmed.slice(0, lastSlash + 1)}`;
}

export default function Bucket({
  bucketName,
  pathPrefix = '',
}: {
  bucketName: string;
  pathPrefix?: string;
}) {
  const normalizedPrefix = normalizePathPrefix(pathPrefix);
  const locationLabel = normalizedPrefix
    ? `${bucketName}/${normalizedPrefix}`
    : bucketName;

  return (
    <Stack gap="4">
      <Inline justify="between" align="center" gap="3" wrap="wrap">
        <Stack gap="1">
          <h1>{locationLabel}</h1>
          <p>
            {normalizedPrefix
              ? 'First-level folders and blobs in this path.'
              : 'First-level folders and blobs in this bucket.'}
          </p>
        </Stack>
        <Inline gap="2" align="center" wrap="wrap">
          <Button variant="secondary" asChild>
            <Link href={adminBucketsPath()}>Back to buckets</Link>
          </Button>
          {normalizedPrefix ? (
            <Button variant="secondary" asChild>
              <Link
                href={bucketFolderPath(
                  bucketName,
                  parentPrefix(normalizedPrefix)
                )}
              >
                Up one level
              </Link>
            </Button>
          ) : null}
          <BlobModal bucketName={bucketName} />
        </Inline>
      </Inline>

      <BlobTable bucketName={bucketName} pathPrefix={normalizedPrefix} />
    </Stack>
  );
}
