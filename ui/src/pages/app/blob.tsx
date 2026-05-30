import { resource } from '@askrjs/askr/resources';
import { Link } from '@askrjs/askr/router';
import { Button } from '@askrjs/themes/controls';
import { EmptyState } from '@askrjs/themes/feedback';
import { Stack } from '@askrjs/themes/layouts';
import BlobDetails from '../../components/storage/blob-details';
import { loadAllObjectPages as loadAllBlobPages } from '../../features/objects/objects.query';
import { bucketPath, blobIdFromBlobKey } from '../../shared/routes';

export default function Blob({
  bucketName,
  blobId,
}: {
  bucketName: string;
  blobId: string;
}) {
  const blobs = resource(
    ({ signal }) =>
      loadAllBlobPages({
        bucketName,
        signal,
      }),
    [bucketName]
  );

  const resolvedBlob = blobs.value?.find(
    (blob) => blobIdFromBlobKey(blob.key) === blobId
  );

  if (blobs.error && !blobs.value) {
    return (
      <EmptyState
        title="Blob details could not load"
        description="Retry the admin API call to see the blob details."
        actions={<Button onPress={() => blobs.refresh()}>Retry</Button>}
      />
    );
  }

  if (blobs.pending && !blobs.value) {
    return (
      <Stack gap="4">
        <p>Resolving blob...</p>
      </Stack>
    );
  }

  if (!resolvedBlob) {
    return (
      <EmptyState
        title="Blob not found"
        description="The blob id does not match any blob in this bucket."
        actions={
          <Button variant="secondary" asChild>
            <Link href={bucketPath(bucketName)}>Back to bucket</Link>
          </Button>
        }
      />
    );
  }

  return (
    <Stack gap="4">
      <Stack gap="1">
        <h1>Blob</h1>
        <p>{resolvedBlob.key}</p>
      </Stack>

      <BlobDetails bucketName={bucketName} blobKey={resolvedBlob.key} />
    </Stack>
  );
}
