import type { Meta, StoryObj } from "@storybook/react";
import { expect, fn, userEvent, within } from "@storybook/test";
import { AuthForm } from "../blocks";
import { Button } from "../components/Button";

const meta: Meta<typeof AuthForm> = {
  title: "Blocks/AuthForm",
  component: AuthForm,
  parameters: { layout: "fullscreen" },
  args: {
    onSubmit: fn(),
    onModeSwitch: fn(),
  },
};

export default meta;

type Story = StoryObj<typeof AuthForm>;

/* ─── 1. Sign in (default) ─────────────────────────────────────────────
 * The everyday case: Email + Password, a "Forgot password?" secondary
 * action, the full-width submit, and a mode-switch footer. The play()
 * fills the form, submits, and asserts onSubmit fires with the typed
 * values + that the password input carries `current-password`. */
export const SignIn: Story = {
  name: "Sign in (email · password)",
  parameters: {
    docs: {
      description: {
        story:
          "Default `signIn` mode. Email + Password, both required, with " +
          "the password field autocompleting as `current-password` so " +
          "password managers fill an existing credential. The form is " +
          "named by its heading via `aria-labelledby`; submit reads the " +
          "values off the native form and calls `onSubmit(values, event)` " +
          "with `preventDefault` already applied.",
      },
    },
  },
  args: {
    mode: "signIn",
    description: "Welcome back. Enter your details to continue.",
    secondaryAction: (
      <Button variant="plain" size="small" type="button">
        Forgot password?
      </Button>
    ),
  },
  play: async ({ canvasElement, args }) => {
    const canvas = within(canvasElement);

    // The form is named by its heading: aria-labelledby resolves to the
    // visible title element, whose text is the accessible name.
    const form = canvasElement.querySelector("form");
    const labelledBy = form?.getAttribute("aria-labelledby");
    await expect(labelledBy).toBeTruthy();
    const heading = canvasElement.querySelector(`#${CSS.escape(labelledBy!)}`);
    await expect(heading).toHaveTextContent(/sign in/i);

    const email = canvas.getByLabelText(/email/i);
    const password = canvas.getByLabelText(/password/i);

    // signIn password autocompletes as an existing credential.
    await expect(password).toHaveAttribute("autocomplete", "current-password");
    await expect(email).toHaveAttribute("type", "email");
    await expect(email).toHaveAttribute("autocomplete", "email");

    await userEvent.type(email, "ada@example.com");
    await userEvent.type(password, "lovelace123");

    const submit = canvas.getByRole("button", { name: /^sign in$/i });
    await userEvent.click(submit);

    // onSubmit fires with the typed values; default mode has no `name`.
    await expect(args.onSubmit).toHaveBeenCalledTimes(1);
    const [values] = (args.onSubmit as ReturnType<typeof fn>).mock.calls[0];
    await expect(values).toEqual({
      email: "ada@example.com",
      password: "lovelace123",
    });

    // Mode-switch link drives onModeSwitch("signUp").
    const switchLink = canvas.getByRole("button", { name: /create one/i });
    await userEvent.click(switchLink);
    await expect(args.onModeSwitch).toHaveBeenCalledWith("signUp");
  },
};

/* ─── 2. Sign up (name · email · password) ─────────────────────────────
 * The signUp field set adds a Name field; the password autocompletes as
 * `new-password` so managers offer to generate / save. */
export const SignUp: Story = {
  name: "Sign up (name · email · password)",
  parameters: {
    docs: {
      description: {
        story:
          "`signUp` mode. Adds a Name field (`autocomplete=\"name\"`) above " +
          "Email + Password; the password autocompletes as `new-password` " +
          "so password managers offer to generate and save a new " +
          "credential. The mode-switch footer flips to \"Already have an " +
          "account? Sign in\".",
      },
    },
  },
  args: {
    mode: "signUp",
    description: "Create your account to get started.",
  },
  play: async ({ canvasElement, args }) => {
    const canvas = within(canvasElement);

    const name = canvas.getByLabelText(/name/i);
    const email = canvas.getByLabelText(/email/i);
    const password = canvas.getByLabelText(/password/i);

    // signUp password autocompletes as a NEW credential.
    await expect(password).toHaveAttribute("autocomplete", "new-password");
    await expect(name).toHaveAttribute("autocomplete", "name");

    await userEvent.type(name, "Ada Lovelace");
    await userEvent.type(email, "ada@example.com");
    await userEvent.type(password, "newpass456");

    const submit = canvas.getByRole("button", { name: /^create account$/i });
    await userEvent.click(submit);

    await expect(args.onSubmit).toHaveBeenCalledTimes(1);
    const [values] = (args.onSubmit as ReturnType<typeof fn>).mock.calls[0];
    await expect(values).toEqual({
      name: "Ada Lovelace",
      email: "ada@example.com",
      password: "newpass456",
    });

    // Footer flips to the signIn prompt.
    const switchLink = canvas.getByRole("button", { name: /^sign in$/i });
    await userEvent.click(switchLink);
    await expect(args.onModeSwitch).toHaveBeenCalledWith("signIn");
  },
};

