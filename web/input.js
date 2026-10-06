// Mouse, keyboard, touch and soft-keyboard input, encoded as tilt protocol messages (brief 6.4, 6.5).
import { XK, charKeysym, isCharKey, isModifierKeysym, keysymForKey, usKeysym } from './keysyms.js';

const MOVE = 0x20;
const BUTTON = 0x21;
const WHEEL = 0x22;
const KEY = 0x30;
const TEXT = 0x31;
const RELEASE_ALL = 0x32;

const DRAG_INTERVAL_MS = 1000 / 240;
const WHEEL_LINE_PX = 40;
const WHEEL_NOTCH_PX = 50;
const TAP_MS = 250;
const TAP_PX = 10;
const LONG_PRESS_MS = 500;
// Direct mode presses the button for a one-finger drag only after this long, so that a second
// finger landing late still makes a pinch or a scroll rather than a remote drag.
const DRAG_DEFER_MS = 100;
const SCROLL_STEP_PX = 20;
const CLASSIFY_PX = 30;
const PINCH_RATIO = 0.1;
// Server cursor reports trail our own trackpad moves; only adopt them once the finger has rested.
const CURSOR_ADOPT_MS = 300;
// Windows sends a fake ControlLeft with the same timestamp just before AltRight when AltGr is pressed.
const ALTGR_WAIT_MS = 100;
// A lone Option tap reaches the remote as Alt this late at most (see loneOption).
const LONE_OPTION_MS = 1500;
// Held extra-strip keys repeat like keyboard keys.
const REPEAT_DELAY_MS = 500;
const REPEAT_MS = 50;
// The hidden textarea keeps padding before the caret for soft-keyboard backspaces to delete.
// U+200B is neither a word character nor a space (UAX #29), so suggestions and autocorrect never
// take it for part of the word being typed; '_' would be.
const PAD_CHAR = '\u200B';
const PAD = PAD_CHAR.repeat(32);
// The server types at most this much UTF-8 per TEXT message and drops the rest.
const TEXT_CHUNK_BYTES = 4096;

// MouseEvent.button -> X button number, and -> its bit in MouseEvent.buttons.
const X_BUTTON = [1, 2, 3, 8, 9];
const BUTTONS_BIT = [1, 4, 2, 8, 16];
// Cmd shortcuts Apple users expect to act as Ctrl on the remote (Microsoft Remote Desktop's set).
const CMD_AS_CTRL = new Set(['KeyC', 'KeyV', 'KeyX', 'KeyA', 'KeyZ', 'KeyF']);
const STICKY = { ctrl: XK.Control_L, alt: XK.Alt_L, super: XK.Super_L };

const APPLE = /Mac|iPhone|iPad|iPod/.test(navigator.platform);
// iPadOS reports a Mac platform; its touch points tell it apart.
const IOS = APPLE && navigator.maxTouchPoints > 1;
const WINDOWS = /^Win/.test(navigator.platform);
const FINE_POINTER = matchMedia('(pointer: fine)');

const clamp = (v, lo, hi) => Math.min(hi, Math.max(lo, v));

function message(type, len) {
  const b = new Uint8Array(len);
  b[0] = type;
  return [b, new DataView(b.buffer)];
}

/**
 * Wires input listeners to the viewer and returns the input controller.
 *
 * env: {
 *   viewport, canvas, textarea   elements: listener target, mapping rect, hidden keyboard field
 *   send(bytes) -> bool          sends one binary message if connected
 *   videoSize() -> [w, h]        current video size ([0, 0] before the first frame)
 *   touchMode() -> 'trackpad' | 'direct'
 *   cmdToCtrl() -> bool          Apple: map Cmd+C/V/X/A/Z/F to Ctrl
 *   optionAsAlt() -> bool        Apple: Option is plain Alt instead of typing characters
 *   view: { scale(), zoomAt(f, clientX, clientY), panBy(dx, dy), reveal(x, y) }  local zoom/pan
 *   onChange()                   sticky modifiers or the virtual cursor changed
 *   onPointerType(type)          the last pointer type used on the viewer
 * }
 */
