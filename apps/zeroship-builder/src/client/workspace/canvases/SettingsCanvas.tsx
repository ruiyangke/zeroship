// ─── SettingsCanvas — identity, archive, danger zone (`docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §9.7) ───
//
// The console is a PURE creator app: a project is a KV-local per-thread
// sandbox session. There is no plan / billing (no control plane) and no
// deployed-app concepts. Three sections.
//   • Identity: project name (read-only V1), project id (copyable).
//   • Archive: KV-local soft-delete toggle.
//   • Danger zone: typed-confirmation delete (KV-local). Type the exact
//     project name to enable the red Delete button; on confirm,
//     deleteProject then navigate home.

import { useState } from "react";
import { useNavigate } from "react-router-dom";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import {
  AlertDialog,
  Button,
  Card,
  Container,
  FormSection,
  Input,
} from "@zeroship/ui";
import {
  archiveProject,
  deleteProject,
  unarchiveProject,
  type ProjectRecord,
} from "../../api";
import "./SettingsCanvas.css";

export interface SettingsCanvasProps {
  appId: string;
  app?: ProjectRecord;
}

export function SettingsCanvas({ appId, app }: SettingsCanvasProps) {
  const navigate = useNavigate();
  const qc = useQueryClient();
  const [confirmOpen, setConfirmOpen] = useState(false);

  const del = useMutation({
    mutationFn: async () => deleteProject(appId),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["projects"] });
      navigate("/home", { replace: true });
    },
  });
  const archive = useMutation({
    mutationFn: async () => archiveProject({ appId }),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["projects"] });
      qc.invalidateQueries({ queryKey: ["app", appId] });
      navigate("/home", { replace: true });
    },
  });
  const unarchive = useMutation({
    mutationFn: async () => unarchiveProject({ appId }),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["projects"] });
      qc.invalidateQueries({ queryKey: ["app", appId] });
    },
  });

  const isArchived = app?.archived === true;

  return (
    <div data-testid="settings-canvas" className="settings-canvas">
      <Container size="md" padX={6} className="settings-canvas__sections">
        <FormSection
          orientation="aside"
          title="Identity"
          description="The name and id your project lives under."
        >
          <Input label="Project name" value={app?.name ?? "—"} readOnly />
          <CopyField label="Project id" value={appId} />
        </FormSection>

        <FormSection
          orientation="aside"
          title="Archive"
          description="Tuck a project away without losing it. Reversible — restore any time from the Archived view on Home."
        >
          <Card data-testid="settings-archive">
            <p className="settings-canvas__card-title">
              {isArchived
                ? "This project is archived"
                : "Archive this project"}
            </p>
            <p className="settings-canvas__card-copy">
              {isArchived
                ? "It's hidden from the default Home view but everything's intact. Restore to bring it back."
                : "Hide it from the default Home view. Your code and settings stay put. You can restore it later."}
            </p>
            {isArchived ? (
              <Button
                variant="tinted"
                onClick={() => unarchive.mutate()}
                disabled={unarchive.isPending}
                loading={unarchive.isPending}
                data-testid="settings-unarchive"
              >
                Restore project
              </Button>
            ) : (
              <Button
                variant="tinted"
                onClick={() => archive.mutate()}
                disabled={archive.isPending}
                loading={archive.isPending}
                data-testid="settings-archive-btn"
              >
                Archive project
              </Button>
            )}
          </Card>
        </FormSection>

        <FormSection
          orientation="aside"
          title={<span className="settings-canvas__danger-title">Danger zone</span>}
          description="Permanent operations. Take a moment."
        >
          <Card variant="outline">
            <p className="settings-canvas__card-title settings-canvas__danger-title">
              Delete this project
            </p>
            <p className="settings-canvas__card-copy">
              The project and its sandbox workspace go away. This can't be
              undone.
            </p>
            <Button
              variant="filled"
              intent="destructive"
              onClick={() => setConfirmOpen(true)}
              data-testid="settings-delete"
            >
              Delete project
            </Button>
          </Card>
        </FormSection>
      </Container>

      <DeleteConfirm
        open={confirmOpen}
        onClose={() => setConfirmOpen(false)}
        appName={app?.name ?? ""}
        onConfirm={() => del.mutate()}
        deleting={del.isPending}
      />
    </div>
  );
}

// ─── delete confirmation: type-the-name gate ────────────────────

function DeleteConfirm({
  open,
  onClose,
  appName,
  onConfirm,
  deleting,
}: {
  open: boolean;
  onClose: () => void;
  appName: string;
  onConfirm: () => void;
  deleting: boolean;
}) {
  const [typed, setTyped] = useState("");
  const matches = appName.length > 0 && typed === appName;

  return (
    <AlertDialog
      open={open}
      onOpenChange={(nextOpen) => {
        // While a delete is in flight, do nothing — the AlertDialog
        // must remain open until the mutation resolves. Once it's
        // safe to close, surface the close intent to the parent.
        if (deleting) return;
        if (!nextOpen) onClose();
      }}
    >
      <AlertDialog.Portal>
        <AlertDialog.Backdrop />
        <AlertDialog.Popup data-testid="settings-delete-modal">
          <AlertDialog.Header>
            <AlertDialog.Title>Delete project?</AlertDialog.Title>
            <AlertDialog.Description>
              This permanently deletes the project and its sandbox
              workspace. Type{" "}
              <code className="settings-canvas__code">{appName || "—"}</code>{" "}
              to confirm.
            </AlertDialog.Description>
          </AlertDialog.Header>
          <AlertDialog.Body>
            <Input
              autoFocus
              value={typed}
              onChange={(e) => setTyped(e.target.value)}
              placeholder={appName}
              data-testid="settings-delete-typed"
              label="Project name"
            />
          </AlertDialog.Body>
          <AlertDialog.Footer>
            <AlertDialog.Cancel disabled={deleting}>cancel</AlertDialog.Cancel>
            <AlertDialog.Action
              tone="destructive"
              disabled={!matches || deleting}
              loading={deleting}
              preventClose
              onClick={() => {
                if (matches) onConfirm();
              }}
              data-testid="settings-delete-confirm"
            >
              Delete forever
            </AlertDialog.Action>
          </AlertDialog.Footer>
        </AlertDialog.Popup>
      </AlertDialog.Portal>
    </AlertDialog>
  );
}

// ─── copy-id field: read-only mono value + copy button ──────────

function CopyField({ label, value }: { label: string; value: string }) {
  const [copied, setCopied] = useState(false);
  function copy() {
    void navigator.clipboard?.writeText(value).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 1200);
    });
  }
  return (
    <div className="settings-canvas__copy-field">
      <span className="settings-canvas__copy-label">{label}</span>
      <div className="settings-canvas__copy-row">
        <Input
          value={value}
          readOnly
          variant="filled"
          aria-label={label}
          className="settings-canvas__copy-input"
          wrapperProps={{ className: "settings-canvas__copy-input-wrap" }}
          data-testid="settings-copy-id"
        />
        <Button
          variant="tinted"
          onClick={copy}
          data-testid="settings-copy-id-btn"
        >
          {copied ? "copied" : "copy"}
        </Button>
      </div>
    </div>
  );
}
