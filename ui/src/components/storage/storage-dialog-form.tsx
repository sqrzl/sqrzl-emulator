import { Stack } from '@askrjs/themes/components';

export default function StorageDialogForm({
  children,
  onSubmit,
}: {
  children?: unknown;
  onSubmit: (event: Event) => void;
}) {
  return (
    <Stack
      asChild
      data-sqrzl-slot="storage-dialog-form"
      align="stretch"
      gap="4"
      width="full"
    >
      <form onSubmit={onSubmit}>{children}</form>
    </Stack>
  );
}
