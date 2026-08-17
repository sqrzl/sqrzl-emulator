import { route } from '@askrjs/askr/router';
import Buckets from './buckets';
import BucketPage from './buckets/bucket';
import BlobPage from './buckets/blob';
import Mailboxes from './mailboxes';
import MailboxPage from './mailboxes/mailbox';
import MailMessagePage from './mailboxes/message';
import TextsPage from './texts';
import TextConversationPage from './texts/conversation';
import TextMessagePage from './texts/message';
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
    <TextMessagePage
      peer={params.peer ?? ''}
      messageId={params.messageId ?? ''}
    />
  ));
  route(`${adminBucketsPath()}/{bucketName}/*`, (params) => (
    <BucketPage
      bucketName={params.bucketName ?? ''}
      pathPrefix={normalizeStoragePathPrefix(params['*'] ?? '')}
    />
  ));
}
