import { state } from '@askrjs/askr';
import { navigate } from '@askrjs/askr/router';
import { Input } from '@askrjs/ui';
import { Button, Field } from '@askrjs/themes/controls';
import { Container, Section, Stack } from '@askrjs/themes/layouts';
import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
} from '@askrjs/themes/surfaces';
import { loginAdminSession } from '../../features/auth/admin-session';
import { adminBucketsPath } from '../../shared/routes';

function returnPath(): string {
  if (typeof window === 'undefined') {
    return adminBucketsPath();
  }

  const candidate = new URLSearchParams(window.location.search).get('next');
  return candidate?.startsWith('/') && !candidate.startsWith('//')
    ? candidate
    : adminBucketsPath();
}

export default function LoginPage() {
  const [error, setError] = state('');
  const [pending, setPending] = state(false);
  const [username, setUsername] = state('');
  const [password, setPassword] = state('');

  async function handleSubmit(event: Event) {
    if (pending()) {
      return;
    }

    if (!(event.target instanceof Element)) {
      return;
    }

    const credentials = {
      username: username().trim(),
      password: password(),
    };

    setPending(true);
    setError('');

    try {
      await loginAdminSession(credentials);
      navigate(returnPath());
    } catch (caughtError) {
      setError(
        caughtError instanceof Error
          ? caughtError.message
          : 'The admin server is unavailable right now.'
      );
    } finally {
      setPending(false);
    }
  }

  function onUsernameInput(event: Event) {
    const value =
      event.target instanceof HTMLInputElement ? event.target.value : '';
    setUsername(value);
  }

  function onPasswordInput(event: Event) {
    const value =
      event.target instanceof HTMLInputElement ? event.target.value : '';
    setPassword(value);
  }

  return (
    <Section size="4">
      <Container size="sm">
        <Card variant="raised">
          <CardHeader>
            <CardTitle>Sign in</CardTitle>
          </CardHeader>
          <CardContent>
            <form
              onSubmit={(event: Event) => {
                event.preventDefault();
                void handleSubmit(event);
              }}
            >
              <Stack gap="4">
                <Field>
                  <label htmlFor="username">Username</label>
                  <Input
                    id="username"
                    name="username"
                    type="text"
                    autoComplete="username"
                    disabled={pending()}
                    placeholder="username"
                    onInput={onUsernameInput}
                  />
                </Field>
                <Field>
                  <label htmlFor="password">Password</label>
                  <Input
                    id="password"
                    name="password"
                    type="password"
                    autoComplete="current-password"
                    disabled={pending()}
                    placeholder="password"
                    onInput={onPasswordInput}
                  />
                </Field>
                {error() ? <p role="alert">{error()}</p> : null}
                <Button type="submit" disabled={pending()}>
                  {pending() ? 'Signing in...' : 'Sign in'}
                </Button>
              </Stack>
            </form>
          </CardContent>
        </Card>
      </Container>
    </Section>
  );
}
