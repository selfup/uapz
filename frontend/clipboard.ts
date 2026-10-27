/** Copy text for the user, falling back to the legacy command where the async
 * clipboard is unavailable (for example in a webview without a secure context). */
export async function copyText(text: string): Promise<boolean> {
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch {
    // Fall through to the legacy path.
  }
  try {
    const area = document.createElement("textarea");
    area.value = text;
    area.setAttribute("readonly", "");
    area.style.position = "fixed";
    area.style.opacity = "0";
    document.body.append(area);
    area.select();
    const copied = document.execCommand("copy");
    area.remove();
    return copied;
  } catch {
    return false;
  }
}

/** The platform's name for showing a file in its file manager. */
export function revealLabel(): string {
  return /Mac/i.test(navigator.platform || navigator.userAgent)
    ? "Show in Finder"
    : "Show in folder";
}
