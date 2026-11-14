import { randomUUID } from 'node:crypto';
import { expect, test } from '@playwright/test';

test('open-mode storage journey survives dotted folder reloads', async ({
  page,
}) => {
  const bucket = `browser-${randomUUID()}`;
  await page.goto('/');
  await expect(
    page.getByRole('heading', { name: 'Buckets', exact: true })
  ).toBeVisible();
  await expect(page.locator('main')).toHaveCount(1);
  await page.getByRole('button', { name: 'Add bucket', exact: true }).click();
  await page.getByLabel('Bucket name', { exact: true }).fill(bucket);
  await page
    .getByRole('button', { name: 'Create bucket', exact: true })
    .click();
  await page.getByRole('link', { name: bucket, exact: true }).click();
  await page.getByRole('button', { name: 'Add blob', exact: true }).click();
  await page
    .getByLabel('Blob key', { exact: true })
    .fill('nested/folder.with.dot/sample.txt');
  await page
    .getByLabel('File', { exact: true })
    .setInputFiles({
      name: 'sample.txt',
      mimeType: 'text/plain',
      buffer: Buffer.from('browser regression'),
    });
  await page.getByRole('button', { name: 'Upload blob', exact: true }).click();
  await page.getByRole('link', { name: 'nested/', exact: true }).click();
  await page
    .getByRole('link', { name: 'folder.with.dot/', exact: true })
    .click();
  await expect(
    page.getByRole('link', { name: 'sample.txt', exact: true })
  ).toBeVisible();
  await page.reload();
  await expect(
    page.getByRole('link', { name: 'sample.txt', exact: true })
  ).toBeVisible();
  await page.goto(`/admin/buckets/${bucket}/nested%2Ffolder.with.dot/`);
  await expect(
    page.getByRole('link', { name: 'sample.txt', exact: true })
  ).toBeVisible();
  expect(
    await page.evaluate(() => document.documentElement.scrollWidth)
  ).toBeLessThanOrEqual(page.viewportSize()!.width);
  await page.goto('/admin/buckets');
  await page
    .getByRole('button', { name: `Delete bucket ${bucket}`, exact: true })
    .click();
  await expect(page.getByRole('alertdialog')).toBeVisible();
  await page
    .getByRole('button', { name: /^Delete bucket and 1 blobs$/ })
    .click();
  await expect(
    page.getByRole('link', { name: bucket, exact: true })
  ).toHaveCount(0);
});

test('mailbox email addresses survive direct navigation and refresh', async ({
  page,
  request,
}) => {
  const mailbox = `browser-${randomUUID()}@example.com`;
  const response = await request.post(
    'http://127.0.0.1:19000/v2/email/outbound-emails',
    {
      data: {
        FromEmailAddress: 'sender@example.com',
        Destination: { ToAddresses: [mailbox] },
        Content: {
          Simple: {
            Subject: { Data: 'Browser mailbox regression' },
            Body: { Text: { Data: 'Captured locally' } },
          },
        },
      },
    }
  );
  expect(response.ok()).toBeTruthy();
  await page.goto(`/admin/mailboxes/${encodeURIComponent(mailbox)}`);
  await expect(
    page.getByText('Browser mailbox regression', { exact: true })
  ).toBeVisible();
  await page.reload();
  await expect(
    page.getByText('Browser mailbox regression', { exact: true })
  ).toBeVisible();
});

test('protected UI signs in and signs out through the real server', async ({
  page,
}) => {
  await page.goto('http://127.0.0.1:19003/admin/buckets');
  await expect(
    page.getByRole('heading', { name: 'Sign in', exact: true })
  ).toBeVisible();
  await page.getByLabel('Username', { exact: true }).fill('browser-admin');
  await page
    .getByLabel('Password', { exact: true })
    .fill('browser-test-secret');
  await page.getByRole('button', { name: 'Sign in', exact: true }).click();
  await expect(
    page.getByRole('heading', { name: 'Buckets', exact: true })
  ).toBeVisible();
  await page.goto('http://127.0.0.1:19003/logout');
  await expect(
    page.getByRole('heading', { name: 'Sign in', exact: true })
  ).toBeVisible();
});

test('unknown routes offer a working return-home action', async ({ page }) => {
  await page.goto('/missing-route');
  await expect(
    page.getByRole('heading', { name: 'Page not found', exact: true })
  ).toBeVisible();
  await page.getByRole('link', { name: 'Return home', exact: true }).click();
  await expect(
    page.getByRole('heading', { name: 'Buckets', exact: true })
  ).toBeVisible();
});
