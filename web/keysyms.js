// DOM KeyboardEvent key/code -> X11 keysym tables (values from keysymdef.h and XF86keysym.h).

export const XK = Object.freeze({
  BackSpace: 0xff08,
  Tab: 0xff09,
  Return: 0xff0d,
  Escape: 0xff1b,
  Left: 0xff51,
  Up: 0xff52,
  Right: 0xff53,
  Down: 0xff54,
  Shift_L: 0xffe1,
  Control_L: 0xffe3,
  Alt_L: 0xffe9,
  Super_L: 0xffeb,
  Super_R: 0xffec,
  ISO_Level3_Shift: 0xfe03,
});

// Named KeyboardEvent.key values (UI Events KeyboardEvent key Values).
const NAMED = {
  Backspace: 0xff08, Tab: 0xff09, Enter: 0xff0d, Escape: 0xff1b, Delete: 0xffff,
  Home: 0xff50, ArrowLeft: 0xff51, ArrowUp: 0xff52, ArrowRight: 0xff53, ArrowDown: 0xff54,
  PageUp: 0xff55, PageDown: 0xff56, End: 0xff57, Insert: 0xff63,
  Pause: 0xff13, PrintScreen: 0xff61, ContextMenu: 0xff67, Help: 0xff6a, Clear: 0xff0b,
  Select: 0xff60, Execute: 0xff62, Cancel: 0xff69, Find: 0xff68, Undo: 0xff65, Redo: 0xff66,
  Again: 0xff66, Compose: 0xff20, AltGraph: 0xfe03,
  // Convert is 変換 (Henkan_Mode), which X keymaps bind and IMEs listen to; Kanji is another key.
  Convert: 0xff23, NonConvert: 0xff22, KanaMode: 0xff2d, KanjiMode: 0xff21, Romaji: 0xff24,
  Hiragana: 0xff25, Katakana: 0xff26, HiraganaKatakana: 0xff27, Zenkaku: 0xff28, Hankaku: 0xff29,
  ZenkakuHankaku: 0xff2a, Eisu: 0xff30, HangulMode: 0xff31, HanjaMode: 0xff34,
  AudioVolumeDown: 0x1008ff11, AudioVolumeMute: 0x1008ff12, AudioVolumeUp: 0x1008ff13,
  MediaPlayPause: 0x1008ff14, MediaStop: 0x1008ff15, MediaTrackPrevious: 0x1008ff16,
  MediaTrackNext: 0x1008ff17, BrowserHome: 0x1008ff18, LaunchMail: 0x1008ff19,
  BrowserSearch: 0x1008ff1b, BrowserBack: 0x1008ff26, BrowserForward: 0x1008ff27,
  BrowserStop: 0x1008ff28, BrowserRefresh: 0x1008ff29, BrowserFavorites: 0x1008ff30,
  Power: 0x1008ff2a, WakeUp: 0x1008ff2b, Eject: 0x1008ff2c, Sleep: 0x1008ff2f,
};
for (let i = 1; i <= 35; i++) NAMED['F' + i] = 0xffbe + i - 1;

// Modifier keys as [left, right]; the side comes from `code` (or `location` when code is empty).
// Meta is the Windows/Command key, which X desktops know as Super.
const MODIFIERS = {
  Shift: [0xffe1, 0xffe2],
  Control: [0xffe3, 0xffe4],
  Alt: [0xffe9, 0xffea],
  Meta: [0xffeb, 0xffec],
  OS: [0xffeb, 0xffec],
  Super: [0xffeb, 0xffec],
  Hyper: [0xffed, 0xffee],
};
const RIGHT_CODES = new Set(['ShiftRight', 'ControlRight', 'AltRight', 'MetaRight', 'OSRight']);

// US-layout characters by physical key, for shortcuts typed on layouts (or with macOS Option)
// that would otherwise produce non-Latin characters: Ctrl+С on a Russian layout must be Ctrl+c.
const US = {
  Backquote: '`', Minus: '-', Equal: '=', BracketLeft: '[', BracketRight: ']', Backslash: '\\',
  Semicolon: ';', Quote: "'", Comma: ',', Period: '.', Slash: '/', Space: ' ',
};
for (let i = 0; i < 26; i++) US['Key' + String.fromCharCode(65 + i)] = String.fromCharCode(97 + i);
for (let i = 0; i < 10; i++) US['Digit' + i] = String(i);

/** Unicode rule: Latin-1 keysyms equal the code point, everything else is 0x01000000 | cp. */
export function charKeysym(ch) {
  const cp = ch.codePointAt(0);
  return (cp >= 0x20 && cp <= 0x7e) || (cp >= 0xa0 && cp <= 0xff) ? cp : (0x01000000 | cp) >>> 0;
}

/** True when `key` is exactly one code point (a printable character, not a named key). */
export function isCharKey(key) {
  return key.length === 1 || (key.length === 2 && key.codePointAt(0) > 0xffff);
}

/** The keysym for a keydown/keyup event, or 0 when the key has none. */
export function keysymForKey(e) {
  const mod = MODIFIERS[e.key];
  if (mod) return RIGHT_CODES.has(e.code) || (!e.code && e.location === 2) ? mod[1] : mod[0];
  const named = NAMED[e.key];
  if (named) return named;
  return isCharKey(e.key) ? charKeysym(e.key) : 0;
}

/** The US-layout keysym of the physical key, or 0. */
export function usKeysym(code) {
  const ch = US[code];
  return ch ? ch.codePointAt(0) : 0;
}

export function isModifierKeysym(ks) {
  return (ks >= 0xffe1 && ks <= 0xffee) || ks === XK.ISO_Level3_Shift;
}
