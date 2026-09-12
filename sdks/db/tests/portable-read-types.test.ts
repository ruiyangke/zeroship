import { t } from "@zeroship/db";
import type { Collection } from "@zeroship/db";

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

async function portableReadTypes(): Promise<void> {
  const payloads = await records.distinct("payload");
  const _: Uint8Array[] | null = payloads.data;

  records.find().sort({ title: 1, score: -1 });
  // @ts-expect-error booleans have no portable sort order
  records.find().sort({ enabled: 1 });
  // @ts-expect-error JSON has no portable sort order
  records.find().sort({ document: 1 });
  // @ts-expect-error JSON has no portable distinct equality
  await records.distinct("document");
  // @ts-expect-error vectors use the search API
  await records.distinct("embedding");
  // @ts-expect-error encrypted values cannot participate in distinct queries
  await records.distinct("secret");
}

void portableReadTypes;
