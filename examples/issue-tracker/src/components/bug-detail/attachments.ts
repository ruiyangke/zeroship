/**
 * Shared attachment helpers.
 *
 * Extracted from AttachmentsPanel when files became something you attach BY
 * commenting: two surfaces now upload, and a second copy of the size cap is
 * the kind of duplicate that stays right until the day someone changes one.
 */

/**
 * Mirrors the server's cap.
 *
 * Checked here as well so a too-large file is refused before it is read into
 * memory, base64'd and sent -- the server would reject it anyway, having been
 * handed a third of a megabyte to reach that conclusion.
 */
export const MAX_ATTACHMENT_BYTES = 512 * 1024;

export function fileToBase64(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onerror = () => reject(new Error(`Could not read ${file.name}`));
    reader.onload = () => {
      const result = String(reader.result ?? "");
      // A data: URL, not raw base64 -- the payload starts after the comma.
      resolve(result.slice(result.indexOf(",") + 1));
    };
    reader.readAsDataURL(file);
  });
}

export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}
