// Appearance preference shared by the server (settings validation) and the UI
// (effective theme calculation). The default is light: the Windows build does
// not follow the OS theme unless the user asks it to, so a dark desktop does
// not silently repaint the studio.
export const APPEARANCES = Object.freeze(['light', 'dark', 'system']);
export const DEFAULT_APPEARANCE = 'light';

export function validateAppearance(value, previous = DEFAULT_APPEARANCE) {
  if (value === undefined) return previous;
  if (!APPEARANCES.includes(value)) throw new Error('外观设置无效');
  return value;
}

/** Resolves the theme to apply. `systemDark` is only consulted for 'system';
 * anything unrecognised falls back to the light default, never to the OS. */
export function effectiveTheme(appearance, systemDark) {
  if (appearance === 'dark') return 'dark';
  if (appearance === 'system') return systemDark === true ? 'dark' : 'light';
  return 'light';
}
