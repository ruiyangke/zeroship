import { t } from "@zeroship/db";
import type { Collection, SortableField, SortInput } from "@zeroship/db";

const fields = {
  title: t.string(),
  score: t.number(),
  enabled: t.boolean(),
  payload: t.bytes(),
  document: t.json(),
  embedding: t.vector(3),
  secret: t.encrypted(),
};

declare const records: Collection<typeof fields>;
const sortableField: SortableField<typeof fields> = "title";
// @ts-expect-error booleans are absent from the portable sortable field set
const unsortableField: SortableField<typeof fields> = "enabled";
void [sortableField, unsortableField];
// @ts-expect-error direct sort input requires a selected field
const emptySortInput: SortInput<typeof fields> = {};
// @ts-expect-error direct string sort input uses the sortable field set
const unsortableInput: SortInput<typeof fields> = "enabled";
void [emptySortInput, unsortableInput];

async function portableReadTypes(): Promise<void> {
  const payloads = await records.distinct("payload");
  const _: Uint8Array[] | null = payloads.data;

  records.find().sort({ title: 1, score: -1 });
  records.find().sort("title");
  records.find().sort("-score");
  // @ts-expect-error a sort object must select at least one field
  records.find().sort({});
  // @ts-expect-error booleans have no portable sort order
  records.find().sort({ enabled: 1 });
  // @ts-expect-error JSON has no portable sort order
  records.find().sort({ document: 1 });
  // @ts-expect-error string sort syntax uses the same portable field set
  records.find().sort("enabled");
  // @ts-expect-error JSON has no portable distinct equality
  await records.distinct("document");
  // @ts-expect-error vectors use the search API
  await records.distinct("embedding");
  // @ts-expect-error encrypted values cannot participate in distinct queries
  await records.distinct("secret");
}

void portableReadTypes;
