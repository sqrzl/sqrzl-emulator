import { Stack } from '@askrjs/themes/components';
import { DialogDescription, DialogTitle } from '@askrjs/ui';
import { Show } from '@askrjs/askr/control';

export default function StorageDialogHeader({
  children,
  title,
}: {
  children?: unknown;
  title: string;
}) {
  return (
    <Stack
      data-sqrzl-slot="storage-dialog-header"
      align="stretch"
      gap="1"
      width="full"
    >
      <DialogTitle asChild>
        <h2 data-sqrzl-slot="storage-dialog-title">{title}</h2>
      </DialogTitle>
      <Show when={children}>
        <DialogDescription asChild>
          <Stack data-sqrzl-slot="storage-dialog-description" gap="1">
            {children}
          </Stack>
        </DialogDescription>
      </Show>
    </Stack>
  );
}
