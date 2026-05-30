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
  DialogDescription,
  DialogOverlay,
  DialogPortal,
  DialogTitle,
  Input,
  Label,
} from '@askrjs/ui';
import { putObjectContent as putBlobContent } from '../../features/objects/objects.query';
import { blobListKey } from '../../features/storage/keys';

export default function BlobModal({ bucketName }: { bucketName: string }) {
  const [isOpen, setOpen] = state(false);
  const [error, setError] = state('');

  const upload = createMutation({
    action: (
      input: { objectKey: string; content: File; contentType?: string },
      { signal }
    ) => putBlobContent({ bucketName, ...input, signal }),
    affects: () => [blobListKey(bucketName)],
    afterSuccess: 'invalidate',
  });

  async function submit(event: Event) {
    event.preventDefault();
    if (upload.pending) {
      return;
    }

    const form =
      event.target instanceof Element ? event.target.closest('form') : null;
    const keyInput = form?.querySelector('#blob-key');
    const fileInput = form?.querySelector('#blob-file');
    const selectedFile =
      fileInput instanceof HTMLInputElement
        ? (fileInput.files?.[0] ?? null)
        : null;

    if (!selectedFile) {
      setError('Choose a file to upload.');
      return;
    }

    const typedKey =
      keyInput instanceof HTMLInputElement ? keyInput.value.trim() : '';
    const objectKey = typedKey || selectedFile.name;

    setError('');

    try {
      await upload.execute({
        objectKey,
        content: selectedFile,
        contentType: selectedFile.type || undefined,
      });
      form?.reset();
      setOpen(false);
    } catch (caughtError) {
      setError(
        caughtError instanceof Error
          ? caughtError.message
          : 'Blob could not be uploaded.'
      );
    }
  }

  return (
    <>
      <Button onPress={() => setOpen(true)}>Add blob</Button>
      <Dialog open={isOpen()} onOpenChange={setOpen}>
        <DialogPortal>
          <DialogOverlay />
          <DialogContent>
            <Stack gap="4">
              <Stack gap="1">
                <DialogTitle>Add blob</DialogTitle>
                <DialogDescription>
                  Upload a file into {bucketName}.
                </DialogDescription>
              </Stack>
              <form onSubmit={(event: Event) => void submit(event)}>
                <Stack gap="4">
                  <Field>
                    <Label for="blob-key">Blob key</Label>
                    <Input
                      id="blob-key"
                      name="blob-key"
                      disabled={upload.pending}
                    />
                  </Field>
                  <Field>
                    <Label for="blob-file">File</Label>
                    <Input
                      id="blob-file"
                      name="blob-file"
                      type="file"
                      disabled={upload.pending}
                    />
                  </Field>
                  <Show when={error()}>
                    <FieldError role="alert">{error()}</FieldError>
                  </Show>
                  <ButtonGroup>
                    <Button type="submit" disabled={upload.pending}>
                      {upload.pending ? 'Uploading...' : 'Upload blob'}
                    </Button>
                    <DialogClose asChild onPress={() => setError('')}>
                      <Button variant="secondary" disabled={upload.pending}>
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
