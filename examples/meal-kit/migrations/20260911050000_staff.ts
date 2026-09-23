import { table, t } from "@zeroship/migrate";

export default {
  name: "staff_access",
  schema() {
    table("meal_staff_members").create({
      columns: {
        subject: t.text().required(),
        settings: t.json().required(),
      },
      indexes: [{ name: "meal_staff_subject", on: ["subject"], unique: true }],
    });
    table("meal_staff_events").create({
      columns: {
        event_key: t.text().required(),
        actor_id: t.text().required(),
        subject: t.text().required(),
        command: t.json().required(),
        before: t.json().required(),
        after: t.json().required(),
      },
      indexes: [
        { name: "meal_staff_event_key", on: ["event_key"], unique: true },
        { name: "meal_staff_event_subject", on: ["subject"] },
      ],
    });
  },
};
