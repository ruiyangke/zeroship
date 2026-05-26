import { useState } from "react";
import type { Meta, StoryObj } from "@storybook/react";
import {
  Accordion,
  Badge,
  Button,
  Card,
  Checkbox,
  Chip,
  Dialog,
  EmptyState,
  Input,
  Menu,
  Popover,
  RadioGroup,
  Select,
  Separator,
  Spinner,
  Switch,
  Table,
  Tabs,
  Textarea,
  Toast,
  ToastViewport,
  Tooltip,
} from "../index";

const meta = {
  title: "Components/Base UI",
  tags: ["autodocs"],
} satisfies Meta;

export default meta;
type Story = StoryObj<typeof meta>;

export const ButtonStates: Story = {
  render: () => (
    <div className="zs-story-shell zs-story-stack">
      <div className="zs-story-row">
        <Button>Primary</Button>
        <Button variant="secondary">Secondary</Button>
        <Button variant="ghost">Ghost</Button>
        <Button variant="danger">Danger</Button>
      </div>
      <div className="zs-story-row">
        <Button size="sm">Small</Button>
        <Button size="md">Medium</Button>
        <Button size="lg">Large</Button>
        <Button loading>Building</Button>
        <Button disabled>Disabled</Button>
      </div>
    </div>
  ),
};

export const FieldInputs: Story = {
  render: () => (
    <div className="zs-story-shell zs-story-grid">
      <Input label="Project name" defaultValue="Supper Society" />
      <Input label="Slug" defaultValue="supper-society" hint="Used in the public URL." />
      <Input label="API key" defaultValue="sk_live_hidden" readOnly />
      <Input label="Budget" defaultValue="$15" error="Enter a monthly limit." />
      <Textarea
        label="Brief"
        defaultValue="A recipe journal for a supper club with ratings and a host vote."
      />
      <Select label="Plan" defaultValue="maker">
        <option value="free">Free</option>
        <option value="maker">Maker</option>
        <option value="pro">Pro</option>
      </Select>
    </div>
  ),
};

export const ChoiceControls: Story = {
  render: () => (
    <div className="zs-story-shell zs-story-grid">
      <Switch label="Public app" defaultChecked hint="Available at the public route." />
      <Checkbox label="Require sign-in" defaultChecked />
      <Checkbox label="Mixed inherited setting" indeterminate />
      <RadioGroup
        label="Plan"
        defaultValue="maker"
        items={[
          { value: "free", label: "Free" },
          { value: "maker", label: "Maker" },
          { value: "pro", label: "Pro", disabled: true },
        ]}
      />
    </div>
  ),
};

function DialogOpenDemo() {
  // Open on load so screenshot/a11y runs catch it, but stateful so Escape /
  // backdrop / Close actually dismiss it.
  const [open, setOpen] = useState(true);
  return (
    <div className="zs-story-shell">
      <Dialog
        open={open}
        onOpenChange={setOpen}
        title="Delete project?"
        description="Dialog focus, dismissal, and labels come from Base UI."
        footer={
          <>
            <Button variant="ghost" onClick={() => setOpen(false)}>
              Cancel
            </Button>
            <Button variant="danger" onClick={() => setOpen(false)}>
              Delete forever
            </Button>
          </>
        }
      >
        Type the project name before continuing.
      </Dialog>
    </div>
  );
}

export const DialogOpen: Story = {
  render: () => <DialogOpenDemo />,
};

function DialogKeyboardDemo() {
  const [open, setOpen] = useState(false);
  return (
    <div className="zs-story-shell">
      <Button onClick={() => setOpen(true)}>Open keyboard dialog</Button>
      <Dialog
        open={open}
        onOpenChange={setOpen}
        title="Keyboard dialog"
        description="Used by the automated keyboard sanity check."
        footer={
          <>
            <Button variant="ghost" onClick={() => setOpen(false)}>
              Cancel
            </Button>
            <Button onClick={() => setOpen(false)}>Confirm</Button>
          </>
        }
      >
        Focus should stay in the dialog until Escape closes it.
      </Dialog>
    </div>
  );
}

export const DialogKeyboard: Story = {
  render: () => <DialogKeyboardDemo />,
};

