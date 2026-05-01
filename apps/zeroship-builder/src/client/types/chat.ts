// Type contract for the AI-SDK-protocol custom data parts.
// Server's stream translator emits these as `data-part` chunks; client renders them.

import { z } from "zod";

// --- Survey (per design §8.2.7) ---

export const optionSchema = z.object({
  value: z.string(),
  label: z.string(),
  hint: z.string().optional(),
});
export type Option = z.infer<typeof optionSchema>;

export const questionKindSchema = z.discriminatedUnion("type", [
  z.object({ type: z.literal("single_choice"), options: z.array(optionSchema) }),
  z.object({
    type: z.literal("multi_choice"),
    options: z.array(optionSchema),
    min: z.number().int().min(0).optional(),
    max: z.number().int().min(1).optional(),
  }),
  z.object({
    type: z.literal("short_text"),
    placeholder: z.string().optional(),
    max_length: z.number().int().min(1).optional(),
  }),
  z.object({
    type: z.literal("long_text"),
    placeholder: z.string().optional(),
    max_length: z.number().int().min(1).optional(),
  }),
  z.object({ type: z.literal("yes_no") }),
  z.object({
    type: z.literal("scale"),
    min: z.number().int(),
    max: z.number().int(),
    labels: z.tuple([z.string(), z.string()]).optional(),
  }),
  z.object({
    type: z.literal("image_upload"),
    max_count: z.number().int().min(1).optional(),
    hint: z.string().optional(),
  }),
]);
export type QuestionKind = z.infer<typeof questionKindSchema>;

export const questionSchema = z.object({
  id: z.string().min(1),
  prompt: z.string().min(1),
  kind: questionKindSchema,
  default: z.unknown().optional(),
  required: z.boolean().optional(),
});
export type Question = z.infer<typeof questionSchema>;

export const surveySchema = z.object({
  preamble: z.string().optional(),
  questions: z.array(questionSchema).max(3),
  skip_label: z.string().optional(),
});
export type Survey = z.infer<typeof surveySchema>;

export type SurveyResponse = {
  survey_id: string;
  answers: Record<string, unknown>;
  skipped: boolean;
};

// --- Diff card ---

export const diffSchema = z.object({
  path: z.string(),
  before: z.string(),  // file content before
  after: z.string(),   // file content after
});
export type Diff = z.infer<typeof diffSchema>;

// --- Critic round indicator ---

export const criticRoundSchema = z.object({
  round: z.number().int().min(1),
  total: z.number().int().min(1),
  approved: z.boolean(),
  issues: z.array(z.object({
    dimension: z.string(),
    severity: z.enum(["low", "medium", "high", "critical"]),
    note: z.string(),
  })).default([]),
});
export type CriticRound = z.infer<typeof criticRoundSchema>;

// --- Wizard brief (terminal chunk from the wizard runtime) ---

// Mirrors the server-side `WizardBrief` in
// apps/zeroship-builder/src/server/_wizard.ts. Wizard-only — Builder
// doesn't emit data-brief; it consumes one as starting context for
// its first turn.
export const briefSchema = z.object({
  idea: z.string(),
  summary: z.string(),
  answers: z.array(
    z.object({
      question: z.string(),
      answer: z.unknown(),
    }),
  ),
});
export type Brief = z.infer<typeof briefSchema>;

// --- Custom data part union (what the translator emits) ---

export type CustomDataPart =
  | { kind: "survey";        payload: Survey }
  | { kind: "diff";          payload: Diff }
  | { kind: "critic-round";  payload: CriticRound }
  | { kind: "brief";         payload: Brief };
