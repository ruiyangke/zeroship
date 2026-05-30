/*
 * AuthForm — a governed sign-in / sign-up form in a centered Card.
 *
 * Composes the real design-system surface end to end: a centered
 * `Card` wraps the real `Form` (Base UI's consolidated <form>), with
 * `Field`+`Input` rows, a full-width submit `Button`, an optional error
 * `Banner`, an optional social-provider slot fronted by a `Separator`,
 * and a mode-switch `footer`. There is NO bundled auth or network — the
 * block is presentational and emits `onSubmit(values, event)`; the
 * consumer owns the credential exchange.
 *
 * Two modes, one component:
 *
 *   - `signIn` (default): Email + Password (+ an optional
 *     `secondaryAction`, e.g. "Forgot password?"). Password autocompletes
 *     as `current-password`.
 *   - `signUp`: (Name when `showName`) + Email + Password. Password
 *     autocompletes as `new-password` so password managers offer to
 *     generate / save rather than fill an existing credential.
 *
 * Why the field set + autocomplete tokens are not configurable: they ARE
 * the value of the block. The whole reason to reach for AuthForm instead
 * of hand-rolling a <form> is that the email/password/name fields carry
 * the right `type` + `autocomplete` so browser password managers and
 * passkey UIs light up. Exposing those as props would invite call sites
 * to get them subtly wrong.
 *
 * Submit semantics: the underlying Base UI `Form` validates every Field
 * on submit and focuses the first invalid control before our handler runs
 * (all fields are `required`, so an empty field blocks submit and focuses
 * itself — free a11y). When every Field is valid, Base UI calls our native
 * `onSubmit`; we read the values off the native form via `FormData`,
 * `event.preventDefault()` by default (so the page does not navigate), and
 * hand `{email, password, name?}` to the consumer's `onSubmit(values,
 * event)`. We always `preventDefault`, so a consumer who genuinely wants a
 * native navigation should trigger it themselves from the handler.
 *
 * The title renders as an `<h2>` (the block assumes the page owns the
 * `<h1>`); wrap or relevel at the page level if this is a standalone route.
 *
 * Loading: `loading` busies the submit `Button` (aria-busy + disabled
 * spinner) AND disables the whole field set via a native `<fieldset
 * disabled>` so no input can be edited mid-request.
 *
 * Accessibility:
 *   - The `<form>` is named by its title heading via `aria-labelledby`
 *     (a generated id stamped on both the heading and the form).
 *   - Each Field/Input is a real labelled control with the correct `type`
 *     + `autocomplete`; `required` drives `aria-required`.
 *   - The error `Banner` is a live region (`intent="danger"` + `live`
 *     → `role="alert"`), placed above the fields so it is announced and
 *     read before the inputs.
 *   - Submit is a real `type="submit"` Button; the mode-switch and
 *     secondary actions are real Buttons with clear text. Tab order is
 *     source order: fields → secondary → submit → social → footer.
 *
 * data-slot vocabulary: `auth-form` (root Card), `-header`, `-title`,
 * `-error`, `-fields`, `-submit`, `-social`, `-footer`.
 */
import {
  forwardRef,
  useId,
  type ComponentPropsWithoutRef,
  type FormEvent,
  type ReactNode,
} from "react";
import { classnames } from "../../components/_classnames";
import { Card } from "../../components/Card";
import { Form } from "../../components/Form";
import { Field } from "../../components/Field";
import { Input } from "../../components/Input";
import { Button } from "../../components/Button";
import { Separator } from "../../components/Separator";
import { Center } from "../../layouts/Center";
import { Stack } from "../../layouts/Stack";
import { Banner } from "../Banner";

export type AuthFormMode = "signIn" | "signUp";

/** The values gathered from the form on submit. `name` is present only
 *  in `signUp` mode when the Name field is shown. */
export interface AuthFormValues {
  email: string;
  password: string;
  name?: string;
}

