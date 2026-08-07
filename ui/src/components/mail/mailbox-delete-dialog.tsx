import { Button, FieldError, Stack } from '@askrjs/themes/components';
import {
  AlertDialog,
  AlertDialogContent,
  AlertDialogOverlay,
  AlertDialogPortal,
} from '@askrjs/ui';
import { Show } from '@askrjs/askr/control';
import type { DeleteTarget } from '../../features/storage/use-delete-target';
import StorageDialogFooter from '../storage/storage-dialog-footer';
import StorageDialogHeader from '../storage/storage-dialog-header';

export type MailboxDeleteTarget = DeleteTarget<{ mailbox: string }>;

export default function MailboxDeleteDialog({
  onCancel,
  onConfirm,
  target,
}: {
  onCancel: () => void;
  onConfirm: () => void;
  target: MailboxDeleteTarget | null;
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
            <StorageDialogHeader title="Delete mailbox">
              <p>
                {target
                  ? `Delete ${target.mailbox} and all captured messages for it.`
                  : 'Delete this mailbox and all captured messages.'}
              </p>
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
                disabled={target?.deleting}
                onPress={onConfirm}
              >
                {target?.deleting ? 'Deleting...' : 'Delete mailbox'}
              </Button>
            </StorageDialogFooter>
          </Stack>
        </AlertDialogContent>
      </AlertDialogPortal>
    </AlertDialog>
  );
}
