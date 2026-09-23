// One projection of a stored review row, read by the customer who wrote it in
// the storefront and by the staff who read it in the back office.

import { env } from "zeroship";
import { feedbackSchema } from "../cooking-domain";

export type FeedbackRow = NonNullable<
  Awaited<ReturnType<typeof env.db.meal_recipe_feedback.get>>["data"]
>;

export const feedbackDto = (row: FeedbackRow) =>
  feedbackSchema.parse({
    id: row.id,
    version: row.version,
    rating: row.rating,
    cookAgain: row.cook_again ?? null,
    comment: row.comment,
    createdAt: new Date(row.created_at).toISOString(),
    updatedAt: new Date(row.updated_at).toISOString(),
  });
