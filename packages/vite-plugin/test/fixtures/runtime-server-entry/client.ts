// The browser half of the fixture app: its imports are what the build
// discovers the procedures from, as it would in a deployed app.
import { eager } from "./eager";
import { lazy, tokens } from "./lazy";

export const procedures = { eager, lazy, tokens };