export function createInput(env) {
  const { viewport, canvas, textarea: ta } = env;
  let enabled = false;
  let lastInputAt = 0;

  function send(bytes) {
    if (enabled && env.send(bytes)) lastInputAt = performance.now();
  }

  // ---- coordinates

  function norm(clientX, clientY) {
    const r = canvas.getBoundingClientRect();
    if (!r.width || !r.height) return null;
    return [
      Math.round(clamp((clientX - r.left) / r.width, 0, 1) * 65535),
      Math.round(clamp((clientY - r.top) / r.height, 0, 1) * 65535),
    ];
  }

  // Inside the video and inside the viewport's client area (not on its scrollbars in 1:1 mode).
  function overCanvas(clientX, clientY) {
    const r = canvas.getBoundingClientRect();
    const v = viewport.getBoundingClientRect();
    return clientX >= r.left && clientX < r.right && clientY >= r.top && clientY < r.bottom
      && clientX < v.left + viewport.clientLeft + viewport.clientWidth
      && clientY < v.top + viewport.clientTop + viewport.clientHeight;
  }

  // Clamped like norm(): a cursor position from before the screen shrank must not wrap around.
  function fromVideo([x, y]) {
    const [w, h] = env.videoSize();
    if (!w || !h) return null;
    return [
      Math.round(clamp(x / Math.max(1, w - 1), 0, 1) * 65535),
      Math.round(clamp(y / Math.max(1, h - 1), 0, 1) * 65535),
    ];
  }

  // ---- pointer messages

  let sentPos = null;
  let pendingPos = null;
  let moveRaf = 0;
  let lastDragAt = -Infinity;
  const held = new Set();

  function moveTo(p) {
    if (!p || (sentPos && sentPos[0] === p[0] && sentPos[1] === p[1])) return;
    sentPos = p;
    const [b, d] = message(MOVE, 5);
    d.setUint16(1, p[0], true);
    d.setUint16(3, p[1], true);
    send(b);
  }

  function flushMove() {
    if (moveRaf) cancelAnimationFrame(moveRaf);
    moveRaf = 0;
    const p = pendingPos;
    pendingPos = null;
    moveTo(p);
  }

  function queueMove(p) {
    pendingPos = p;
    if (!moveRaf) moveRaf = requestAnimationFrame(() => { moveRaf = 0; flushMove(); });
  }

  // Drags send every coalesced sample, at most one per DRAG_INTERVAL_MS; the rest wait for rAF.
  function dragTo(e) {
    const samples = e.getCoalescedEvents ? e.getCoalescedEvents() : [];
    for (const s of samples.length ? samples : [e]) {
      const p = norm(s.clientX, s.clientY);
      if (s.timeStamp - lastDragAt >= DRAG_INTERVAL_MS) {
        pendingPos = null;
        moveTo(p);
        lastDragAt = s.timeStamp;
      } else {
        pendingPos = p;
      }
    }
    if (pendingPos) queueMove(pendingPos);
  }

  function button(b, down, p) {
    flushMove();
    pressFakeCtrl();
    sendLoneOption();
    if (!p) return;
    if (down) {
      pointerChord(true);
      held.add(b);
    } else {
      held.delete(b);
    }
    sentPos = p;
    const [m, d] = message(BUTTON, 7);
    m[1] = b;
    m[2] = down ? 1 : 0;
    d.setUint16(3, p[0], true);
    d.setUint16(5, p[1], true);
    send(m);
    if (!down) releaseOneShot();
  }

  function releaseButtons() {
    for (const b of [...held]) button(b, false, sentPos);
  }

  function wheel(dx, dy, p) {
    flushMove();
    pressFakeCtrl();
    sendLoneOption();
    if (!p) return;
    pointerChord(false);
    sentPos = p;
    const [m, d] = message(WHEEL, 9);
    d.setInt16(1, clamp(dx, -32768, 32767), true);
    d.setInt16(3, clamp(dy, -32768, 32767), true);
    d.setUint16(5, p[0], true);
    d.setUint16(7, p[1], true);
    send(m);
  }

  // ---- mouse

  function mouseDown(e) {
    if (!enabled) return;
    const b = X_BUTTON[e.button];
    // A press in the letterbox around the video is not a click on the remote edge.
    if (!b || (!held.size && !overCanvas(e.clientX, e.clientY))) return;
    e.preventDefault();
    if (FINE_POINTER.matches) focusKeyboard();
    try { viewport.setPointerCapture(e.pointerId); } catch { /* synthetic or already released */ }
    button(b, true, norm(e.clientX, e.clientY));
  }

  function mouseMove(e) {
    if (!enabled) return;
    // Pressing or releasing a second button while another is down arrives as pointermove.
    if (e.button >= 0 && X_BUTTON[e.button]) {
      const b = X_BUTTON[e.button];
      const down = (e.buttons & BUTTONS_BIT[e.button]) !== 0;
      if (down !== held.has(b)) button(b, down, norm(e.clientX, e.clientY));
      return;
    }
    if (held.size) dragTo(e);
    else if (overCanvas(e.clientX, e.clientY)) queueMove(norm(e.clientX, e.clientY));
  }

  function mouseUp(e) {
    const b = X_BUTTON[e.button];
    if (!held.has(b)) return;
    e.preventDefault();
    button(b, false, norm(e.clientX, e.clientY));
  }

  let wheelX = 0;
  let wheelY = 0;

  function onWheel(e) {
    if (!enabled) return;
    e.preventDefault();
    const k = e.deltaMode === 1 ? WHEEL_LINE_PX : e.deltaMode === 2 ? viewport.clientHeight : 1;
    const dx = e.deltaX * k;
    const dy = e.deltaY * k;
    // Keep the remainder between events, but not across a change of direction.
    if (dx && Math.sign(dx) !== Math.sign(wheelX)) wheelX = 0;
    if (dy && Math.sign(dy) !== Math.sign(wheelY)) wheelY = 0;
    wheelX += dx;
    wheelY += dy;
    const nx = Math.trunc(wheelX / WHEEL_NOTCH_PX);
    const ny = Math.trunc(wheelY / WHEEL_NOTCH_PX);
    if (!nx && !ny) return;
    wheelX -= nx * WHEEL_NOTCH_PX;
    wheelY -= ny * WHEEL_NOTCH_PX;
    wheel(nx, ny, norm(e.clientX, e.clientY));
  }

  // ---- keyboard

  const down = new Map(); // key id (code, or key where code is empty) -> keysym sent on keydown
  const underCmd = new Map(); // keys pressed while Cmd was held: macOS never sends their keyup
  const underCtrl = new Set(); // iOS: ids in `down` pressed while Ctrl was held (see onKeyUp)
  let cmd = null; // Apple Cmd held: { id, ks, superSent, ctrlSent, used }
  let opt = null; // Apple Option held back: { id, ks, sent, used }
  let loneOpt = null; // a lone Option tap not sent yet: { ks, timer }
  let fakeCtrl = null; // Windows ControlLeft held back until we know it is not AltGr's fake Ctrl
  const sticky = { ctrl: 0, alt: 0, super: 0 }; // 0 off, 1 next key only, 2 locked
  const stripKeys = new Map(); // extra-strip keysym held down -> its repeat timer
  const keysDown = new Set(); // keysyms sent down and not yet up, whatever sent them

  function key(ks, isDown) {
    flushMove();
    if (isDown) keysDown.add(ks);
    else keysDown.delete(ks);
    const [m, d] = message(KEY, 6);
    m[1] = isDown ? 1 : 0;
    d.setUint32(2, ks, true);
    send(m);
  }

  function tap(ks) {
    key(ks, true);
    key(ks, false);
  }

  function setSticky(name, level) {
    const was = sticky[name];
    if (was === level) return;
    sticky[name] = level;
    if (!was) key(STICKY[name], true);
    else if (!level) key(STICKY[name], false);
    env.onChange();
  }

  function releaseOneShot() {
    for (const name in sticky) if (sticky[name] === 1) setSticky(name, 0);
  }

  function pressFakeCtrl() {
    if (!fakeCtrl) return;
    clearTimeout(fakeCtrl.timer);
    fakeCtrl = null;
    down.set('ControlLeft', XK.Control_L);
    key(XK.Control_L, true);
  }

  // A click or wheel turn under a held-back modifier decides it. Cmd+click is Ctrl+click when Cmd
  // acts as Ctrl (new tab, multi-select), Cmd+wheel a plain wheel (Ctrl+wheel would zoom), and
  // Option a plain Alt. Either way a lone Cmd or Option tap is no longer due on release.
  function pointerChord(click) {
    if (cmd) {
      cmd.used = true;
      if (click && !cmd.superSent && !cmd.ctrlSent) {
        key(XK.Control_L, true);
        cmd.ctrlSent = true;
      }
    }
    if (opt) optionAlt();
  }

  function ours(t) {
    return t === ta || t === document.body || t === document.documentElement || viewport.contains(t);
  }

  function keysymFor(e) {
    let ks = keysymForKey(e);
    const shortcut = (e.ctrlKey || e.altKey || e.metaKey) && !e.getModifierState('AltGraph');
    if (!ks || !shortcut || !isCharKey(e.key)) return ks;
    // Shortcuts use the Latin key: Ctrl+С on a Russian layout is Ctrl+c, and Option as Alt on a
    // Mac makes Option+c Alt+c rather than Alt+ç.
    const us = usKeysym(e.code);
    if (us && ks > 0x7e) ks = us;
    // Letter case must agree with Shift, or the server fakes a Shift change around the key.
    if (ks >= 0x61 && ks <= 0x7a && e.shiftKey) ks -= 0x20;
    else if (ks >= 0x41 && ks <= 0x5a && !e.shiftKey) ks += 0x20;
    return ks;
  }

  function releaseUnderCmd() {
    for (const ks of underCmd.values()) key(ks, false);
    underCmd.clear();
  }

  // iPadOS now and then sends no keyup for a key pressed under Ctrl (Ctrl+Z): it goes up with Ctrl.
  function releaseUnderCtrl() {
    if (!underCtrl.size) return;
    for (const id of underCtrl) {
      key(down.get(id), false);
      down.delete(id);
    }
    underCtrl.clear();
    releaseOneShot();
  }

  function cmdKeyDown(e, id, ks) {
    // The layout's letter decides (QWERTZ Cmd+Z is undo though its Z sits on the US Y key); keys
    // that type no Latin letter (Cyrillic, Greek layouts) fall back to their US position.
    const k = e.key.length === 1 ? e.key.toLowerCase() : '';
    const asCtrl = env.cmdToCtrl() && (k >= 'a' && k <= 'z' ? 'cvxazf'.includes(k) : CMD_AS_CTRL.has(e.code));
    if (!underCmd.has(id)) {
      if (asCtrl && !cmd.ctrlSent) {
        releaseUnderCmd();
        if (cmd.superSent) key(cmd.ks, false);
        cmd.superSent = false;
        key(XK.Control_L, true);
        cmd.ctrlSent = true;
      } else if (!asCtrl && !cmd.superSent) {
        releaseUnderCmd();
        if (cmd.ctrlSent) key(XK.Control_L, false);
        cmd.ctrlSent = false;
        key(cmd.ks, true);
        cmd.superSent = true;
      }
    }
    underCmd.set(id, ks);
    key(ks, true);
  }

  function cmdKeyUp() {
    const lone = !cmd.superSent && !cmd.ctrlSent && !cmd.used;
    releaseUnderCmd();
    if (cmd.ctrlSent) key(XK.Control_L, false);
    if (cmd.superSent) key(cmd.ks, false);
    // With Cmd->Ctrl on, Super is only pressed once we know Cmd is not a shortcut: a lone tap.
    if (lone) tap(cmd.ks);
    cmd = null;
  }

  // Option means Alt for this key or click: press Alt if it is not down yet.
  function optionAlt() {
    opt.used = true;
    if (!opt.sent) {
      key(opt.ks, true);
      opt.sent = true;
    }
  }

  // Option typed a character (or started a dead-key sequence): it was not Alt for this key.
  function optionTyped() {
    opt.used = true;
    if (opt.sent) {
      key(opt.ks, false);
      opt.sent = false;
    }
  }

  // iOS can type a dead key (Option+E) with no composition at all, or start its composition only
  // after Option is up: nothing shows that Option typed until the composed character (é) or the
  // composition comes. So there a lone Option tap (Alt: menus in many X apps) waits for the next
  // key or click, or LONE_OPTION_MS.
  function loneOption(ks) {
    loneOpt = { ks, timer: setTimeout(sendLoneOption, LONE_OPTION_MS) };
  }

  function sendLoneOption() {
    if (!loneOpt) return;
    const { ks } = loneOpt;
    dropLoneOption();
    tap(ks);
  }

  function dropLoneOption() {
    if (loneOpt) clearTimeout(loneOpt.timer);
    loneOpt = null;
  }

  // macOS Option composes characters, and on many layouts it is the only way to type @ [ ] { } |
  // \ ~. A key whose character differs from the key's own (US) character was composed: send that
  // character with no Alt held. Other keys (arrows, Backspace, chords with Ctrl or Cmd) get Alt.
  function optionComposed(e) {
    if (e.ctrlKey || cmd || !isCharKey(e.key)) return false;
    const us = usKeysym(e.code);
    return !us || e.key.toLowerCase() !== String.fromCodePoint(us);
  }

  function onKeyDown(e) {
    if (!enabled || !ours(e.target)) return;
    if (e.isComposing || e.keyCode === 229 || !e.key || e.key === 'Dead' || e.key === 'Process' || e.key === 'Unidentified') {
      if (opt) optionTyped();
      return;
    }
    const id = e.code || e.key;
    if (fakeCtrl) {
      if (e.code === 'AltRight' && Math.abs(e.timeStamp - fakeCtrl.ts) < 1) {
        clearTimeout(fakeCtrl.timer);
        fakeCtrl = null;
      } else {
        pressFakeCtrl();
      }
    }
    let ks = underCmd.get(id) ?? down.get(id) ?? keysymFor(e);
    if (!ks) return;
    e.preventDefault();
    // Shift does not decide: it comes first for a capital (Option+E, Shift+E types É).
    if (loneOpt && e.key !== 'Shift') {
      if (optionComposed(e)) {
        // The dead key's character: Option typed it.
        dropLoneOption();
        deadEnd = true;
      } else {
        sendLoneOption();
      }
    }
    // Not for repeats only: Windows repeats AltGr's fake Ctrl with every autorepeat of AltGr.
    if (WINDOWS && e.code === 'ControlLeft' && !down.has(id)) {
      fakeCtrl = { ts: e.timeStamp, timer: setTimeout(pressFakeCtrl, ALTGR_WAIT_MS) };
      return;
    }
    if (APPLE && (e.key === 'Meta' || e.key === 'OS')) {
      if (!cmd) {
        cmd = { id, ks, superSent: !env.cmdToCtrl(), ctrlSent: false, used: false };
        if (cmd.superSent) key(ks, true);
      }
      return;
    }
    if (APPLE && e.key === 'Alt' && !env.optionAsAlt()) {
      // Held back like Cmd until the next key shows whether it types a character or means Alt.
      if (!opt) opt = { id, ks, sent: false, used: false };
      return;
    }
    if (opt && !isModifierKeysym(ks) && !down.has(id) && !underCmd.has(id)) {
      if (optionComposed(e)) {
        optionTyped();
        ks = keysymForKey(e);
      } else {
        optionAlt();
      }
    }
    if (deadEnd && !isModifierKeysym(ks)) {
      deadEnd = false;
      // iOS sends a dead key's character as a plain keydown (é) after ending its composition empty
      // or with none at all, and may never send its keyup: tap it.
      if (!cmd && isCharKey(e.key) && !down.has(id)) {
        tap(ks);
        releaseOneShot();
        return;
      }
    }
    if (cmd && !isModifierKeysym(ks)) {
      cmdKeyDown(e, id, ks);
      return;
    }
    down.set(id, ks);
    if (IOS && e.ctrlKey && !isModifierKeysym(ks)) underCtrl.add(id);
    key(ks, true);
  }

  function onKeyUp(e) {
    const id = e.code || e.key;
    // A key pressed in the viewer is released wherever focus went meanwhile (the Type text box,
    // the menu): dropping its keyup would leave it held at the server.
    const tracked = (cmd && id === cmd.id) || (opt && id === opt.id) || underCmd.has(id) || down.has(id)
      || (fakeCtrl && id === 'ControlLeft');
    if (!tracked && !ours(e.target)) return;
    pressFakeCtrl();
    if (cmd && id === cmd.id) {
      e.preventDefault();
      cmdKeyUp();
      return;
    }
    if (opt && id === opt.id) {
      e.preventDefault();
      if (opt.sent) key(opt.ks, false);
      else if (!opt.used && IOS) loneOption(opt.ks);
      else if (!opt.used) tap(opt.ks);
      opt = null;
      return;
    }
    let ks = underCmd.get(id);
    if (ks !== undefined) {
      e.preventDefault();
      underCmd.delete(id);
      key(ks, false);
      return;
    }
    ks = down.get(id);
    if (ks === undefined) return;
    e.preventDefault();
    if (e.key === 'Control') releaseUnderCtrl();
    down.delete(id);
    underCtrl.delete(id);
    key(ks, false);
    if (!isModifierKeysym(ks)) releaseOneShot();
  }

  /** Extra-strip keys: down on press and, held, autorepeat (a repeated KEY down) like a keyboard. */
  function holdKey(ks) {
    if (!enabled || stripKeys.has(ks)) return;
    // As on a keyboard, only the newest held key repeats.
    for (const [k, timer] of stripKeys) {
      clearTimeout(timer);
      stripKeys.set(k, 0);
    }
    key(ks, true);
    const again = () => {
      key(ks, true);
      stripKeys.set(ks, setTimeout(again, REPEAT_MS));
    };
    stripKeys.set(ks, setTimeout(again, REPEAT_DELAY_MS));
  }

  function releaseKey(ks) {
    if (!stripKeys.has(ks)) return;
    clearTimeout(stripKeys.get(ks));
    stripKeys.delete(ks);
    key(ks, false);
    releaseOneShot();
  }

  // ---- hidden textarea: soft keyboards, dead keys and IME arrive as edits, diffed by code point

  let synced = '';
  let composing = false;
  let deadEnd = false; // a dead key's character comes next (see onKeyDown)

  function caretToEnd() {
    const n = ta.value.length;
    try { ta.setSelectionRange(n, n); } catch { /* not focusable yet */ }
  }

  function resetPad() {
    ta.value = PAD;
    synced = PAD;
    caretToEnd();
  }

  function diffText() {
    const value = ta.value;
    if (value !== synced) {
      const a = Array.from(synced);
      const b = Array.from(value);
      let i = 0;
      while (i < a.length && i < b.length && a[i] === b[i]) i++;
      synced = value;
      // The edit happened at the caret, which is kept at the end (so no common-suffix diff: the
      // remote caret is at the end too). An edit that also inserts (autocorrect, a suggestion)
      // replaces typed text only: padding it swallowed never reached the server. A plain deletion
      // is the user's backspace, padding included.
      const removed = a.slice(i);
      const inserted = b.slice(i).join('');
      const backspaces = inserted ? removed.filter((ch) => ch !== PAD_CHAR).length : removed.length;
      for (let n = backspaces; n > 0; n--) tap(XK.BackSpace);
      if (inserted) typed(inserted);
    }
    if (value.length < PAD.length / 2 || value.length > PAD.length * 2) resetPad();
    else caretToEnd();
  }

  // Soft-keyboard text: with a sticky modifier on, a single character is a shortcut, not text.
  function typed(text) {
    const chars = Array.from(text);
    if (chars.length === 1 && (sticky.ctrl || sticky.alt || sticky.super)) {
      const ch = chars[0];
      tap(ch === '\n' ? XK.Return : ch === '\t' ? XK.Tab : charKeysym(ch));
      releaseOneShot();
      return;
    }
    sendLiteral(text);
  }

  /**
   * TEXT types literal text, so sticky modifiers are lifted around it (the server would type it
   * as Ctrl+h, Ctrl+e, ...): locked ones go down again afterwards, one-shot ones are used up.
   */
  function sendLiteral(text) {
    const active = Object.keys(sticky).filter((name) => sticky[name]);
    for (const name of active) key(STICKY[name], false);
    sendText(text);
    for (const name of active) {
      if (sticky[name] === 2) key(STICKY[name], true);
      else sticky[name] = 0;
    }
    if (active.length) env.onChange();
  }

  const encoder = new TextEncoder();

  function sendText(text) {
    flushMove();
    let chunk = '';
    let bytes = 0;
    const flush = () => {
      if (!chunk) return;
      const utf8 = encoder.encode(chunk);
      const m = new Uint8Array(1 + utf8.length);
      m[0] = TEXT;
      m.set(utf8, 1);
      send(m);
      chunk = '';
      bytes = 0;
    };
    for (const ch of text) {
      const cp = ch.codePointAt(0);
      const n = cp < 0x80 ? 1 : cp < 0x800 ? 2 : cp < 0x10000 ? 3 : 4;
      if (bytes + n > TEXT_CHUNK_BYTES) flush();
      chunk += ch;
      bytes += n;
    }
    flush();
  }

  function onInput(e) {
    if (!enabled) {
      resetPad();
      return;
    }
    if (composing || e.isComposing) return;
    diffText();
  }

  function focusKeyboard() {
    if (document.activeElement === ta) return;
    resetPad();
    ta.focus({ preventScroll: true });
    caretToEnd();
  }

  // ---- touch

  const touches = new Map(); // pointerId -> { x, y, x0, y0 }
  let g = null; // the gesture in progress
  let tapHold = 0; // trackpad: button 1 is held after a tap until this timer fires or a touch follows
  let vc = null; // trackpad virtual cursor [x, y] in video pixels
  let serverPos = null;
  let localMoveAt = -Infinity;

  const trackpad = () => env.touchMode() === 'trackpad';

  // Where trackpad clicks land: the virtual cursor, else the remote pointer, else the centre.
  function cursorAt() {
    const [w, h] = env.videoSize();
    return vc || serverPos || [w / 2, h / 2];
  }
  const cursorNorm = () => fromVideo(cursorAt());

  function moveCursor(dx, dy) {
    const [w, h] = env.videoSize();
    if (!w || !h) return;
    const from = cursorAt();
    vc = [clamp(from[0] + dx, 0, w - 1), clamp(from[1] + dy, 0, h - 1)];
    localMoveAt = performance.now();
    moveTo(fromVideo(vc));
    env.view.reveal(vc[0], vc[1]);
    env.onChange();
  }

  function touchDown(e) {
    e.preventDefault();
    touches.set(e.pointerId, { x: e.clientX, y: e.clientY, x0: e.clientX, y0: e.clientY });
    if (touches.size === 1) startOne(e);
    else if (touches.size === 2 && g && g.kind === 'one' && !g.dragging) startTwo(e);
  }

  function startOne(e) {
    g = { kind: 'one', id: e.pointerId, t0: e.timeStamp, x0: e.clientX, y0: e.clientY, lt: e.timeStamp, moved: false, dragging: false, afterTap: false, longPressed: false, outside: false, timer: 0 };
    if (!enabled) return;
    if (trackpad()) {
      if (tapHold) {
        // Tap, then touch again: button 1 stays down so this touch can drag (or double-click).
        clearTimeout(tapHold);
        tapHold = 0;
        g.afterTap = true;
      }
    } else if (!overCanvas(e.clientX, e.clientY)) {
      // As with the mouse, a touch in the letterbox around the video is not a touch on the remote
      // screen's edge (a panel full of buttons): it only pans the local view.
      g.outside = true;
    } else {
      g.timer = setTimeout(longPress, LONG_PRESS_MS);
    }
  }

  function longPress() {
    if (!g || g.kind !== 'one' || g.moved || !enabled) return;
    g.longPressed = true;
    const p = norm(g.x0, g.y0);
    button(3, true, p);
    button(3, false, p);
  }

  function startTwo(e) {
    clearTimeout(g.timer);
    if (g.afterTap) button(1, false, cursorNorm());
    const [a, b] = [...touches.values()];
    const d0 = Math.max(1, Math.hypot(b.x - a.x, b.y - a.y));
    const m0 = { x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 };
    // Direct mode, both fingers in the letterbox: no right-click at the edge, and scrolling goes
    // to the remote pointer.
    const outside = !trackpad() && !overCanvas(a.x, a.y) && !overCanvas(b.x, b.y);
    g = { kind: 'two', t0: g.t0, d0, m0, ld: d0, lm: m0, mode: null, travel: 0, sx: 0, sy: 0, outside };
  }

  function touchMove(e) {
    const t = touches.get(e.pointerId);
    if (!t) return;
    e.preventDefault();
    const dx = e.clientX - t.x;
    const dy = e.clientY - t.y;
    t.x = e.clientX;
    t.y = e.clientY;
    if (!g) return;
    if (g.kind === 'one' && e.pointerId === g.id) oneMove(e, dx, dy);
    else if (g.kind === 'two') twoMove();
  }

  function oneMove(e, dx, dy) {
    if (Math.hypot(e.clientX - g.x0, e.clientY - g.y0) > TAP_PX) g.moved = true;
    if (!enabled || g.outside) {
      if (g.moved) env.view.panBy(dx, dy);
    } else if (trackpad()) {
      // Guacamole's pointer acceleration: delta * (1 + velocity in px/ms), in screen space.
      const k = (1 + Math.hypot(dx, dy) / Math.max(1, e.timeStamp - g.lt)) / env.view.scale();
      moveCursor(dx * k, dy * k);
    } else if (g.moved && !g.longPressed && (g.dragging || e.timeStamp - g.t0 >= DRAG_DEFER_MS)) {
      if (!g.dragging) {
        clearTimeout(g.timer);
        g.dragging = true;
        button(1, true, norm(g.x0, g.y0));
      }
      dragTo(e);
    }
    g.lt = e.timeStamp;
  }

  function twoMove() {
    const [a, b] = [...touches.values()];
    const d = Math.max(1, Math.hypot(b.x - a.x, b.y - a.y));
    const m = { x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 };
    if (!g.mode) {
      g.travel = Math.max(g.travel, Math.hypot(a.x - a.x0, a.y - a.y0), Math.hypot(b.x - b.x0, b.y - b.y0));
      if (g.travel < CLASSIFY_PX) return;
      // The first CLASSIFY_PX of movement decide; both modes then apply that movement too.
      g.mode = Math.abs(d / g.d0 - 1) > PINCH_RATIO ? 'pinch' : 'scroll';
      g.ld = g.d0;
      g.lm = g.m0;
    }
    if (g.mode === 'pinch' || !enabled) {
      env.view.panBy(m.x - g.lm.x, m.y - g.lm.y);
      env.view.zoomAt(d / g.ld, m.x, m.y);
    } else {
      g.sx += m.x - g.lm.x;
      g.sy += m.y - g.lm.y;
      const nx = Math.trunc(g.sx / SCROLL_STEP_PX);
      const ny = Math.trunc(g.sy / SCROLL_STEP_PX);
      if (nx || ny) {
        g.sx -= nx * SCROLL_STEP_PX;
        g.sy -= ny * SCROLL_STEP_PX;
        // Content follows the fingers: moving them up scrolls down (+dy).
        wheel(-nx, -ny, trackpad() || g.outside ? cursorNorm() : norm(m.x, m.y));
      }
    }
    g.ld = d;
    g.lm = m;
  }

  function touchUp(e) {
    if (!touches.has(e.pointerId)) return;
    e.preventDefault();
    touches.delete(e.pointerId);
    if (g && g.kind === 'one' && e.pointerId === g.id) oneUp(e);
    else if (g && g.kind === 'two') twoUp(e);
    if (!touches.size) g = null;
  }

  function oneUp(e) {
    clearTimeout(g.timer);
    const quick = !g.moved && e.timeStamp - g.t0 <= TAP_MS;
    g.kind = 'done';
    if (!enabled || g.outside) return;
    if (trackpad()) {
      if (g.afterTap) button(1, false, cursorNorm());
      if (quick) {
        const at = cursorNorm();
        button(1, true, at);
        tapHold = setTimeout(() => {
          tapHold = 0;
          button(1, false, at);
        }, TAP_MS);
      }
    } else if (g.dragging) {
      button(1, false, norm(e.clientX, e.clientY));
    } else if (g.moved && !g.longPressed) {
      // A drag released within DRAG_DEFER_MS: play it now.
      const end = norm(e.clientX, e.clientY);
      button(1, true, norm(g.x0, g.y0));
      moveTo(end);
      button(1, false, end);
    } else if (!g.longPressed) {
      const p = norm(g.x0, g.y0);
      button(1, true, p);
      button(1, false, p);
    }
  }

  function twoUp(e) {
    const { mode, m0, outside } = g;
    const tapped = !mode && !outside && g.travel < TAP_PX && e.timeStamp - g.t0 <= TAP_MS;
    g = { kind: 'done' };
    if (!enabled) return;
    if (mode === 'pinch' && trackpad() && vc) env.view.reveal(vc[0], vc[1]);
    if (!tapped) return;
    const at = trackpad() ? cursorNorm() : norm(m0.x, m0.y);
    button(3, true, at);
    button(3, false, at);
  }

  function touchCancel(e) {
    touches.delete(e.pointerId);
    if (g && (g.dragging || g.afterTap)) button(1, false, sentPos);
    if (g) clearTimeout(g.timer);
    g = touches.size ? { kind: 'done' } : null;
  }

  // ---- listeners

  let lastPointerType = '';

  function pointerType(e) {
    if (e.pointerType !== lastPointerType) {
      lastPointerType = e.pointerType;
      env.onPointerType(e.pointerType);
    }
    return e.pointerType === 'mouse';
  }

  viewport.addEventListener('pointerdown', (e) => (pointerType(e) ? mouseDown(e) : touchDown(e)));
  viewport.addEventListener('pointermove', (e) => (e.pointerType === 'mouse' ? mouseMove(e) : touchMove(e)));
  viewport.addEventListener('pointerup', (e) => (e.pointerType === 'mouse' ? mouseUp(e) : touchUp(e)));
  viewport.addEventListener('pointercancel', (e) => (e.pointerType === 'mouse' ? releaseButtons() : touchCancel(e)));
  viewport.addEventListener('lostpointercapture', (e) => {
    if (e.pointerType === 'mouse') releaseButtons();
  });
  viewport.addEventListener('wheel', onWheel, { passive: false });
  viewport.addEventListener('contextmenu', (e) => e.preventDefault());
  // iOS still runs its own touch gestures (magnifier, callout, focus changes) unless these are cancelled.
  for (const type of ['touchstart', 'touchmove']) viewport.addEventListener(type, (e) => e.preventDefault(), { passive: false });

  document.addEventListener('keydown', onKeyDown);
  document.addEventListener('keyup', onKeyUp);
  ta.addEventListener('input', onInput);
  ta.addEventListener('compositionstart', () => {
    composing = true;
    deadEnd = false;
    // iOS sends no keydown for a dead key (Option+E), only this, while Option is held or just
    // after it came up: Option typed something, so it must not reach the remote as a lone Alt tap.
    if (opt) optionTyped();
    dropLoneOption();
  });
  ta.addEventListener('compositionend', (e) => {
    composing = false;
    deadEnd = IOS && !e.data;
    if (enabled) diffText();
  });

  window.addEventListener('blur', releaseAll);
  window.addEventListener('pagehide', releaseAll);
  document.addEventListener('visibilitychange', () => {
    if (document.visibilityState === 'hidden') releaseAll();
  });

  // ---- state

  function forget() {
    if (moveRaf) cancelAnimationFrame(moveRaf);
    moveRaf = 0;
    pendingPos = null;
    if (fakeCtrl) clearTimeout(fakeCtrl.timer);
    fakeCtrl = null;
    clearTimeout(tapHold);
    tapHold = 0;
    if (g) clearTimeout(g.timer);
    g = touches.size ? { kind: 'done' } : null;
    held.clear();
    keysDown.clear();
    down.clear();
    underCmd.clear();
    underCtrl.clear();
    cmd = null;
    opt = null;
    dropLoneOption();
    deadEnd = false;
    for (const timer of stripKeys.values()) clearTimeout(timer);
    stripKeys.clear();
    wheelX = 0;
    wheelY = 0;
    for (const name in sticky) sticky[name] = 0;
    env.onChange();
  }

  /**
   * Sends RELEASE_ALL (when controlling and a key or button is down) and forgets everything held
   * locally. Only then: RELEASE_ALL also stops a TEXT the server is still typing, and losing focus
   * (even to the address bar) must not cut a paste short.
   */
  function releaseAll() {
    if (enabled) {
      flushMove();
      if (keysDown.size || held.size) send(new Uint8Array([RELEASE_ALL]));
    }
    forget();
  }

  return {
    get enabled() { return enabled; },
    /** performance.now() of the last input message sent. */
    get lastInputAt() { return lastInputAt; },
    /** Input flows only while this session holds control; losing it drops local state (the server releases). */
    setEnabled(on) {
      if (on === enabled) return;
      forget();
      enabled = on;
      vc = null;
      if (on) resetPad();
      env.onChange();
    },
    releaseAll,
    /** Must run inside a user gesture handler: iOS only opens the keyboard from one. */
    focusKeyboard,
    blurKeyboard() { ta.blur(); },
    keyboardFocused() { return document.activeElement === ta; },
    /** Literal text (the Type text dialog). */
    sendText: sendLiteral,
    holdKey,
    releaseKey,
    /** Extra-key strip modifiers: off -> next key only -> locked -> off. */
    toggleSticky(name) {
      if (enabled) setSticky(name, (sticky[name] + 1) % 3);
    },
    sticky() { return { ...sticky }; },
    /** The trackpad cursor while controlling in trackpad mode, else null (draw the server's). */
    virtualCursor() { return enabled && trackpad() ? vc : null; },
    serverCursor(x, y) {
      serverPos = [x, y];
      if (vc && performance.now() - localMoveAt > CURSOR_ADOPT_MS && !touches.size) vc = [x, y];
    },
  };
}
