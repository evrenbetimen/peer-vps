/**
 * What `text` adds to `shown`, when both are bounded tails of the same growing log
 * (the console endpoint returns the last 64 KiB). Null when they no longer overlap.
 */
export function newSuffix(shown: string, text: string): string | null {
  if (text.startsWith(shown)) return text.slice(shown.length);
  // The window moved: find where the end of what is on screen sits in the new tail.
  const anchor = shown.slice(-256);
  const at = text.lastIndexOf(anchor);
  return at >= 0 ? text.slice(at + anchor.length) : null;
}
