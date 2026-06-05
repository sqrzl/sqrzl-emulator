import { Button, ButtonGroup, FieldError } from '@askrjs/themes/controls';
import { Stack } from '@askrjs/themes/layouts';
import {
  AlertDialog,
  AlertDialogContent,
  AlertDialogOverlay,
  AlertDialogPortal,
} from '@askrjs/ui';
import { Show } from '@askrjs/askr/control';
import type { DeleteTarget } from '../../features/storage/use-delete-target';

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
          <Stack gap="4">
            <Stack gap="1">
              <h2>Delete bucket</h2>
              <p>
                {target?.pendingCount
                  ? 'Checking how many blobs are in this bucket.'
                  : target
                    ? `You are going to delete ${target.count ?? 0} blobs from ${target.bucketName}.`
                    : 'You are going to delete this bucket.'}
              </p>
              <p>This also removes the bucket itself.</p>
            </Stack>
            <Show when={target?.error}>
              <FieldError role="alert">{target?.error}</FieldError>
            </Show>
            <ButtonGroup>
              <Button
                type="button"
                disabled={target?.pendingCount || target?.deleting}
                onPress={onConfirm}
              >
                {target?.deleting
                  ? 'Deleting...'
                  : target
                    ? `Delete bucket and ${target.count ?? 0} blobs`
                    : 'Delete bucket'}
              </Button>
              <Button
                type="button"
                variant="secondary"
                disabled={target?.deleting}
                onPress={onCancel}
              >
                Cancel
              </Button>
            </ButtonGroup>
          </Stack>
        </AlertDialogContent>
      </AlertDialogPortal>
    </AlertDialog>
  );
}
