import { useRef, useState } from "react";
import { useLingui } from "@lingui/react";
import { msg } from "@lingui/core/macro";
import { markets } from "@gather/meal-kit/catalog";
import {
  staffRoles,
  type StaffRole,
  type StaffMember,
  type StaffSettings,
} from "@gather/meal-kit/staff-domain";
import * as api from "../api";
import {
  Button,
  Field,
  Input,
  Badge,
  Loading,
  Empty,
  ErrorState,
  useLoad,
} from "@gather/meal-kit/components/shared";
import { Checkbox } from "@gather/meal-kit/components/ui/checkbox";
import { Label } from "@gather/meal-kit/components/ui/label";
import { Card } from "@gather/meal-kit/components/ui/card";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogTitle,
} from "@gather/meal-kit/components/ui/dialog";
import { FieldSet, FieldLegend } from "@gather/meal-kit/components/ui/field";
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from "@gather/meal-kit/components/ui/collapsible";

function useRoleLabels(): Record<StaffRole, string> {
  const { _: t } = useLingui();
  return {
    manager: t(msg`Country manager`),
    menu_editor: t(msg`Menu editor`),
    fulfillment: t(msg`Fulfillment`),
    support: t(msg`Customer support`),
  };
}
function AccessSummary({ member }: { member: StaffSettings }) {
  const labels = useRoleLabels();
  const { _: t } = useLingui();
  return (
    <div className="text-sm space-y-1">
      {member.recipeEditor && <p>{t(msg`Shared recipe library editor`)}</p>}
      {member.grants.map((grant) => (
        <p key={grant.market}>
          {t(markets[grant.market].name)}:{" "}
          {grant.roles.map((role) => labels[role]).join(", ")}
        </p>
      ))}
      {!member.recipeEditor && !member.grants.length && (
        <p>{t(msg`No workspace access`)}</p>
      )}
    </div>
  );
}
export function StaffTeam() {
  const { _: t, i18n } = useLingui();
  const result = useLoad(async () => api.getStaffTeam(), []);
  const [editing, setEditing] = useState<StaffMember | "new" | null>(null);
  return (
    <div className="space-y-6 mt-6">
      <div className="flex justify-between items-center flex-wrap gap-4">
        <h2>{t(msg`Team access`)}</h2>
        <div className="flex gap-3">
          <Button variant="outline" onClick={result.refresh}>
            {t(msg`Refresh`)}
          </Button>
          <Button onClick={() => setEditing("new")}>
            {t(msg`Add team member`)}
          </Button>
        </div>
      </div>
      <p className="text-sm text-muted-foreground">
        {t(
          msg`Give each colleague access to the countries and work they manage. Changes apply to their next request.`,
        )}
      </p>
      {result.error ? (
        <ErrorState error={result.error} retry={result.refresh} />
      ) : !result.data ? (
        <Loading />
      ) : (
        <>
          {result.data.members.length ? (
            <div className="space-y-4">
              {result.data.members.map((member) => (
                <Card className="p-5" key={member.id}>
                  <div className="flex items-start justify-between gap-4">
                    <div className="min-w-0 space-y-2">
                      <h3 className="text-xl">{member.name}</h3>
                      <p className="break-all text-xs text-muted-foreground">
                        {member.subject}
                      </p>
                      <Badge>
                        {member.active
                          ? t(msg`Active`)
                          : t(msg`Access suspended`)}
                      </Badge>
                      <AccessSummary member={member} />
                    </div>
                    <Button
                      variant="outline"
                      onClick={() => setEditing(member)}
                      aria-label={t(msg`Edit access for ${member.name}`)}
                    >
                      {t(msg`Edit access`)}
                    </Button>
                  </div>
                </Card>
              ))}
            </div>
          ) : (
            <Empty
              title={t(msg`Your team starts here`)}
              text={t(msg`Add a colleague and choose what they can manage.`)}
            />
          )}
          <Card className="p-5 space-y-3">
            <h3 className="text-xl">{t(msg`Deployment administrators`)}</h3>
            <p className="text-sm text-muted-foreground">
              {t(
                msg`These accounts manage the team and all countries. Change them in the deployment settings.`,
              )}
            </p>
            <ul className="text-sm space-y-2 break-all">
              {result.data.administrators.map((subject) => (
                <li key={subject}>{subject}</li>
              ))}
            </ul>
          </Card>
          <Collapsible>
            <CollapsibleTrigger render={<Button variant="outline" />}>
              {t(msg`Access history`)}
            </CollapsibleTrigger>
            <CollapsibleContent className="mt-4 space-y-4">
              {result.data.history.length ? (
                result.data.history.map((entry) => (
                  <Card key={entry.id} className="p-5 space-y-3">
                    <h3 className="text-lg">{entry.after.name}</h3>
                    <p className="text-xs break-all">
                      {t(msg`Changed by ${entry.actor}`)}
                    </p>
                    <time className="text-xs" dateTime={entry.at}>
                      {new Intl.DateTimeFormat(i18n.locale, {
                        dateStyle: "medium",
                        timeStyle: "short",
                      }).format(new Date(entry.at))}
                    </time>
                    <div className="grid gap-4 sm:grid-cols-2">
                      <div>
                        <h4 className="font-medium mb-2">
                          {t(msg`Previous access`)}
                        </h4>
                        {entry.before ? (
                          <>
                            <p>
                              {entry.before.active
                                ? t(msg`Active`)
                                : t(msg`Access suspended`)}
                            </p>
                            <AccessSummary member={entry.before} />
                          </>
                        ) : (
                          <p>{t(msg`New team member`)}</p>
                        )}
                      </div>
                      <div>
                        <h4 className="font-medium mb-2">
                          {t(msg`Saved access`)}
                        </h4>
                        <p>
                          {entry.after.active
                            ? t(msg`Active`)
                            : t(msg`Access suspended`)}
                        </p>
                        <AccessSummary member={entry.after} />
                      </div>
                    </div>
                  </Card>
                ))
              ) : (
                <p>{t(msg`No access changes yet`)}</p>
              )}
            </CollapsibleContent>
          </Collapsible>
        </>
      )}
      <Dialog
        open={editing !== null}
        onOpenChange={(open) => {
          if (!open) setEditing(null);
        }}
      >
        <DialogContent className="max-h-[90dvh] overflow-y-auto">
          <DialogTitle>
            {editing === "new"
              ? t(msg`Add team member`)
              : t(msg`Edit team access`)}
          </DialogTitle>
          <DialogDescription>
            {t(
              msg`Country roles control operational access. Editing the shared recipe library is a separate permission.`,
            )}
          </DialogDescription>
          {editing !== null && (
            <StaffEditor
              key={editing === "new" ? "new" : editing.id + editing.version}
              member={editing === "new" ? undefined : editing}
              onSaved={() => {
                setEditing(null);
                result.refresh();
              }}
            />
          )}
        </DialogContent>
      </Dialog>
    </div>
  );
}
function StaffEditor({
  member,
  onSaved,
}: {
  member?: StaffMember;
  onSaved: () => void;
}) {
  const { _: t } = useLingui();
  const labels = useRoleLabels();
  const [subject, setSubject] = useState(member?.subject ?? "");
  const [value, setValue] = useState<StaffSettings>(
    member ?? { name: "", active: true, recipeEditor: false, grants: [] },
  );
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const attempt = useRef<{ signature: string; requestKey: string } | null>(
    null,
  );
  return (
    <form
      className="space-y-5"
      onSubmit={async (event) => {
        event.preventDefault();
        if (busy) return;
        setBusy(true);
        setError("");
        const input = {
          ...value,
          subject,
          expectedVersion: member?.version ?? null,
        };
        const signature = JSON.stringify(input);
        if (attempt.current?.signature !== signature)
          attempt.current = { signature, requestKey: crypto.randomUUID() };
        try {
          await api.saveStaffMember({
            ...input,
            requestKey: attempt.current.requestKey,
          });
          onSaved();
        } catch (error) {
          setError(String((error as Error).message));
        } finally {
          setBusy(false);
        }
      }}
    >
      <FieldSet disabled={busy}>
        <Field label={t(msg`Colleague's name`)}>
          <Input
            required
            maxLength={100}
            value={value.name}
            onChange={(e) => setValue({ ...value, name: e.target.value })}
          />
        </Field>
        <Field label={t(msg`Account ID`)}>
          <Input
            required
            maxLength={100}
            readOnly={Boolean(member)}
            value={subject}
            onChange={(e) => setSubject(e.target.value)}
          />
        </Field>
        <Label className="mb-4">
          <Checkbox
            checked={value.active}
            disabled={busy}
            onCheckedChange={(active) => setValue({ ...value, active })}
          />
          {t(msg`Allow staff access`)}
        </Label>
        <p className="text-sm text-muted-foreground mb-4">
          {t(
            msg`Suspending staff access leaves the customer's orders and personal account available.`,
          )}
        </p>
        <Label>
          <Checkbox
            checked={value.recipeEditor}
            disabled={busy}
            onCheckedChange={(recipeEditor) =>
              setValue({ ...value, recipeEditor })
            }
          />
          {t(msg`Edit and approve the shared recipe library`)}
        </Label>
      </FieldSet>
      <p className="text-sm text-muted-foreground">
        {t(
          msg`Managers can issue refunds and manage all operations in their countries. Support colleagues can respond to requests; refunds require a manager.`,
        )}
      </p>
      {Object.entries(markets).map(([market, country]) => (
        <FieldSet key={market} className="rounded-lg border p-4">
          <FieldLegend>{t(country.name)}</FieldLegend>
          <div className="grid grid-cols-2 gap-4">
            {staffRoles.map((role) => (
              <Label key={role}>
                <Checkbox
                  disabled={busy}
                  checked={value.grants.some(
                    (grant) =>
                      grant.market === market && grant.roles.includes(role),
                  )}
                  onCheckedChange={(checked) => {
                    const current =
                      value.grants.find((grant) => grant.market === market)
                        ?.roles ?? [];
                    const roles = checked
                      ? [...current, role]
                      : current.filter((entry) => entry !== role);
                    setValue({
                      ...value,
                      grants: [
                        ...value.grants.filter(
                          (grant) => grant.market !== market,
                        ),
                        ...(roles.length
                          ? [{ market: market as keyof typeof markets, roles }]
                          : []),
                      ],
                    });
                  }}
                />
                {labels[role]}
              </Label>
            ))}
          </div>
        </FieldSet>
      ))}
      {error && (
        <p role="alert" className="notice">
          {t(error)}
        </p>
      )}
      <Button type="submit" disabled={busy}>
        {busy ? t(msg`Saving…`) : t(msg`Save access`)}
      </Button>
    </form>
  );
}