export interface AuthFormProps
  extends Omit<ComponentPropsWithoutRef<"form">, "onSubmit" | "title"> {
  /**
   * Which form to show. `signIn` (default) = Email + Password; `signUp`
   * = (Name when `showName`) + Email + Password. Drives the default
   * title / submit label and the password `autocomplete` token.
   */
  mode?: AuthFormMode;

  /**
   * Heading shown at the top of the Card and used as the form's
   * accessible name (via `aria-labelledby`). Defaults to "Sign in" /
   * "Create account" per mode.
   */
  title?: ReactNode;

  /** Supporting copy beneath the title. */
  description?: ReactNode;

  /**
   * Fired on a valid submit. Receives `{email, password, name?}` read
   * from the native form and the `FormEvent`. `event.preventDefault()`
   * has ALREADY been called (the block never navigates) — the consumer
   * owns the credential exchange. Empty required fields block submit and
   * focus the first invalid control before this fires.
   */
  onSubmit?: (values: AuthFormValues, event: FormEvent<HTMLFormElement>) => void;

  /**
   * Mark the request in flight. Busies the submit `Button` (aria-busy +
   * spinner) and disables the entire field set so inputs can't be edited
   * mid-request.
   */
  loading?: boolean;

  /**
   * Error to surface above the fields as a danger `Banner` (a live
   * `role="alert"` region). Pass the message text; clearing it removes
   * the Banner.
   */
  error?: ReactNode;

  /**
   * In `signUp` mode, include a Name field (`autocomplete="name"`).
   * Defaults to `true` in `signUp`; ignored in `signIn`.
   */
  showName?: boolean;

  /** Submit button label. Defaults to "Sign in" / "Create account". */
  submitLabel?: ReactNode;

  /**
   * Optional slot for social / SSO provider buttons. Rendered below the
   * submit Button, separated by an "or" `Separator`. Brand icons are the
   * CONSUMER's to provide — the block bundles none.
   */
  socialActions?: ReactNode;

  /**
   * Optional slot rendered directly beneath the Password field — e.g. a
   * "Forgot password?" link in `signIn`. Sits before the submit Button
   * in tab order.
   */
  secondaryAction?: ReactNode;

  /**
   * Footer content. Defaults to the mode-switch prompt + a `plain`
   * Button that calls `onModeSwitch(other)`. Pass your own node to
   * replace it entirely.
   */
  footer?: ReactNode;

  /**
   * Drives the default footer's mode-switch Button. Called with the
   * OTHER mode (`signUp` from signIn, `signIn` from signUp). Ignored
   * when a custom `footer` is supplied.
   */
  onModeSwitch?: (next: AuthFormMode) => void;
}

const DEFAULT_TITLE: Record<AuthFormMode, string> = {
  signIn: "Sign in",
  signUp: "Create account",
};

const DEFAULT_SUBMIT: Record<AuthFormMode, string> = {
  signIn: "Sign in",
  signUp: "Create account",
};

/** The mode-switch prompt + action label baked into the default footer. */
const FOOTER_COPY: Record<
  AuthFormMode,
  { prompt: string; action: string; next: AuthFormMode }
> = {
  signIn: {
    prompt: "Don't have an account?",
    action: "Create one",
    next: "signUp",
  },
  signUp: {
    prompt: "Already have an account?",
    action: "Sign in",
    next: "signIn",
  },
};

/**
 * Read a string off a FormData entry verbatim (no trimming — a password
 * may legitimately contain leading/trailing spaces). FormData values are
 * `string | File`; auth inputs are always text, so a non-string entry
 * (or a missing field) collapses to "".
 */
function readField(data: FormData, name: string): string {
  const value = data.get(name);
  return typeof value === "string" ? value : "";
}

