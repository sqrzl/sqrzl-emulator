import { Block } from '@askrjs/themes/components';

export default function DialogFooter({ children }: { children?: unknown }) {
  return (
    <Block
      direction="row"
      data-sqrzl-slot="storage-dialog-footer"
      justify="end"
      align="center"
      gap="xs"
      style={{ flexWrap: 'wrap' }}
      width="full"
    >
      {children}
    </Block>
  );
}
