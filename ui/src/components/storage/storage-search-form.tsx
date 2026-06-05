import { Button, ButtonGroup, Field } from '@askrjs/themes/controls';
import { Inline } from '@askrjs/themes/layouts';
import { Input, Label } from '@askrjs/ui';

const debounceMs = 250;

export default function StorageSearchForm({
  inputId,
  label,
  defaultValue,
  onSearch,
}: {
  inputId: string;
  label: string;
  defaultValue?: string;
  onSearch: (value: string) => void;
}) {
  let inputRef: HTMLInputElement | null = null;
  let timer: ReturnType<typeof setTimeout> | undefined;

  function inputElement(): HTMLInputElement | null {
    return inputRef;
  }

  function initializeInput(element: HTMLInputElement | null) {
    inputRef = element;
    if (!element) {
      return;
    }

    // Keep the field uncontrolled while still hydrating from URL-derived search state.
    if ((element.value ?? '').trim() === '' && defaultValue) {
      element.value = defaultValue;
    }
  }

  function handleInput(event: Event) {
    const value =
      event.target instanceof HTMLInputElement ? event.target.value.trim() : '';
    clearTimeout(timer);
    timer = setTimeout(() => {
      if (value !== (defaultValue?.trim() ?? '')) {
        onSearch(value);
      }
    }, debounceMs);
  }

  function searchNow(event?: Event) {
    event?.preventDefault();
    clearTimeout(timer);
    onSearch(inputElement()?.value.trim() ?? '');
  }

  function clearSearch() {
    clearTimeout(timer);
    const input = inputElement();
    if (input) {
      input.value = '';
      input.focus();
    }
    onSearch('');
  }

  return (
    <form onSubmit={searchNow}>
      <Inline align="end" gap="3" wrap="wrap">
        <Field>
          <Label for={inputId}>{label}</Label>
          <Input id={inputId} name={inputId} ref={initializeInput} onInput={handleInput} />
        </Field>
        <ButtonGroup>
          <Button type="submit">Search</Button>
          <Button type="button" variant="secondary" onPress={clearSearch}>
            Clear
          </Button>
        </ButtonGroup>
      </Inline>
    </form>
  );
}
