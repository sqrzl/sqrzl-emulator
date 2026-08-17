import { Button, FieldError, Block } from '@askrjs/themes/components';
import {
  AlertDialog,
  AlertDialogContent,
  AlertDialogOverlay,
  AlertDialogPortal,
} from '@askrjs/themes/components';
import { Show } from '@askrjs/askr/control';
import type { DeleteTarget } from '../../features/storage/use-delete-target';
import StorageDialogFooter from '../storage/storage-dialog-footer';
import StorageDialogHeader from '../storage/storage-dialog-header';

export type MessageDeleteTarget = DeleteTarget<{
  mailbox: string;
  messageId: string;
}>;

export default function MessageDeleteDialog({
  onCancel,
  onConfirm,
  target,
}: {
  onCancel: () => void;
  onConfirm: () => void;
  target: MessageDeleteTarget | null;
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
            <StorageDialogHeader title="Delete message">
              <p>
                {target
                  ? `Delete ${target.messageId} from ${target.mailbox}.`
                  : 'Delete this message.'}
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
                {target?.deleting ? 'Deleting...' : 'Delete message'}
              </Button>
            </StorageDialogFooter>
          </Block>
        </AlertDialogContent>
      </AlertDialogPortal>
    </AlertDialog>
  );
}
