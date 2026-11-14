// Type surface for shared/screen-ambient.mjs. The implementation stays plain
// JavaScript (it runs in the Node server too); the UI imports these types.
export type ScreenEffect = 'mirror' | 'average' | 'edge';

export type ScreenSettings = {
  effect: ScreenEffect;
  gain: number;
  smoothing: number;
};

export type ScreenZones = {
  rows: number;
  cols: number;
  zones: number[][];
};

export declare const SCREEN_ZONES: { readonly rows: number; readonly cols: number };
export declare const SCREEN_EFFECTS: ReadonlyArray<{
  id: ScreenEffect;
  label: string;
  description: string;
}>;
export declare const DEFAULT_SCREEN: Readonly<ScreenSettings>;

export declare function validateScreenSettings(
  input?: Partial<ScreenSettings>,
  previous?: ScreenSettings,
): ScreenSettings;
export declare function validateScreenFeature(value: unknown): ScreenZones;

export declare class ScreenAmbient {
  zones: number[][] | null;
  smoothed: number[][] | null;
  average: number[];
  sequence: number;
  reset(): void;
  feed(value: ScreenZones): void;
  zoneOf(key: { x: number; y: number; w: number }): number;
  render(config: Partial<ScreenSettings>, now: number, stale?: boolean): string[];
  summary(): { average: string; saturation: number };
}
