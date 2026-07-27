import { state } from '@askrjs/askr';
import { currentRoute, navigate } from '@askrjs/askr/router';
import {
  Button,
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
  Container,
  Field,
  Input,
  Label,
  Stack,
  Section,
  Text,
} from '@askrjs/themes/components';
import {
  isDevAuthBypassed,
  loginAdminSession,
} from '../../features/auth/admin-session';
import { adminBucketsPath } from '../../shared/routes';

function resolveNextTarget() {
  const next = currentRoute().query.get('next');
  return next && next.startsWith('/') && !next.startsWith('//')
    ? next
    : adminBucketsPath();
}

export default function LoginPage() {
  const devAuthBypassed = isDevAuthBypassed();
  const nextTarget = resolveNextTarget();

  if (devAuthBypassed) {
    return (
      <Section size="4">
        <Container size="sm">
          <Card variant="raised">
            <CardHeader>
              <CardTitle>Local development mode</CardTitle>
            </CardHeader>
            <CardContent>
              <Stack gap="4">
                <p>Admin sign-in is bypassed while running the local dev UI.</p>
                <Button onPress={() => navigate(nextTarget)}>
                  Open buckets
                </Button>
              </Stack>
            </CardContent>
          </Card>
        </Container>
      </Section>
    );
  }

  const [error, setError] = state('');
  const [isSigningIn, setIsSigningIn] = state(false);
  const [username, setUsername] = state('');
  const [password, setPassword] = state('');

  async function handleSubmit(event?: { preventDefault?: () => void }) {
    event?.preventDefault?.();

    if (isSigningIn()) {
      return;
    }

    setError('');
    setIsSigningIn(true);

    try {
      await loginAdminSession({
        username: username().trim(),
        password: password(),
      });
      navigate(nextTarget, { history: 'replace' });
    } catch (caughtError) {
      setError(
        caughtError instanceof Error
          ? caughtError.message
          : 'The admin server is unavailable right now.'
      );
    } finally {
      setIsSigningIn(false);
    }
  }

  return (
    <Section size="4">
      <Container size="sm">
        <Card variant="raised">
          <CardHeader>
            <CardTitle>Sign in</CardTitle>
            <CardDescription>Sign in to Sqrzl to continue.</CardDescription>
          </CardHeader>
          <CardContent>
            <form onSubmit={handleSubmit}>
              <Stack gap="4">
                <Field>
                  <Label for="username">Username</Label>
                  <Input
                    id="username"
                    name="username"
                    type="text"
                    autoComplete="username"
                    disabled={isSigningIn()}
                    placeholder="username"
                    value={username()}
                    onInput={(event: Event) => {
                      setUsername((event.target as HTMLInputElement).value);
                    }}
                  />
                </Field>
                <Field>
                  <Label for="password">Password</Label>
                  <Input
                    id="password"
                    name="password"
                    type="password"
                    autoComplete="current-password"
                    disabled={isSigningIn()}
                    placeholder="password"
                    value={password()}
                    onInput={(event: Event) => {
                      setPassword((event.target as HTMLInputElement).value);
                    }}
                  />
                </Field>
                <div aria-live="assertive" aria-atomic="true">
                  {error() ? (
                    <Text tone="danger" size="sm">
                      {error()}
                    </Text>
                  ) : null}
                </div>
                <Button
                  type="submit"
                  variant="primary"
                  width="full"
                  disabled={isSigningIn()}
                  aria-busy={isSigningIn()}
                >
                  {isSigningIn() ? 'Signing in...' : 'Sign in'}
                </Button>
              </Stack>
            </form>
          </CardContent>
        </Card>
      </Container>
    </Section>
  );
}
