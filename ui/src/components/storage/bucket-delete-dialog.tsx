import { Button, FieldError, Block } from '@askrjs/themes/components';
import {
  AlertDialog,
  AlertDialogContent,
  AlertDialogOverlay,
  AlertDialogPortal,
} from '@askrjs/themes/components';
import { Show } from '@askrjs/askr/control';
import type { DeleteTarget } from '../../features/storage/use-delete-target';
import StorageDialogFooter from './storage-dialog-footer';
import StorageDialogHeader from './storage-dialog-header';

export type BucketDeleteTarget = DeleteTarget<{ bucketName: string }>;

export default function BucketDeleteDialog({
  onCancel,
  onConfirm,
  target,
}: {
  onCancel: () => void;
  onConfirm: () => void;
  target: BucketDeleteTarget | null;
}) {
  return (
    <AlertDialog
      open={Boolean(target)}
      onOpenChange={(open) => {
        if (!open) {
          onCancel();
        }
      }}
    >
      <AlertDialogPortal>
        <AlertDialogOverlay />
        <AlertDialogContent>
          <Block direction="column" gap="md">
            <StorageDialogHeader title="Delete bucket">
              <p>
                {target?.pendingCount
                  ? 'Checking how many blobs are in this bucket.'
                  : target
                    ? `You are going to delete ${target.count ?? 0} blobs from ${target.bucketName}.`
                    : 'You are going to delete this bucket.'}
              </p>
              <p>This also removes the bucket itself.</p>
            </StorageDialogHeader>
            <Show when={target?.error}>
              <FieldError role="alert">{target?.error}</FieldError>
            </Show>
            <StorageDialogFooter>
              <Button
                type="button"
                variant="secondary"
                disabled={target?.deleting}
                onPress={onCancel}
              >
                Cancel
              </Button>
              <Button
                type="button"
                variant="destructive"
                disabled={target?.pendingCount || target?.deleting}
                onPress={onConfirm}
              >
                {target?.deleting
                  ? 'Deleting...'
                  : target
                    ? `Delete bucket and ${target.count ?? 0} blobs`
                    : 'Delete bucket'}
              </Button>
            </StorageDialogFooter>
          </Block>
        </AlertDialogContent>
      </AlertDialogPortal>
    </AlertDialog>
  );
}
