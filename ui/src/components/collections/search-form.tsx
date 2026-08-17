import { SearchIcon } from '@askrjs/lucide';
import { resource } from '@askrjs/askr/resources';
import { Button, ButtonGroup, Field, Block } from '@askrjs/themes/components';
import { Input, Label } from '@askrjs/themes/components';

export default function SearchForm({
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
  let searchInput: HTMLInputElement | null = null;

  resource(() => {
    const next = defaultValue ?? '';
    if (searchInput && searchInput.value !== next) {
      searchInput.value = next;
    }

    return null;
  }, [defaultValue]);

  function searchNow(event?: Event) {
    event?.preventDefault();
    onSearch(searchInput?.value.trim() ?? '');
  }

  function searchFromInput(event: Event) {
    const target = event.target;
    if (target instanceof HTMLInputElement) {
      onSearch(target.value);
    }
  }

  function clearSearch() {
    if (searchInput) {
      searchInput.value = '';
      searchInput.focus();
    }

    onSearch('');
  }

  return (
    <form data-sqrzl-slot="storage-search-form" onSubmit={searchNow}>
      <Block direction="row" align="end" gap="sm" style={{ flexWrap: 'wrap' }}>
        <Block
          data-sqrzl-slot="storage-search-field"
          grow
          minWidth={{ base: 'full', sm: 'sm' }}
          maxWidth={{ base: 'full', md: 'md' }}
        >
          <Field>
            <Label for={inputId}>{label}</Label>
            <Input
              id={inputId}
              name={inputId}
              ref={(node: HTMLInputElement | null) => {
                searchInput = node;
                if (node && node.value !== (defaultValue ?? '')) {
                  node.value = defaultValue ?? '';
                }
              }}
              onInput={searchFromInput}
            />
          </Field>
        </Block>
        <ButtonGroup attached={false}>
          <Button type="submit">
            <SearchIcon aria-hidden="true" /> Search
          </Button>
          <Button type="button" variant="secondary" onPress={clearSearch}>
            Clear
          </Button>
        </ButtonGroup>
      </Block>
    </form>
  );
}
