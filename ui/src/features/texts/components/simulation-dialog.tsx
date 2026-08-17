import { state } from '@askrjs/askr';
import { Show } from '@askrjs/askr/control';
import { createMutation } from '@askrjs/askr/data';
import { navigate } from '@askrjs/askr/router';
import { MessageSquarePlusIcon } from '@askrjs/lucide';
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
  Textarea,
} from '@askrjs/themes/components';
import type {
  InboundTextSimulationRequest,
  TextProvider,
} from '@/adapters/api.g';
import { simulateInboundText } from '../texts.query';
import { textConversationListKey } from '@/features/text-conversations';
import { textMessagePath } from '@/shared/routes';
import DialogFooter from '@/components/dialog/dialog-footer';
import DialogForm from '@/components/dialog/dialog-form';
import DialogHeader from '@/components/dialog/dialog-header';

const providers: Array<{ value: TextProvider; label: string }> = [
  { value: 'twilio', label: 'Twilio' },
  { value: 'sns', label: 'Amazon SNS' },
  { value: 'aws-sms-voice-v2', label: 'AWS SMS Voice v2' },
  { value: 'acs', label: 'Azure Communication Services' },
];

export default function SimulationDialog() {
  const [open, setOpen] = state(false);
  const [provider, setProvider] = state<TextProvider>('twilio');
  const [error, setError] = state('');
  let fromInput: HTMLInputElement | null = null;
  let toInput: HTMLInputElement | null = null;
  let bodyInput: HTMLTextAreaElement | null = null;
  let mediaInput: HTMLInputElement | null = null;

  const create = createMutation({
    action: (payload: InboundTextSimulationRequest, { signal }) =>
      simulateInboundText(payload, signal),
    affects: () => [textConversationListKey],
    afterSuccess: 'invalidate',
  });

  async function submit(event: Event): Promise<void> {
    event.preventDefault();
    const from = fromInput?.value.trim() ?? '';
    const to = toInput?.value.trim() ?? '';
    const body = bodyInput?.value ?? '';
    if (!from || !to) {
      setError('From and to phone numbers are required.');
      return;
    }

    const media = [] as NonNullable<InboundTextSimulationRequest['media']>;
    const file = mediaInput?.files?.[0];
    if (file) {
      if (provider() !== 'twilio') {
        setError('Inbound MMS media is supported only for Twilio.');
        return;
      }
      const bytes = new Uint8Array(await file.arrayBuffer());
      let binary = '';
      for (const byte of bytes) binary += String.fromCharCode(byte);
      media.push({
        filename: file.name,
        content_type: file.type || 'application/octet-stream',
        content_base64: window.btoa(binary),
      });
    }

    setError('');
    try {
      const message = await create.execute({
        provider: provider(),
        from,
        to,
        body,
        media: media.length ? media : undefined,
      });
      setOpen(false);
      navigate(textMessagePath(message.peer, message.message_id));
    } catch (caught) {
      setError(
        caught instanceof Error
          ? caught.message
          : 'Inbound text could not be simulated.'
      );
    }
  }

  return (
    <>
      <Button onPress={() => setOpen(true)}>
        <MessageSquarePlusIcon aria-hidden="true" /> Simulate inbound
      </Button>
      <Dialog open={open()} onOpenChange={setOpen}>
        <DialogPortal>
          <DialogOverlay />
          <DialogContent>
            <Block direction="column" gap="md">
              <DialogHeader title="Simulate inbound text">
                <p>
                  Store an inbound provider event and attempt its configured
                  destination.
                </p>
              </DialogHeader>
              <DialogForm onSubmit={(event) => void submit(event)}>
                <Field>
                  <Label for="text-provider">Provider</Label>
                  <Select
                    id="text-provider"
                    value={provider()}
                    onValueChange={(value) =>
                      setProvider(value as TextProvider)
                    }
                  >
                    <SelectTrigger aria-label="Text provider">
                      <SelectValue />
                    </SelectTrigger>
                    <SelectPortal>
                      <SelectContent>
                        {providers.map((item) => (
                          <SelectItem value={item.value} key={item.value}>
                            <SelectItemText>{item.label}</SelectItemText>
                          </SelectItem>
                        ))}
                      </SelectContent>
                    </SelectPortal>
                  </Select>
                </Field>
                <Field>
                  <Label for="text-from">From</Label>
                  <Input
                    id="text-from"
                    type="tel"
                    required
                    placeholder="+15551234567"
                    ref={(node: Element | null) => {
                      fromInput = node as HTMLInputElement | null;
                    }}
                  />
                </Field>
                <Field>
                  <Label for="text-to">To (local number)</Label>
                  <Input
                    id="text-to"
                    type="tel"
                    required
                    placeholder="+15557654321"
                    ref={(node: Element | null) => {
                      toInput = node as HTMLInputElement | null;
                    }}
                  />
                </Field>
                <Field>
                  <Label for="text-body">Body</Label>
                  <Textarea
                    id="text-body"
                    placeholder="Message body"
                    ref={(node: Element | null) => {
                      bodyInput = node as HTMLTextAreaElement | null;
                    }}
                  />
                </Field>
                <Field>
                  <Label for="text-media">Twilio MMS media (optional)</Label>
                  <Input
                    id="text-media"
                    type="file"
                    disabled={provider() !== 'twilio'}
                    ref={(node: Element | null) => {
                      mediaInput = node as HTMLInputElement | null;
                    }}
                  />
                </Field>
                <Show when={error()}>
                  <FieldError role="alert">{error()}</FieldError>
                </Show>
                <DialogFooter>
                  <DialogClose asChild>
                    <Button variant="secondary" disabled={create.pending}>
                      Cancel
                    </Button>
                  </DialogClose>
                  <Button type="submit" disabled={create.pending}>
                    {create.pending ? 'Simulating...' : 'Simulate inbound'}
                  </Button>
                </DialogFooter>
              </DialogForm>
            </Block>
          </DialogContent>
        </DialogPortal>
      </Dialog>
    </>
  );
}
