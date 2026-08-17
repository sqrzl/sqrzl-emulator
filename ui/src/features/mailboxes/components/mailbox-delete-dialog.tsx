import { Button, FieldError, Block } from '@askrjs/themes/components';
import {
  AlertDialog,
  AlertDialogContent,
  AlertDialogOverlay,
  AlertDialogPortal,
} from '@askrjs/themes/components';
import { Show } from '@askrjs/askr/control';
import type { DeleteTarget } from '@/shared/delete-target';
import DialogFooter from '@/components/dialog/dialog-footer';
import DialogHeader from '@/components/dialog/dialog-header';

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
          <Block direction="column" gap="md">
            <DialogHeader title="Delete mailbox">
              <p>
                {target
                  ? `Delete ${target.mailbox} and all captured messages for it.`
                  : 'Delete this mailbox and all captured messages.'}
              </p>
            </DialogHeader>
            <Show when={target?.error}>
              <FieldError role="alert">{target?.error}</FieldError>
            </Show>
            <DialogFooter>
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
            </DialogFooter>
          </Block>
        </AlertDialogContent>
      </AlertDialogPortal>
    </AlertDialog>
  );
}
