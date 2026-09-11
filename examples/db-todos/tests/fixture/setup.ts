import type { TestProject } from "vitest/node";
import { Platform } from "./platform";

export default async function setup(project: TestProject) {
  const platform = await Platform.create();
  const cancel = () => platform.processes.cancel();
  const removeCancel = project.vitest.onCancel(cancel);
  process.on("SIGINT", cancel);
  process.on("SIGTERM", cancel);
  const cleanup = async () => {
    try {
      await platform.close();
    } finally {
      removeCancel();
      process.off("SIGINT", cancel);
      process.off("SIGTERM", cancel);
    }
  };
  try {
    project.provide("databaseTargets", await platform.start());
    project.provide("databaseArtifacts", platform.logs);
  } catch (error) {
    try { await cleanup(); } catch (cleanupError) {
      throw new AggregateError([error, cleanupError], "Database setup and cleanup failed");
    }
    throw error;
  }
  return async () => {
    try {
      if (!platform.processes.signal.aborted) platform.processes.assertAlive();
    } finally {
      await cleanup();
    }
  };
}
