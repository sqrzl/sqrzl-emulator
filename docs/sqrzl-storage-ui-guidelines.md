# Sqrzl Storage UI Guidelines

These are the product-level presentation rules for the Sqrzl admin UI. Use
[`ui/AGENTS.md`](../ui/AGENTS.md) for ownership and workflow; use this file for
visual, layout, and interaction rules.

## Principles

- Keep storage screens operational and quiet: fast scanning, direct actions,
  minimal chrome.
- Use Askr primitives, themes, and tokens as the base. Keep Sqrzl CSS to small
  storage-specific polish through `data-sqrzl-slot`.
- Do not clone Askr behavior or override Askr-owned `data-slot` styling to hide
  framework bugs. If a bug is real, document it in [`askr-bug.md`](../askr-bug.md).

## Dialogs

- Dialogs are compact task surfaces, not centered hero panels.
- Titles and descriptions are left-aligned, full-width, and modestly sized.
- Dialog structure is always title, short description, body/form, error, footer.
- Footers are right-aligned with `Flex`, ordered `Cancel` then confirm.
- Destructive confirms use `variant="destructive"` and stay last.
- Errors render directly above the footer with `FieldError`.
- Prefer uncontrolled inputs and read values on submit.

## Forms

- Storage create/upload forms use uncontrolled inputs and read values on submit.
- Fields stretch to the dialog body width.
- Labels are concise. Helper text belongs in the dialog description.
- Blob upload copy explains the implied path without repeating bucket/path noise.

## Tables And Routes

- Bucket and blob tables optimize for scanning: name first, type/status next,
  actions last.
- Folder rows appear before blob rows.
- Folder navigation uses Askr routes. Blob detail routes stay stable at
  `/admin/buckets/{bucketName}/blob/{blobId}`.

## Review Checklist

- No centered modal headings.
- No `ButtonGroup` in storage dialog footers.
- No local Askr primitive clones.
- No Sqrzl CSS targeting Askr `data-slot` except narrow descendants inside
  Sqrzl-owned slots.
- Desktop and mobile browser QA covers at least one create dialog, one
  destructive dialog, and one nested bucket path.