/* ─── 3. With error ────────────────────────────────────────────────────
 * The error Banner renders above the fields as a danger live region. */
export const WithError: Story = {
  name: "With error (live danger Banner)",
  parameters: {
    docs: {
      description: {
        story:
          "An `error` surfaces above the fields as a danger `Banner` that " +
          "is a live region (`role=\"alert\"`), so assistive tech " +
          "announces it when it appears in response to a failed attempt.",
      },
    },
  },
  args: {
    mode: "signIn",
    error: "That email or password is incorrect.",
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);

    // The error Banner is present and is a live alert region (danger
    // intent + live → role="alert", assertive).
    const alert = canvas.getByRole("alert");
    await expect(alert).toHaveTextContent(/incorrect/i);

    // The block's slot wrapper carries `auth-form-error`.
    const slot = canvasElement.querySelector('[data-slot="auth-form-error"]');
    await expect(slot).not.toBeNull();
    await expect(slot).toContainElement(alert);
  },
};

/* ─── 4. Loading ───────────────────────────────────────────────────────
 * The submit Button busies and the field set is disabled. */
export const Loading: Story = {
  name: "Loading (submit busy · fields disabled)",
  parameters: {
    docs: {
      description: {
        story:
          "`loading` busies the submit `Button` (`aria-busy`, spinner, " +
          "disabled) and disables the entire field set via a native " +
          "`<fieldset disabled>` so no input can be edited mid-request.",
      },
    },
  },
  args: {
    mode: "signIn",
    loading: true,
  },
  play: async ({ canvasElement, args }) => {
    const canvas = within(canvasElement);

    const submit = canvas.getByRole("button", { name: /^sign in$/i });
    await expect(submit).toHaveAttribute("aria-busy", "true");
    await expect(submit).toBeDisabled();

    // Fields are disabled via the wrapping fieldset.
    const email = canvas.getByLabelText(/email/i);
    await expect(email).toBeDisabled();

    // Regression (loading guard): an imperative submit while loading must
    // NOT emit values. Without the `if (loading) return` guard the
    // disabled fieldset drops its controls from FormData and onSubmit
    // would fire with blank fields.
    const form = canvasElement.querySelector("form");
    form?.requestSubmit();
    await expect(args.onSubmit).not.toHaveBeenCalled();
  },
};

/* ─── 5. With social ───────────────────────────────────────────────────
 * Provider buttons in the socialActions slot, fronted by an "or"
 * Separator. Brand icons are the consumer's to supply — none bundled. */
export const WithSocial: Story = {
  name: "With social (provider slot + 'or' divider)",
  parameters: {
    docs: {
      description: {
        story:
          "The `socialActions` slot renders consumer-supplied provider " +
          "buttons below the submit, separated by a labelled \"or\" " +
          "divider. The block bundles no brand icons — providers are " +
          "entirely the consumer's to supply.",
      },
    },
  },
  args: {
    mode: "signIn",
    socialActions: (
      <>
        {/* Consumer-owned width — providers stretch via the consumer's own
            style, NOT by reaching into the block's private classes. */}
        <Button variant="gray" size="large" type="button" style={{ inlineSize: "100%" }}>
          Continue with Google
        </Button>
        <Button variant="gray" size="large" type="button" style={{ inlineSize: "100%" }}>
          Continue with GitHub
        </Button>
      </>
    ),
  },
  play: async ({ canvasElement }) => {
    const canvas = within(canvasElement);

    // The "or" divider label is present and the provider buttons render.
    await expect(canvas.getByText(/^or$/i)).toBeInTheDocument();
    await expect(
      canvas.getByRole("button", { name: /continue with google/i }),
    ).toBeInTheDocument();
    await expect(
      canvas.getByRole("button", { name: /continue with github/i }),
    ).toBeInTheDocument();
  },
};
