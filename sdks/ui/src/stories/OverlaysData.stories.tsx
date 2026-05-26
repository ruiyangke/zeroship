import { useState } from "react";
import type { Meta, StoryObj } from "@storybook/react";
import {
  Badge,
  Button,
  Dialog,
  Table,
  Tabs,
  Toast,
  ToastViewport,
} from "../index";

const rows = [
  { id: "evt_001", time: "12:14:02", level: "info", message: "Booted worker" },
  { id: "evt_002", time: "12:14:08", level: "request", message: "Served /" },
  { id: "evt_003", time: "12:14:11", level: "warn", message: "Retried upstream request" },
];

function OverlaysDataDemo() {
  const [dialogOpen, setDialogOpen] = useState(false);
  return (
    <div className="zs-story-shell zs-story-stack">
      <div className="zs-story-row">
        <Button onClick={() => setDialogOpen(true)}>Open dialog</Button>
        <Toast title="App is live" tone="success" action={<Button size="sm">Open</Button>}>
          The URL is ready to share.
        </Toast>
      </div>
      <Tabs
        items={[
          {
            value: "ledger",
            label: "Ledger",
            content: (
              <Table
                aria-label="Runtime events"
                rows={rows}
                getRowKey={(row) => row.id}
                columns={[
                  { key: "time", header: "Time" },
                  {
                    key: "level",
                    header: "Level",
                    cell: (row) => (
                      <Badge tone={row.level === "warn" ? "warn" : "info"}>
                        {String(row.level)}
                      </Badge>
                    ),
                  },
                  { key: "message", header: "Message" },
                ]}
              />
            ),
          },
          {
            value: "notes",
            label: "Notes",
            content: "Tabs carry keyboard arrow navigation and token-driven focus states.",
          },
        ]}
      />
      <ToastViewport>
        <Toast title="Autosaved" tone="info">
          Changes were kept in the current draft.
        </Toast>
      </ToastViewport>
      <Dialog
        open={dialogOpen}
        onOpenChange={setDialogOpen}
        title="Delete project?"
        description="Type-to-confirm flows use this dialog primitive."
        footer={
          <>
            <Button variant="ghost" onClick={() => setDialogOpen(false)}>
              Cancel
            </Button>
            <Button variant="danger" onClick={() => setDialogOpen(false)}>
              Delete forever
            </Button>
          </>
        }
      >
        This operation removes deploys, logs, secrets, and the project record.
      </Dialog>
    </div>
  );
}

const meta = {
  title: "Primitives/Overlays and Data",
  component: OverlaysDataDemo,
  tags: ["autodocs"],
} satisfies Meta<typeof OverlaysDataDemo>;

export default meta;
type Story = StoryObj<typeof meta>;

export const DialogTabsToastTable: Story = {};
