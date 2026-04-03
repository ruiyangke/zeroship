// Zod schema definitions — demonstrates npm package usage
import { z } from "zod";

export const UserSchema = z.object({
    name: z.string().min(1, "Name is required"),
    email: z.string().email("Invalid email format"),
    age: z.number().int().min(0).max(150).optional(),
});

export type User = z.infer<typeof UserSchema>;

export function validateUser(data: unknown): User {
    return UserSchema.parse(data);
}
