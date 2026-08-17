import { ArrowLeftIcon, ArrowRightIcon } from '@askrjs/lucide';
import { Button, ButtonGroup, Block } from '@askrjs/themes/components';

export default function CursorPagination({
  hasNext,
  hasPrevious,
  onNext,
  onPrevious,
}: {
  hasNext: boolean;
  hasPrevious: boolean;
  onNext: () => void;
  onPrevious: () => void;
}) {
  return (
    <Block
      direction="row"
      justify="end"
      align="center"
      gap="xs"
      style={{ flexWrap: 'wrap' }}
    >
      <ButtonGroup>
        <Button
          variant="secondary"
          disabled={!hasPrevious}
          onPress={onPrevious}
        >
          <ArrowLeftIcon aria-hidden="true" />
          Previous
        </Button>
        <Button variant="secondary" disabled={!hasNext} onPress={onNext}>
          Next
          <ArrowRightIcon aria-hidden="true" />
        </Button>
      </ButtonGroup>
    </Block>
  );
}
