import { state } from '@askrjs/askr';
import { For, Show } from '@askrjs/askr/control';
import { resource } from '@askrjs/askr/resources';
import {
  Block,
  Button,
  Field,
  FieldError,
  Input,
  Label,
  Select,
  SelectTrigger,
  SelectValue,
  SelectPortal,
  SelectContent,
  SelectItem,
  SelectItemText,
  Toolbar,
  AlertDialog,
  AlertDialogPortal,
  AlertDialogOverlay,
  AlertDialogContent,
} from '@askrjs/themes/components';
import type {
  SesEventRecord,
  SesEventType,
  SimulateSesEventRequest,
} from '@/adapters/api.g';
import DialogHeader from '@/components/dialog/dialog-header';
import DialogFooter from '@/components/dialog/dialog-footer';
import {
  getSesDestination,
  saveSesDestination,
  deleteSesDestination,
  listSesEvents,
  simulateSesEvent,
} from '../ses-events.query';

const eventTypes: SesEventType[] = [
  'Send',
  'Delivery',
  'Bounce',
  'Complaint',
  'Reject',
  'DeliveryDelay',
  'Rendering Failure',
];

export default function SesEventsPanel({
  messageId,
  configurationSet,
}: {
  messageId: string;
  configurationSet?: string | null;
}) {
  const [cursor, setCursor] = state<string | undefined>(undefined);
  const [previous, setPrevious] = state<Array<string | undefined>>([]);
  const events = resource(
    ({ signal }) => listSesEvents(messageId, signal, cursor()),
    [messageId, cursor()]
  );
  function refreshEvents() {
    setPrevious([]);
    if (cursor() === undefined) events.refresh();
    else setCursor(undefined);
  }
  const destination = resource(
    ({ signal }) =>
      configurationSet
        ? getSesDestination(configurationSet, signal)
        : Promise.resolve(null),
    [configurationSet]
  );
  const [callbackUrl, setCallbackUrl] = state<string | null>(null);
  const [eventType, setEventType] = state<SesEventType>('Delivery');
  const [pending, setPending] = state(false);
  const [error, setError] = state('');
  const [notice, setNotice] = state('');
  const [removeOpen, setRemoveOpen] = state(false);
  // Keep this exact payload through an ambiguous response. Only a successful
  // acknowledgement lets the next explicit simulation create a new identity.
  const [retryRequest, setRetryRequest] = state<SimulateSesEventRequest | null>(
    null
  );

  async function run(action: () => Promise<void>) {
    if (pending()) return;
    setPending(true);
    setError('');
    setNotice('');
    try {
      await action();
    } catch (caught) {
      setError(
        caught instanceof Error
          ? caught.message
          : 'The operation could not complete.'
      );
    } finally {
      setPending(false);
    }
  }
  async function simulate() {
    const body = retryRequest() ?? {
      message_id: messageId,
      event_type: eventType(),
      request_id: crypto.randomUUID(),
    };
    setRetryRequest(body);
    await run(async () => {
      const result = await simulateSesEvent(body);
      setRetryRequest(null);
      setNotice(
        result.state === 'pending'
          ? 'Delivery status is uncertain. This event will not be resent automatically.'
          : 'Event recorded.'
      );
      refreshEvents();
    });
  }
  return (
    <section aria-label="SES event publishing">
      <Block direction="column" gap="md">
        <Toolbar title="SES event publishing" />
        <p>
          Simulate provider events for this captured message. Callback delivery
          is restricted to allowlisted hosts.
        </p>
        <Show
          when={Boolean(configurationSet)}
          fallback={
            <p>
              This message has no configuration set. Events are recorded locally
              without a destination.
            </p>
          }
        >
          <Block direction="column" gap="sm">
            <p>Configuration set: {configurationSet}</p>
            <Show when={destination.error}>
              <FieldError role="alert">
                Destination could not load.{' '}
                <Button onPress={() => destination.refresh()}>
                  Retry destination
                </Button>
              </FieldError>
            </Show>
            <Show when={destination.pending}>
              <p role="status">Loading destination…</p>
            </Show>
            <Show when={!destination.pending && !destination.error}>
              <p>
                {destination.value
                  ? `Current destination: ${destination.value.callback_url}`
                  : 'No callback destination configured.'}
              </p>
              <Show when={destination.value}>
                <p>
                  Subscribed events: {destination.value?.event_types.join(', ')}
                </p>
              </Show>
              <Field>
                <Label for={`ses-url-${messageId}`}>SES callback URL</Label>
                <Input
                  id={`ses-url-${messageId}`}
                  type="url"
                  value={callbackUrl() ?? destination.value?.callback_url ?? ''}
                  onInput={(event) =>
                    setCallbackUrl((event.target as HTMLInputElement).value)
                  }
                />
              </Field>
              <p>
                Saving subscribes this destination to all seven supported event
                types.
              </p>
              <Block direction="row" gap="sm">
                <Button
                  disabled={
                    pending() ||
                    !(callbackUrl() ?? destination.value?.callback_url)?.trim()
                  }
                  onPress={() =>
                    void run(async () => {
                      await saveSesDestination(configurationSet!, {
                        callback_url: (
                          callbackUrl() ??
                          destination.value?.callback_url ??
                          ''
                        ).trim(),
                        event_types: eventTypes,
                      });
                      setCallbackUrl(null);
                      destination.refresh();
                      setNotice('Destination saved.');
                    })
                  }
                >
                  Save destination
                </Button>
                <Show when={destination.value}>
                  <Button
                    variant="destructive"
                    disabled={pending()}
                    onPress={() => setRemoveOpen(true)}
                  >
                    Remove destination
                  </Button>
                </Show>
              </Block>
            </Show>
          </Block>
        </Show>
        <Field>
          <Label for={`ses-event-${messageId}`}>Event type</Label>
          <Select
            id={`ses-event-${messageId}`}
            value={eventType()}
            disabled={pending() || Boolean(retryRequest())}
            onValueChange={(value) => setEventType(value as SesEventType)}
          >
            <SelectTrigger aria-label="SES event type">
              <SelectValue />
            </SelectTrigger>
            <SelectPortal>
              <SelectContent>
                <For each={eventTypes} by={(type) => type}>
                  {(type) => (
                    <SelectItem value={type}>
                      <SelectItemText>{type}</SelectItemText>
                    </SelectItem>
                  )}
                </For>
              </SelectContent>
            </SelectPortal>
          </Select>
        </Field>
        <Button disabled={pending()} onPress={() => void simulate()}>
          {pending()
            ? 'Working…'
            : retryRequest()
              ? 'Retry same event'
              : 'Simulate event'}
        </Button>
        <Show when={retryRequest() && !pending()}>
          <p>
            The previous response was not confirmed. Retry uses the same event
            identity; an uncertain callback is never resent automatically.
          </p>
        </Show>
        <Show when={error()}>
          <FieldError role="alert">{error()}</FieldError>
        </Show>
        <Show when={notice()}>
          <p role="status">{notice()}</p>
        </Show>
        <Block direction="row" gap="sm">
          <Toolbar title="Event history" />
          <Button
            variant="secondary"
            disabled={events.pending}
            onPress={refreshEvents}
          >
            Refresh events
          </Button>
        </Block>
        <Show when={events.error}>
          <FieldError role="alert">
            Event history could not load. Use Refresh events to retry.
          </FieldError>
        </Show>
        <Show when={events.pending}>
          <p role="status">Loading events…</p>
        </Show>
        <Show when={events.value && events.value.items.length === 0}>
          <p>No simulated events for this message.</p>
        </Show>
        <For each={events.value?.items ?? []} by={(item) => item.event_id}>
          {(item: SesEventRecord) => (
            <article aria-label={`SES ${item.event_type} ${item.event_id}`}>
              <Block direction="column" gap="xs">
                <p>
                  {item.event_type} · {item.state ?? 'completed'} ·{' '}
                  {item.created_at}
                </p>
                <p>Event ID: {item.event_id}</p>
                <Show
                  when={
                    item.state === 'pending' ||
                    item.callback?.state === 'pending'
                  }
                >
                  <p role="status">
                    Delivery status is uncertain. Inspect the receiver before
                    creating another event. This callback will not be resent
                    automatically.
                  </p>
                </Show>
                <Show when={item.callback}>
                  {(callback) => (
                    <>
                      <p>
                        Callback: {callback.state} · {callback.url} · HTTP{' '}
                        {callback.response_status ?? 'unconfirmed'}
                      </p>
                      <Show when={callback.error}>
                        <p>{callback.error}</p>
                      </Show>
                      <details>
                        <summary>Callback request and response</summary>
                        <pre>
                          {JSON.stringify(
                            {
                              headers: callback.request_headers,
                              body: callback.request_body,
                              response: callback.response_body,
                            },
                            null,
                            2
                          )}
                        </pre>
                      </details>
                    </>
                  )}
                </Show>
                <details>
                  <summary>Event payload</summary>
                  <pre>{JSON.stringify(item.record, null, 2)}</pre>
                </details>
              </Block>
            </article>
          )}
        </For>
        <Block direction="row" gap="sm">
          <Button
            variant="secondary"
            disabled={events.pending || previous().length === 0}
            onPress={() => {
              const stack = [...previous()];
              const marker = stack.pop();
              setPrevious(stack);
              setCursor(marker);
            }}
          >
            Previous events
          </Button>
          <Button
            variant="secondary"
            disabled={events.pending || !events.value?.next}
            onPress={() => {
              if (!events.value?.next) return;
              setPrevious([...previous(), cursor()]);
              setCursor(events.value.next);
            }}
          >
            Next events
          </Button>
        </Block>
        <AlertDialog open={removeOpen()} onOpenChange={setRemoveOpen}>
          <AlertDialogPortal>
            <AlertDialogOverlay />
            <AlertDialogContent>
              <DialogHeader title="Remove SES destination">
                <p>
                  Remove the callback destination for {configurationSet}?
                  Existing event history is retained.
                </p>
              </DialogHeader>
              <DialogFooter>
                <Button
                  variant="secondary"
                  disabled={pending()}
                  onPress={() => setRemoveOpen(false)}
                >
                  Cancel
                </Button>
                <Button
                  variant="destructive"
                  disabled={pending()}
                  onPress={() =>
                    void run(async () => {
                      await deleteSesDestination(configurationSet!);
                      setRemoveOpen(false);
                      destination.refresh();
                      setNotice('Destination removed.');
                    })
                  }
                >
                  Confirm removal
                </Button>
              </DialogFooter>
              <Show when={error()}>
                <FieldError role="alert">{error()}</FieldError>
              </Show>
            </AlertDialogContent>
          </AlertDialogPortal>
        </AlertDialog>
      </Block>
    </section>
  );
}
