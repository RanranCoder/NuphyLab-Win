// Type surface for shared/appearance.mjs.
export type Appearance = 'light' | 'dark' | 'system';

export declare const APPEARANCES: readonly Appearance[];
export declare const DEFAULT_APPEARANCE: Appearance;

export declare function validateAppearance(
  value: unknown,
  previous?: Appearance,
): Appearance;

export declare function effectiveTheme(
  appearance: unknown,
  systemDark: boolean,
): 'light' | 'dark';
