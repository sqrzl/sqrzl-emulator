import { state } from '@askrjs/askr';
import { Show } from '@askrjs/askr/control';
import { RadioIcon } from '@askrjs/lucide';
import { Button, Field, FieldError, Block } from '@askrjs/themes/components';
import {
  Dialog,
  DialogClose,
  DialogContent,
  DialogOverlay,
  DialogPortal,
  Input,
  Label,
  Select,
  SelectContent,
  SelectItem,
  SelectItemText,
  SelectPortal,
  SelectTrigger,
  SelectValue,
} from '@askrjs/themes/components';
import type { TextProvider } from '../../adapters/api.g';
import { saveTextDestination } from '../../features/texts/texts.query';
import StorageDialogFooter from '../storage/storage-dialog-footer';
import StorageDialogForm from '../storage/storage-dialog-form';
import StorageDialogHeader from '../storage/storage-dialog-header';

export default function TextDestinationDialog() {
  const [open, setOpen] = state(false);
  const [provider, setProvider] = state<TextProvider>('twilio');
  const [pending, setPending] = state(false);
  const [error, setError] = state('');
  let numberInput: HTMLInputElement | null = null;
  let callbackInput: HTMLInputElement | null = null;

  async function submit(event: Event): Promise<void> {
    event.preventDefault();
    const localNumber = numberInput?.value.trim() ?? '';
    const callbackUrl = callbackInput?.value.trim() ?? '';
    if (!localNumber || !callbackUrl) {
      setError('Local number and callback URL are required.');
      return;
    }
    setPending(true);
    setError('');
    try {
      await saveTextDestination({
        provider: provider(),
        localNumber,
        callbackUrl,
      });
      setOpen(false);
    } catch (caught) {
      setError(
        caught instanceof Error
          ? caught.message
          : 'Destination could not be saved.'
      );
    } finally {
      setPending(false);
    }
  }

  return (
    <>
      <Button variant="secondary" onPress={() => setOpen(true)}>
        <RadioIcon aria-hidden="true" /> Configure destination
      </Button>
      <Dialog open={open()} onOpenChange={setOpen}>
        <DialogPortal>
          <DialogOverlay />
          <DialogContent>
            <Block direction="column" gap="md">
              <StorageDialogHeader title="Configure text destination">
                <p>
                  Callbacks are restricted to allowlisted hosts and never follow
                  redirects.
                </p>
              </StorageDialogHeader>
              <StorageDialogForm onSubmit={(event) => void submit(event)}>
                <Field>
                  <Label for="destination-provider">Provider</Label>
                  <Select
                    id="destination-provider"
                    value={provider()}
                    onValueChange={(value) =>
                      setProvider(value as TextProvider)
                    }
                  >
                    <SelectTrigger aria-label="Destination provider">
                      <SelectValue />
                    </SelectTrigger>
                    <SelectPortal>
                      <SelectContent>
                        <SelectItem value="twilio">
                          <SelectItemText>Twilio</SelectItemText>
                        </SelectItem>
                        <SelectItem value="sns">
                          <SelectItemText>Amazon SNS</SelectItemText>
                        </SelectItem>
                        <SelectItem value="aws-sms-voice-v2">
                          <SelectItemText>AWS SMS Voice v2</SelectItemText>
                        </SelectItem>
                        <SelectItem value="acs">
                          <SelectItemText>
                            Azure Communication Services
                          </SelectItemText>
                        </SelectItem>
                      </SelectContent>
                    </SelectPortal>
                  </Select>
                </Field>
                <Field>
                  <Label for="destination-number">
                    Local number or identity
                  </Label>
                  <Input
                    id="destination-number"
                    type="tel"
                    required
                    placeholder="+15557654321"
                    ref={(node: Element | null) => {
                      numberInput = node as HTMLInputElement | null;
                    }}
                  />
                </Field>
                <Field>
                  <Label for="destination-url">Callback URL</Label>
                  <Input
                    id="destination-url"
                    type="url"
                    required
                    placeholder="http://127.0.0.1:8080/texts"
                    ref={(node: Element | null) => {
                      callbackInput = node as HTMLInputElement | null;
                    }}
                  />
                </Field>
                <Show when={error()}>
                  <FieldError role="alert">{error()}</FieldError>
                </Show>
                <StorageDialogFooter>
                  <DialogClose asChild>
                    <Button variant="secondary" disabled={pending()}>
                      Cancel
                    </Button>
                  </DialogClose>
                  <Button type="submit" disabled={pending()}>
                    {pending() ? 'Saving...' : 'Save destination'}
                  </Button>
                </StorageDialogFooter>
              </StorageDialogForm>
            </Block>
          </DialogContent>
        </DialogPortal>
      </Dialog>
    </>
  );
}
