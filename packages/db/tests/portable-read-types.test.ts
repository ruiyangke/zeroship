import { t } from "../src/index.js";
import type { Collection, SortableField, SortInput } from "../src/index.js";

const fields = {
  title: t.string(),
  score: t.double(),
  enabled: t.boolean(),
  payload: t.bytes(),
  document: t.json(),
  embedding: t.vector(3),
  location: t.geoPoint(),
  secret: t.string().encrypted(),
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

  const selected = await records.find().select("title");
  const selectedTitle: string | undefined = selected.data?.[0]?.title;
  // @ts-expect-error selecting one field removes other fields from the result
  const selectedScore: number | undefined = selected.data?.[0]?.score;
  void [selectedTitle, selectedScore];
  records.find().select(["title", "score"] as const);
  records.find().select({ title: 1, score: true });
  // @ts-expect-error a projection must select at least one field
  records.find().select({});
  // @ts-expect-error string projection must name a declared field
  records.find().select("missing");
  // @ts-expect-error array projection must name declared fields
  records.find().select(["missing"]);
  // @ts-expect-error object projection must name declared fields
  records.find().select({ missing: 1 });
  // @ts-expect-error typed projections are inclusion-only
  records.find().select({ title: 0 });

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

  records.search({ vector: [1, 2, 3], column: "embedding" });
  // @ts-expect-error vector search columns must be vector fields
  records.search({ vector: [1, 2, 3], column: "title" });
  records.near({ field: "location", point: { lat: 0, lng: 0 }, radius: 1 });
  // @ts-expect-error spatial search fields must be geographic fields
  records.near({ field: "embedding", point: { lat: 0, lng: 0 }, radius: 1 });
}

void portableReadTypes;
