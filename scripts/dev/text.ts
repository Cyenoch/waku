// One ANSI escape (CSI or OSC) or a single code point.
const TOKEN_PATTERN =
  /\x1b(?:\[[0-9;:?]*[A-Za-z]|\][^\x07]*(?:\x07|\x1b\\)?)|[\s\S]/gu;

function charWidth(codePoint: number): number {
  if (codePoint >= 0x0300 && codePoint <= 0x036f) return 0;
  if (
    (codePoint >= 0x1100 && codePoint <= 0x115f) ||
    (codePoint >= 0x2e80 && codePoint <= 0xa4cf && codePoint !== 0x303f) ||
    (codePoint >= 0xac00 && codePoint <= 0xd7a3) ||
    (codePoint >= 0xf900 && codePoint <= 0xfaff) ||
    (codePoint >= 0xfe30 && codePoint <= 0xfe6f) ||
    (codePoint >= 0xff00 && codePoint <= 0xff60) ||
    (codePoint >= 0xffe0 && codePoint <= 0xffe6) ||
    (codePoint >= 0x1f300 && codePoint <= 0x1f64f) ||
    (codePoint >= 0x1f900 && codePoint <= 0x1f9ff) ||
    (codePoint >= 0x20000 && codePoint <= 0x3fffd)
  ) {
    return 2;
  }
  return 1;
}

/** Hard-wrap one log line to `width` columns, keeping ANSI escapes intact. */
export function wrapLine(line: string, width: number): string[] {
  if (width <= 0) return [line];
  const wrapped: string[] = [];
  let current = "";
  let used = 0;
  for (const match of line.matchAll(TOKEN_PATTERN)) {
    const token = match[0]!;
    if (token.charCodeAt(0) === 0x1b) {
      current += token;
      continue;
    }
    const tokenWidth = charWidth(token.codePointAt(0)!);
    if (used + tokenWidth > width && current !== "") {
      wrapped.push(current);
      current = token;
      used = tokenWidth;
    } else {
      current += token;
      used += tokenWidth;
    }
  }
  wrapped.push(current);
  return wrapped;
}

export function wrapLines(lines: string[], width: number): string[] {
  return lines.flatMap((line) => wrapLine(line, width));
}
