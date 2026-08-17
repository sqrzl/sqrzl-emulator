import { state } from '@askrjs/askr';
import { task } from '@askrjs/askr/resources';
import { navigate } from '@askrjs/askr/router';
import {
  Button,
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
  Container,
  Block,
  Section,
  Spinner,
  Text,
} from '@askrjs/themes/components';
import {
  isDevAuthBypassed,
  logoutAdminSession,
} from '../../features/auth/admin-session';
import { adminBucketsPath } from '../../shared/routes';
import { isUnauthorized } from '../../adapters/response';

export default function LogoutPage() {
  if (isDevAuthBypassed()) {
    return (
      <Section size="4">
        <Container size="sm">
          <Card variant="raised">
            <CardHeader>
              <CardTitle>Local development mode</CardTitle>
            </CardHeader>
            <CardContent>
              <Block direction="column" gap="md">
                <Text>
                  Sign out is disabled while admin auth bypass is active.
                </Text>
                <Button asChild>
                  <a href={adminBucketsPath()}>Return to buckets</a>
                </Button>
              </Block>
            </CardContent>
          </Card>
        </Container>
      </Section>
    );
  }

  const [phase, setPhase] = state<'pending' | 'error'>('pending');
  const [error, setError] = state('');

  async function signOutAndRedirect() {
    setPhase('pending');
    setError('');

    try {
      await logoutAdminSession();
      navigate('/login', { history: 'replace' });
    } catch (caughtError) {
      if (isUnauthorized(caughtError)) {
        navigate('/login', { history: 'replace' });
        return;
      }

      setError(
        caughtError instanceof Error
          ? caughtError.message
          : 'The auth server is unavailable right now.'
      );
      setPhase('error');
    }
  }

  task(() => signOutAndRedirect());

  return (
    <Section size="4">
      <Container size="sm">
        <Card variant="raised">
          <CardHeader>
            <CardTitle>
              {phase() === 'error' ? 'Sign out failed' : 'Signing out'}
            </CardTitle>
            <CardDescription>
              {phase() === 'error'
                ? 'The auth cookie could not be cleared right now.'
                : 'Clearing the auth cookie now.'}
            </CardDescription>
          </CardHeader>
          <CardContent>
            <Block direction="column" gap="md" role="status" aria-atomic="true">
              {phase() === 'pending' ? <Spinner label="Signing out" /> : null}

              {phase() === 'error' ? (
                <Block direction="column" gap="md">
                  <Text tone="danger" size="sm">
                    {error() ||
                      'The auth cookie could not be cleared right now.'}
                  </Text>
                  <Button
                    variant="outline"
                    onPress={() => void signOutAndRedirect()}
                  >
                    Retry
                  </Button>
                </Block>
              ) : null}
            </Block>
          </CardContent>
        </Card>
      </Container>
    </Section>
  );
}
