import { Show } from '@askrjs/askr/control';
import { Button, FieldError, Block } from '@askrjs/themes/components';
import {
  AlertDialog,
  AlertDialogContent,
  AlertDialogOverlay,
  AlertDialogPortal,
} from '@askrjs/themes/components';
import type { DeleteTarget } from '@/shared/delete-target';
import DialogFooter from '@/components/dialog/dialog-footer';
import DialogHeader from '@/components/dialog/dialog-header';

export default function ConversationDeleteDialog({
  target,
  onCancel,
  onConfirm,
}: {
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
            <DialogHeader title="Delete conversation">
              <p>
                {target
                  ? `Delete ${target.id}? This cannot be undone.`
                  : 'Delete this conversation.'}
              </p>
            </DialogHeader>
            <Show when={target?.error}>
              <FieldError role="alert">{target?.error}</FieldError>
            </Show>
            <DialogFooter>
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
                {target?.deleting ? 'Deleting...' : 'Delete conversation'}
              </Button>
            </DialogFooter>
          </Block>
        </AlertDialogContent>
      </AlertDialogPortal>
    </AlertDialog>
  );
}