export const AuthForm = forwardRef<HTMLFormElement, AuthFormProps>(
  function AuthForm(
    {
      mode = "signIn",
      title,
      description,
      onSubmit,
      loading = false,
      error,
      showName = true,
      submitLabel,
      socialActions,
      secondaryAction,
      footer,
      onModeSwitch,
      className,
      ...rest
    },
    ref,
  ) {
    // One stable id per instance, stamped on the heading and referenced
    // by the form's `aria-labelledby` so the form has an accessible name
    // tied to its visible title (no duplicated invisible label).
    const titleId = useId();

    const resolvedTitle = title ?? DEFAULT_TITLE[mode];
    const resolvedSubmit = submitLabel ?? DEFAULT_SUBMIT[mode];
    const showNameField = mode === "signUp" && showName;

    const handleSubmit = (event: FormEvent<HTMLFormElement>) => {
      // Base UI's Form only calls this native onSubmit once every Field
      // reports valid (empty required fields are blocked + focused
      // upstream). We preventDefault by default so the block never
      // navigates, gather values from the native form, and hand them to
      // the consumer.
      event.preventDefault();
      // Guard the loading window. The submit Button is already disabled
      // while `loading`, but an imperative `form.requestSubmit()` (or a
      // synthesized submit) could still fire — and the field set is
      // disabled then, so FormData would omit its controls and emit
      // empty values. Swallow the submit instead of reporting blanks.
      if (loading) return;
      const data = new FormData(event.currentTarget);
      const values: AuthFormValues = {
        email: readField(data, "email"),
        password: readField(data, "password"),
      };
      if (showNameField) {
        values.name = readField(data, "name");
      }
      onSubmit?.(values, event);
    };

    // Default footer: a prompt + a plain Button that switches modes. A
    // consumer-supplied `footer` replaces this whole block.
    const footerContent =
      footer !== undefined ? (
        footer
      ) : (
        <Stack
          direction="row"
          gap={1}
          align="center"
          justify="center"
          wrap
          className="zs-auth-form__footer-default"
        >
          <span className="zs-auth-form__footer-prompt">
            {FOOTER_COPY[mode].prompt}
          </span>
          <Button
            type="button"
            variant="plain"
            size="small"
            disabled={loading}
            onClick={() => onModeSwitch?.(FOOTER_COPY[mode].next)}
          >
            {FOOTER_COPY[mode].action}
          </Button>
        </Stack>
      );

    return (
      <Center
        className={classnames("zs-auth-form-center", className)}
        data-slot="auth-form-center"
      >
        <Card
          variant="elevated"
          className="zs-auth-form"
          data-slot="auth-form"
        >
          <Form
            {...rest}
            ref={ref}
            aria-labelledby={titleId}
            onSubmit={handleSubmit}
            className="zs-auth-form__form"
            data-slot="auth-form-form"
          >
            <Stack gap={5}>
              {/* Header — title heading + optional description. */}
              <Stack gap={1} data-slot="auth-form-header">
                <h2 id={titleId} className="zs-auth-form__title" data-slot="auth-form-title">
                  {resolvedTitle}
                </h2>
                {description != null ? (
                  <p
                    className="zs-auth-form__description"
                    data-slot="auth-form-description"
                  >
                    {description}
                  </p>
                ) : null}
              </Stack>

              {/* Error — danger Banner as a live region, above the fields.
                  The Banner owns its own `data-slot="banner"`, so the
                  block's `auth-form-error` slot lives on a wrapper. */}
              {error != null ? (
                <div data-slot="auth-form-error">
                  <Banner
                    intent="danger"
                    live
                    className="zs-auth-form__error"
                  >
                    {error}
                  </Banner>
                </div>
              ) : null}

              {/* Fields — disabled as a set while loading. The native
                  <fieldset disabled> greys + disables every control at
                  once; `border:0;margin:0;padding:0;min-inline-size:0`
                  in CSS strips the legacy fieldset chrome. */}
              <fieldset
                className="zs-auth-form__fieldset"
                disabled={loading || undefined}
                data-slot="auth-form-fields"
              >
                <Stack gap={4}>
                  {showNameField ? (
                    <Field required disabled={loading}>
                      <Field.Label>
                        Name <Field.Required />
                      </Field.Label>
                      <Input
                        type="text"
                        name="name"
                        autoComplete="name"
                        required
                      />
                    </Field>
                  ) : null}

                  <Field required disabled={loading}>
                    <Field.Label>
                      Email <Field.Required />
                    </Field.Label>
                    <Input
                      type="email"
                      name="email"
                      autoComplete="email"
                      required
                    />
                  </Field>

                  <Field required disabled={loading}>
                    <Field.Label>
                      Password <Field.Required />
                    </Field.Label>
                    <Input
                      type="password"
                      name="password"
                      autoComplete={
                        mode === "signUp" ? "new-password" : "current-password"
                      }
                      required
                    />
                  </Field>

                  {secondaryAction != null ? (
                    <div
                      className="zs-auth-form__secondary"
                      data-slot="auth-form-secondary"
                    >
                      {secondaryAction}
                    </div>
                  ) : null}
                </Stack>
              </fieldset>

              {/* Submit — full-width, busies on loading. */}
              <Button
                type="submit"
                variant="filled"
                size="large"
                loading={loading}
                className="zs-auth-form__submit"
                data-slot="auth-form-submit"
              >
                {resolvedSubmit}
              </Button>

              {/* Social — consumer slot, fronted by an "or" Separator. */}
              {socialActions != null ? (
                <Stack gap={4} data-slot="auth-form-social">
                  <div
                    className="zs-auth-form__or"
                    role="presentation"
                  >
                    <Separator className="zs-auth-form__or-line" />
                    <span className="zs-auth-form__or-label">or</span>
                    <Separator className="zs-auth-form__or-line" />
                  </div>
                  <Stack gap={3} className="zs-auth-form__social-actions">
                    {socialActions}
                  </Stack>
                </Stack>
              ) : null}

              {/* Footer — mode-switch prompt or consumer override. */}
              {footerContent != null ? (
                <div
                  className="zs-auth-form__footer"
                  data-slot="auth-form-footer"
                >
                  {footerContent}
                </div>
              ) : null}
            </Stack>
          </Form>
        </Card>
      </Center>
    );
  },
);
AuthForm.displayName = "AuthForm";
