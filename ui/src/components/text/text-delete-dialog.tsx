import { Show } from '@askrjs/askr/control';
import { Button, FieldError, Block } from '@askrjs/themes/components';
import {
  AlertDialog,
  AlertDialogContent,
  AlertDialogOverlay,
  AlertDialogPortal,
} from '@askrjs/themes/components';
import type { DeleteTarget } from '../../features/storage/use-delete-target';
import StorageDialogFooter from '../storage/storage-dialog-footer';
import StorageDialogHeader from '../storage/storage-dialog-header';

export default function TextDeleteDialog({
  label,
  target,
  onCancel,
  onConfirm,
}: {
  label: string;
  target: DeleteTarget<{ id: string }> | null;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  return (
    <AlertDialog
      open={Boolean(target)}
      onOpenChange={(open) => {
        if (!open) onCancel();
      }}
    >
      <AlertDialogPortal>
        <AlertDialogOverlay />
        <AlertDialogContent>
          <Block direction="column" gap="md">
            <StorageDialogHeader title={`Delete ${label}`}>
              <p>
                {target
                  ? `Delete ${target.id}? This cannot be undone.`
                  : `Delete this ${label}.`}
              </p>
            </StorageDialogHeader>
            <Show when={target?.error}>
              <FieldError role="alert">{target?.error}</FieldError>
            </Show>
            <StorageDialogFooter>
              <Button
                variant="secondary"
                disabled={target?.deleting}
                onPress={onCancel}
              >
                Cancel
              </Button>
              <Button
                variant="destructive"
                disabled={target?.deleting}
                onPress={onConfirm}
              >
                {target?.deleting ? 'Deleting...' : `Delete ${label}`}
              </Button>
            </StorageDialogFooter>
          </Block>
        </AlertDialogContent>
      </AlertDialogPortal>
    </AlertDialog>
  );
}
