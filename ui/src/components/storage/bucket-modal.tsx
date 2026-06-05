import { state } from '@askrjs/askr';
import { Show } from '@askrjs/askr/control';
import { createMutation } from '@askrjs/askr/data';
import {
  Button,
  ButtonGroup,
  Field,
  FieldError,
} from '@askrjs/themes/controls';
import { Stack } from '@askrjs/themes/layouts';
import {
  Dialog,
  DialogClose,
  DialogContent,
  DialogOverlay,
  DialogPortal,
  Input,
  Label,
} from '@askrjs/ui';
import { createBucket } from '../../features/buckets/buckets.query';
import { bucketListKey } from '../../features/storage/keys';

export default function BucketModal() {
  const [isOpen, setOpen] = state(false);
  const [error, setError] = state('');

  const create = createMutation({
    action: (name: string, { signal }) => createBucket({ name, signal }),
    affects: () => [bucketListKey],
    afterSuccess: 'invalidate',
  });

  async function submit(event: Event) {
    event.preventDefault();
    if (create.pending) {
      return;
    }

    const form =
      event.target instanceof Element ? event.target.closest('form') : null;
    const input = form?.querySelector('#bucket-name');
    const name = input instanceof HTMLInputElement ? input.value.trim() : '';

    if (!name) {
      setError('Bucket name is required.');
      return;
    }

    setError('');

    try {
      await create.execute(name);
      form?.reset();
      setOpen(false);
    } catch (caughtError) {
      setError(
        caughtError instanceof Error
          ? caughtError.message
          : 'Bucket could not be created.'
      );
    }
  }

  return (
    <>
      <Button onPress={() => setOpen(true)}>Add bucket</Button>
      <Dialog open={isOpen()} onOpenChange={setOpen}>
        <DialogPortal>
          <DialogOverlay />
          <DialogContent>
            <Stack gap="4">
              <Stack gap="1">
                <h2>Add bucket</h2>
                <p>Create a bucket in the emulator.</p>
              </Stack>
              <form onSubmit={(event: Event) => void submit(event)}>
                <Stack gap="4">
                  <Field>
                    <Label for="bucket-name">Bucket name</Label>
                    <Input
                      id="bucket-name"
                      name="bucket-name"
                      disabled={create.pending}
                    />
                  </Field>
                  <Show when={error()}>
                    <FieldError role="alert">{error()}</FieldError>
                  </Show>
                  <ButtonGroup>
                    <Button type="submit" disabled={create.pending}>
                      {create.pending ? 'Creating...' : 'Create bucket'}
                    </Button>
                    <DialogClose asChild onPress={() => setError('')}>
                      <Button variant="secondary" disabled={create.pending}>
                        Cancel
                      </Button>
                    </DialogClose>
                  </ButtonGroup>
                </Stack>
              </form>
            </Stack>
          </DialogContent>
        </DialogPortal>
      </Dialog>
    </>
  );
}
