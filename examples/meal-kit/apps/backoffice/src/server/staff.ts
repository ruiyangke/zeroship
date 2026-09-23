"use server";
import { env } from "zeroship";
import { z } from "zod";
import { query, mutation } from "@zeroship/rpc/server";
import { fail } from "@gather/meal-kit/domain";
import { must, transact, changed } from "@gather/meal-kit/server/core";
import { administratorIds, requireAdministrator } from "@gather/meal-kit/server/staff-access";
import {
  staffSettingsSchema,
  staffMemberSchema,
  saveStaffSchema,
  normalizedStaff,
} from "@gather/meal-kit/staff-domain";

type MemberRow = NonNullable<
  Awaited<ReturnType<typeof env.db.meal_staff_members.get>>["data"]
>;
const memberDto = (row: MemberRow) =>
  staffMemberSchema.parse({
    ...staffSettingsSchema.parse(row.settings),
    id: row.id,
    subject: row.subject,
    version: row.version,
  });

const staffTeamSchema = z.object({
  administrators: z.array(z.string()),
  members: z.array(staffMemberSchema),
  history: z.array(z.object({
    id: z.string(), actor: z.string(), subject: z.string(), at: z.string().datetime(),
    before: staffMemberSchema.nullable(), after: staffMemberSchema,
  })),
});

export const getStaffTeam = query(
  async () => {
    requireAdministrator();
    const members = must(
      await env.db.meal_staff_members.find({}).sort({ subject: 1 }),
    );
    const history = must(
      await env.db.meal_staff_events
        .find({})
        .sort({ created_at: -1 })
        .limit(100),
    );
    return {
      administrators: administratorIds(),
      members: members.map(memberDto),
      history: history.map((row) => ({
        id: row.id,
        actor: row.actor_id,
        subject: row.subject,
        at: new Date(row.created_at).toISOString(),
        before: z
          .object({ member: staffMemberSchema.nullable() })
          .parse(row.before).member,
        after: staffMemberSchema.parse(row.after),
      })),
    };
  },
  { id: "gather.staffTeam", output: staffTeamSchema },
);

export const saveStaffMember = mutation(
  async (raw: z.infer<typeof saveStaffSchema>) => {
    const actor = requireAdministrator();
    const input = normalizedStaff(raw);
    if (administratorIds().includes(input.subject))
      fail(
        /* i18n */ "Administrator access is managed in the deployment settings.",
        "ADMINISTRATOR_MANAGED",
        409,
      );
    return transact(async (tx) => {
      const event_key = `${actor.id}:${input.requestKey}`;
      const prior = await tx.meal_staff_events.get({ event_key });
      if (prior) {
        if (JSON.stringify(prior.command) !== JSON.stringify(input))
          fail(
            /* i18n */ "These details have changed. Refresh the page and try again.",
            "KEY_REUSED",
            409,
          );
        return staffMemberSchema.parse(prior.after);
      }
      const existing = await tx.meal_staff_members.get({
        subject: input.subject,
      });
      if ((existing?.version ?? null) !== input.expectedVersion)
        fail(
          /* i18n */ "This team member's access changed. Reload the team before saving.",
          "CONFLICT",
          409,
        );
      const settings = staffSettingsSchema.parse(input);
      const saved = existing
        ? changed(
            await tx.meal_staff_members.update(
              { id: existing.id, version: existing.version },
              { settings },
            ),
          )
        : await tx.meal_staff_members.insert({
            subject: input.subject,
            settings,
          });
      const after = memberDto(saved);
      await tx.meal_staff_events.insert({
        event_key,
        actor_id: actor.id,
        subject: input.subject,
        command: input,
        before: { member: existing ? memberDto(existing) : null },
        after,
      });
      return after;
    });
  },
  {
    id: "gather.saveStaffMember",
    input: saveStaffSchema,
    output: staffMemberSchema,
  },
);