export const PopoverOpen: Story = {
  render: () => (
    <div className="zs-story-shell">
      <Popover
        trigger={<Button variant="secondary">Open popover</Button>}
        title="Runtime note"
        description="Positioning and outside dismissal are handled by Base UI."
        defaultOpen
      >
        Popover content inherits the current theme even though it is portaled.
      </Popover>
    </div>
  ),
};

export const TooltipOpen: Story = {
  render: () => (
    <div className="zs-story-shell">
      <Tooltip content="Tooltips are labelled and positioned by Base UI." open>
        <Button variant="secondary">Hover or focus</Button>
      </Tooltip>
    </div>
  ),
};

export const MenuOpen: Story = {
  render: () => (
    <div className="zs-story-shell">
      <Menu
        trigger={<Button variant="secondary">Open menu</Button>}
        defaultOpen
        items={[
          { value: "rename", label: "Rename" },
          { value: "duplicate", label: "Duplicate" },
          { value: "sep", separator: true },
          { value: "delete", label: "Delete", disabled: true },
        ]}
      />
    </div>
  ),
};

export const TabsAccordionTable: Story = {
  render: () => (
    <div className="zs-story-shell zs-story-stack">
      <Tabs
        defaultValue="ledger"
        items={[
          {
            value: "ledger",
            label: "Ledger",
            content: (
              <Table
                aria-label="Runtime events"
                rows={[
                  { id: "evt_001", time: "12:14:02", level: "info", message: "Booted worker" },
                  { id: "evt_002", time: "12:14:08", level: "request", message: "Served /" },
                ]}
                getRowKey={(row) => String(row.id)}
                columns={[
                  { key: "time", header: "Time" },
                  {
                    key: "level",
                    header: "Level",
                    cell: (row) => <Badge tone="info">{String(row.level)}</Badge>,
                  },
                  { key: "message", header: "Message" },
                ]}
              />
            ),
          },
          {
            value: "notes",
            label: "Notes",
            content: "Tabs use roving focus and arrow-key activation from Base UI.",
          },
        ]}
      />
      <Accordion
        defaultValue={["routing"]}
        items={[
          {
            value: "routing",
            title: "Routing",
            content: "Manifest rules are evaluated before worker dispatch.",
          },
          {
            value: "billing",
            title: "Billing",
            content: "Creators keep revenue after Stripe fees and the platform share.",
          },
        ]}
      />
    </div>
  ),
};

export const SurfacesAndFeedback: Story = {
  render: () => (
    <div className="zs-story-shell zs-story-stack">
      <div className="zs-story-grid">
        <Card className="zs-story-card-pad">
          <Badge>Draft</Badge>
          <p>Neutral card with tokenized border, surface, and type.</p>
        </Card>
        <Card tone="accent" className="zs-story-card-pad">
          <Badge tone="success">Live</Badge>
          <p>Accent card for selected or promoted states.</p>
        </Card>
      </div>
      <div className="zs-story-row">
        <Chip active>All</Chip>
        <Chip>Drafts</Chip>
        <Chip tone="warn">Needs review</Chip>
        <Spinner />
      </div>
      <Separator />
      <EmptyState
        title="No deploys yet"
        description="Build the first version to populate this table."
        action={<Button variant="secondary">Open preview</Button>}
      />
      <ToastViewport>
        <Toast title="Autosaved" tone="info">
          Changes were kept in the current draft.
        </Toast>
      </ToastViewport>
    </div>
  ),
};

function PortaledPopupProofDemo() {
  // Open on load (proof needs the popups visible) but closeable via Escape /
  // backdrop / Close. The Select stays defaultOpen to prove its popup stacks
  // ABOVE the dialog (z-dropdown > z-modal).
  const [open, setOpen] = useState(true);
  return (
    <div className="zs-story-shell">
      <Dialog
        open={open}
        onOpenChange={setOpen}
        title="Dusk portal proof"
        description="The dialog and select popup are portaled under document.body."
      >
        <Select label="Plan" defaultValue="maker" defaultOpen>
          <option value="free">Free</option>
          <option value="maker">Maker</option>
          <option value="pro">Pro</option>
        </Select>
      </Dialog>
    </div>
  );
}

export const PortaledPopupProof: Story = {
  render: () => <PortaledPopupProofDemo />,
};
