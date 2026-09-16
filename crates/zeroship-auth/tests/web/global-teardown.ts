export default async function globalTeardown(): Promise<void> {
  const rawBaseURL = process.env.ZEROSHIP_AUTH_UI_BASE_URL;
  if (!rawBaseURL) {
    throw new Error("ZEROSHIP_AUTH_UI_BASE_URL disappeared before teardown");
  }

  // The shell EXIT trap owns process, database, and work-directory cleanup so
  // it also runs when Playwright setup itself fails. Here we only prove the
  // real service survived for the entire browser suite.
  const response = await fetch(new URL("/readyz", rawBaseURL), {
    signal: AbortSignal.timeout(5_000),
  });
  if (!response.ok) {
    throw new Error(`zeroship-auth was not ready at teardown: HTTP ${response.status}`);
  }
}
