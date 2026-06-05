# askr bug: controlled text inputs remount the DOM on every keystroke

## Summary

Binding a text input's value to reactive state — `<Input value={state()} onInput={...} />` —
causes askr to **replace the input (and its enclosing form subtree) with brand-new DOM nodes on
every keystroke**, instead of patching the `value` in place.

## Impact

- **In the browser:** the input node is recreated on each character, so focus is dropped after
  every keystroke — controlled text fields are effectively unusable for typing.
- **In tests (jsdom):** any element reference captured before an input event becomes stale. A
  reference to the old node reports `isConnected === false`, and events dispatched on it (e.g.
  `form.dispatchEvent(new Event("submit"))`) no longer reach askr's handlers.

## How it was observed

After opening a dialog with a controlled input, set the value and dispatch a single `input` event,
then re-query the DOM:

```ts
const formBefore = document.querySelector("form");
const inputBefore = document.querySelector("#bucket-name") as HTMLInputElement;

inputBefore.value = "alpha";
inputBefore.dispatchEvent(new Event("input", { bubbles: true }));
await flush();

const formAfter = document.querySelector("form");
const inputAfter = document.querySelector("#bucket-name");

// Observed results:
//   formBefore === formAfter        -> false  (form node was replaced)
//   inputBefore === inputAfter      -> false  (input node was replaced)
//   formBefore.isConnected          -> false  (old subtree detached)
//   (inputAfter as HTMLInputElement).value -> "alpha"  (new node has the value)
```

The same flow with the submit button works (clicking re-queries fresh nodes), but dispatching a
`submit` event on the **captured** (now-detached) form silently does nothing.

## Root cause

askr's re-execution reconciler remounts the controlled subtree when the bound state changes,
rather than diffing and updating the existing `value` attribute. Reading `value={state()}` inside
the component's render makes the whole returned subtree get recreated on each `setState`.

## Workaround

Use **uncontrolled** inputs and read values from the DOM on submit — the pattern the rest of the
codebase already uses (e.g. `src/pages/auth/login.tsx`):

```tsx
// No value= binding; read on submit instead.
<form onSubmit={(event) => void submit(event)}>
  <Input id="bucket-name" name="bucket-name" disabled={pending()} />
</form>

async function submit(event: Event) {
  event.preventDefault();
  const form =
    event.target instanceof Element ? event.target.closest("form") : null;
  const input = form?.querySelector("#bucket-name");
  const name = input instanceof HTMLInputElement ? input.value.trim() : "";
  // ...
}
```

Keep `state()` only for UI that is **not** bound to an input's `value` (e.g. `pending`, `error`,
`isOpen`), since toggling those does not recreate a focused input the user is typing into.

## Affected files (now using the uncontrolled pattern)

- `ui/src/components/storage/bucket-modal.tsx`
- `ui/src/components/storage/blob-modal.tsx`
- `ui/src/components/storage/storage-search-form.tsx`

---

# askr themes bug: ThemeToggle icon props desync after toggling

## Summary

Using `ThemeToggle` with `darkIcon` and `lightIcon` can render the wrong icon after one or more
toggles. The theme itself changes, but the displayed icon can become stale or inverted.

## Impact

- Header/theme controls provide incorrect visual state.
- Users cannot trust the icon to represent current theme mode.

## Environment

- `@askrjs/askr`: `0.0.40`
- `@askrjs/themes`: `0.0.6`
- `@askrjs/ui`: `0.0.7`
- App usage: `ui/src/pages/app/_layout.tsx`

## Reproduction

```tsx
<ThemeToggle
  aria-label="Toggle theme"
  darkIcon={<MoonIcon aria-hidden="true" />}
  lightIcon={<SunIcon aria-hidden="true" />}
/>
```

1. Load page with default light theme.
2. Click toggle to switch to dark.
3. Click toggle again to switch to light.
4. Repeat toggle a few times.

Observed:
- Theme state and `data-theme` change correctly.
- Icon can become inconsistent with current theme.

Expected:
- Displayed icon always matches current theme deterministically.

## Notes

- A render-callback child on `ThemeToggle` can keep icons in sync, but this is a workaround, not
  the intended API path.
