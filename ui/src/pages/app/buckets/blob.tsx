import { resource } from '@askrjs/askr/resources';
import { currentRoute, Link } from '@askrjs/askr/router';
import {
  Button,
  EmptyState,
  Page,
  PageHeader,
  Spinner,
  Block,
} from '@askrjs/themes/components';
import BlobBreadcrumbs from '../../../components/storage/blob-breadcrumbs';
import BlobDetails from '../../../components/storage/blob-details';
import {
  findObjectByBlobId,
  loadObjectMetadata,
} from '../../../features/objects/objects.query';
import { blobFileName } from '../../../features/storage/path';
import { bucketPath } from '../../../shared/routes';

export default function Blob({
  bucketName,
  blobId,
}: {
  bucketName: string;
  blobId: string;
}) {
  const blobKeyFromQuery = currentRoute().query.get('key');

  const resolvedBlob = resource(
    async ({ signal }) => {
      if (blobKeyFromQuery) {
        await loadObjectMetadata({
          bucketName,
          objectKey: blobKeyFromQuery,
          signal,
        });
        return { key: blobKeyFromQuery };
      }

      return findObjectByBlobId({
        bucketName,
        blobId,
        signal,
      });
    },
    [bucketName, blobId, blobKeyFromQuery]
  );

  const resolvedBlobKey = resolvedBlob.value?.key;

  if (resolvedBlob.error && !resolvedBlob.value) {
    return (
      <EmptyState
        title="Blob details could not load"
        description="Retry the admin API call to see the blob details."
        actions={<Button onPress={() => resolvedBlob.refresh()}>Retry</Button>}
      />
    );
  }

  if (resolvedBlob.pending && !resolvedBlob.value) {
    return (
      <Block direction="row" justify="center" align="center">
        <Spinner />
      </Block>
    );
  }

  if (!resolvedBlobKey) {
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
    <Page>
      <BlobBreadcrumbs bucketName={bucketName} blobKey={resolvedBlobKey} />

      <PageHeader
        data-sqrzl-slot="storage-page-header"
        title={blobFileName(resolvedBlobKey)}
        description={resolvedBlobKey}
      />

      <BlobDetails bucketName={bucketName} blobKey={resolvedBlobKey} />
    </Page>
  );
}
