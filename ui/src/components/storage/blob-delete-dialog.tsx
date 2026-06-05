import { Show } from '@askrjs/askr/control';
import { Button, ButtonGroup, FieldError } from '@askrjs/themes/controls';
import { Stack } from '@askrjs/themes/layouts';
import {
  AlertDialog,
  AlertDialogContent,
  AlertDialogOverlay,
  AlertDialogPortal,
} from '@askrjs/ui';
import type { DeleteTarget } from '../../features/storage/use-delete-target';

export type BlobDeleteTarget = DeleteTarget<{ blobKey: string }>;

export default function BlobDeleteDialog({
  bucketName,
  onCancel,
  onConfirm,
  target,
}: {
  bucketName: string;
  onCancel: () => void;
  onConfirm: () => void;
  target: BlobDeleteTarget | null;
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
              <h2>Delete blob</h2>
              <p>
                {target
                  ? `Delete ${target.blobKey} from ${bucketName}.`
                  : 'Delete this blob.'}
              </p>
            </Stack>
            <Show when={target?.error}>
              <FieldError role="alert">{target?.error}</FieldError>
            </Show>
            <ButtonGroup>
              <Button
                type="button"
                disabled={target?.deleting}
                onPress={onConfirm}
              >
                {target?.deleting ? 'Deleting...' : 'Delete blob'}
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
