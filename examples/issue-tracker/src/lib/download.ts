import { getAttachment } from "../api";

/**
 * Fetch an attachment and hand it to the browser as a download.
 *
 * Shared because the same file is offered in two places -- the FILES panel and
 * the chip on the comment it arrived with -- and the second one had no way to
 * do this. It linked to `#/bugs/<id>?attachment=<fileId>`, which looks like a
 * deep link and is not one: the app routes on the hash and splits it on "/",
 * so the query rode along inside the bug id and produced a request for a bug
 * called "bug_0346...?attachment=atta_...". Nothing read the parameter either.
 *
 * There is no URL to link to -- attachments come back over RPC as base64, not
 * from a path -- so the honest affordance is an action, not an anchor.
 */
export async function downloadAttachment(id: string, filename: string): Promise<void> {
  const { contentBase64, contentType } = await getAttachment({ id });
  const bytes = Uint8Array.from(atob(contentBase64), (c) => c.charCodeAt(0));
  const blob = new Blob([bytes], { type: contentType });
  const url = URL.createObjectURL(blob);
  try {
    const link = document.createElement("a");
    link.href = url;
    link.download = filename;
    link.click();
  } finally {
    // Always, even if click() throws: the blob is held in memory until this
    // runs, and a thread with several files would leak one per download.
    URL.revokeObjectURL(url);
  }
}
