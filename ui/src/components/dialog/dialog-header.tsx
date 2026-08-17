import { Block } from '@askrjs/themes/components';
import { DialogDescription, DialogTitle } from '@askrjs/themes/components';
import { Show } from '@askrjs/askr/control';

export default function DialogHeader({
  children,
  title,
}: {
  children?: unknown;
  title: string;
}) {
  return (
    <Block
      direction="column"
      data-sqrzl-slot="storage-dialog-header"
      align="stretch"
      gap="xs"
      width="full"
    >
      <>
        <DialogTitle asChild>
          <h2 data-sqrzl-slot="storage-dialog-title">{title}</h2>
        </DialogTitle>
        <Show when={children}>
          <DialogDescription asChild>
            <Block
              direction="column"
              data-sqrzl-slot="storage-dialog-description"
              gap="xs"
            >
              {children}
            </Block>
          </DialogDescription>
        </Show>
      </>
    </Block>
  );
}
