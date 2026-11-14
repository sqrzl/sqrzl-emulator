import { Link } from '@askrjs/askr/router';
import { Button, EmptyState, Page } from '@askrjs/themes/components';

export default function NotFoundPage() {
  return (
    <Page>
      <EmptyState
        title="Page not found"
        description="This page does not exist. Return home to continue."
        action={
          <Button asChild>
            <Link href="/">Return home</Link>
          </Button>
        }
      />
    </Page>
  );
}
