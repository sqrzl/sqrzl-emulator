import { Block } from '@askrjs/themes/components';

export default function DialogForm({
  children,
  onSubmit,
}: {
  children?: unknown;
  onSubmit: (event: Event) => void;
}) {
  return (
    <Block
      direction="column"
      asChild
      data-sqrzl-slot="storage-dialog-form"
      align="stretch"
      gap="md"
      width="full"
    >
      <form onSubmit={onSubmit}>{children}</form>
    </Block>
  );
}
