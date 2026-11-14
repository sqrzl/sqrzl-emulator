import { spawn } from 'node:child_process';
import { mkdtemp, rm, symlink } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ui = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const authenticated = process.argv[2] === 'authenticated';
const root = await mkdtemp(resolve(tmpdir(), 'sqrzl-browser-'));
await symlink(resolve(ui, 'dist'), resolve(root, 'static'), 'dir');
const environment = Object.fromEntries(
  Object.entries(process.env).filter(
    ([name]) =>
      !/^(SQRZL_|AZURE_|GCS_|OCI_|TWILIO_|SENDGRID_|ACS_|SES_)/.test(name)
  )
);
Object.assign(environment, {
  SES_CONFIGURATION_SETS: 'browser-events',
  SQRZL_BLOBS_PATH: resolve(root, 'blobs'),
  SQRZL_API_PORT: authenticated ? '19002' : '19000',
  SQRZL_UI_PORT: authenticated ? '19003' : '19001',
  SQRZL_SMTP_PORT: authenticated ? '19253' : '19251',
});
if (authenticated)
  Object.assign(environment, {
    SQRZL_ACCESS_KEY_ID: 'browser-admin',
    SQRZL_SECRET_ACCESS_KEY: 'browser-test-secret',
  });
const child = spawn(resolve(ui, '../target/debug/sqrzl-emulator'), [], {
  cwd: root,
  env: environment,
  stdio: 'inherit',
});
for (const signal of ['SIGINT', 'SIGTERM'])
  process.on(signal, () => child.kill(signal));
child.on('error', (error) => {
  console.error(error);
  process.exitCode = 1;
});
child.on('close', async (code, signal) => {
  await rm(root, { recursive: true, force: true });
  process.exitCode = signal ? 0 : (code ?? 1);
});
