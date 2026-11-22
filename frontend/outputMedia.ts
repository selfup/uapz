import type { FullOutput } from "./protocol";

/** Guess an image media type from a saved label when the payload omits one. */
export function guessImageMediaType(label: string | undefined): string {
  if (/jpe?g/i.test(label || "")) return "image/jpeg";
  if (/webp/i.test(label || "")) return "image/webp";
  return "image/png";
}

/** Whether a full output payload should render as an inline image. */
export function isImageOutput(
  full: Pick<FullOutput, "base64" | "media_type" | "label"> | null | undefined,
  mediaType: string,
): boolean {
  if (!full?.base64) return false;
  return (
    mediaType.startsWith("image/") &&
    !!(full.media_type || /image|png|jpe?g|webp/i.test(full.label))
  );
}

export function outputMediaType(
  full: Pick<FullOutput, "media_type" | "label"> | null | undefined,
): string {
  return full?.media_type || guessImageMediaType(full?.label);
}

export function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
