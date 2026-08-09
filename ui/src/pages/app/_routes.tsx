import { route } from '@askrjs/askr/router';
import Buckets from './buckets';
import BucketPage from './bucket';
import BlobPage from './blob';
import MailboxPage from './mailbox';
import MailMessagePage from './mail-message';
import Mailboxes from './mailboxes';
import TextConversationPage from './text-conversation';
import TextMessagePage from './text-message';
import TextsPage from './texts';
import {
  adminBucketsPath,
  adminMailboxesPath,
  adminTextsPath,
} from '../../shared/routes';
import { normalizeStoragePathPrefix } from '../../features/storage/path';

export function registerAppRoutes(): void {
  route(adminBucketsPath(), Buckets);
  route(`${adminBucketsPath()}/{bucketName}`, (params) => (
    <BucketPage bucketName={params.bucketName ?? ''} />
  ));
  route('/admin/blobs/{bucketName}/{blobId}', (params) => (
    <BlobPage
      bucketName={params.bucketName ?? ''}
      blobId={params.blobId ?? ''}
    />
  ));
  route(adminMailboxesPath(), Mailboxes);
  route(`${adminMailboxesPath()}/{mailboxName}`, (params) => (
    <MailboxPage mailboxName={params.mailboxName ?? ''} />
  ));
  route('/admin/mail/{mailboxName}/{messageId}', (params) => (
    <MailMessagePage
      mailboxName={params.mailboxName ?? ''}
      messageId={params.messageId ?? ''}
    />
  ));
  route(adminTextsPath(), TextsPage);
  route(`${adminTextsPath()}/{peer}`, (params) => (
    <TextConversationPage peer={params.peer ?? ''} />
  ));
  route('/admin/text/{peer}/{messageId}', (params) => (
    <TextMessagePage peer={params.peer ?? ''} messageId={params.messageId ?? ''} />
  ));
  route(`${adminBucketsPath()}/{bucketName}/*`, (params) => (
    <BucketPage
      bucketName={params.bucketName ?? ''}
      pathPrefix={normalizeStoragePathPrefix(params['*'] ?? '')}
    />
  ));
}
