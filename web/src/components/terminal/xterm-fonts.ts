/**
 * Terminal font stack and the embedded faces that back it.
 *
 * The app bundles JetBrains Mono for the terminal (see the woff2 in
 * src/assets/fonts, released by `unpack-fonts.mjs`). Browser build metadata:
 * JetBrains Mono covers Latin, box drawing and some symbols, but not the
 * ranges tools actually print: Misc Technical (U+23F5 ⏵, U+23F8 ⏸), Braille
 * spinners (U+2819 ⠙), dingbats (U+2713 ✓, U+273B ✻) and geometric shapes
 * (U+25CB ○, U+25D0 ◐). Those are resolved by the platform's symbol fonts:
 * Windows font-links Segoe UI Symbol and renders them, while a phone's
 * monospace chain has no equivalent and drew tofu.
 *
 * `Open Relay Terminal Symbols` (see XTerm.css) is a subset of Noto Sans
 * Symbols 2 with every glyph scaled to this cell, so the missing characters are
 * monospaced and identical everywhere; it only claims the blocks it was built
 * for. The system symbol, emoji and CJK families below stay as a last resort
 * for anything still uncovered - notably U+23BF ⎿, which no bundled font has.
 */
export const TERMINAL_FONT_SIZE = 13
export const TERMINAL_FONT_FACE = '"Open Relay Terminal"'
export const TERMINAL_SYMBOL_FACE = 'Open Relay Terminal Symbols'
export const TERMINAL_FONT_FAMILY =
  `${TERMINAL_FONT_FACE}, "${TERMINAL_SYMBOL_FACE}", ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, ` +
  '"Liberation Mono", "Courier New", monospace, ' +
  '"Segoe UI Symbol", "Noto Sans Symbols2", "Noto Sans Symbols", "Apple Symbols", ' +
  '"Segoe UI Emoji", "Noto Color Emoji", "Apple Color Emoji", ' +
  '"Noto Sans CJK SC", "Microsoft YaHei", "PingFang SC", "Hiragino Sans", sans-serif'

/**
 * Representative text for the preload below.
 *
 * `document.fonts.load(font, text)` only loads faces whose unicode-range
 * intersects `text`, so it must carry both halves: Latin (covered by the
 * bundled face, and its default sample text) *and* the symbols it does not
 * cover, which makes the browser resolve the fallback chain before the first
 * paint instead of swapping glyphs in after it.
 */
export const TERMINAL_PRELOAD_TEXT = 'BESbswy ⏵⏸⠙✓✻○◐▣⎿'
export const TERMINAL_FONT_VARIANTS = [
  `400 ${TERMINAL_FONT_SIZE}px ${TERMINAL_FONT_FACE}`,
  `700 ${TERMINAL_FONT_SIZE}px ${TERMINAL_FONT_FACE}`,
  `italic 400 ${TERMINAL_FONT_SIZE}px ${TERMINAL_FONT_FACE}`,
  `italic 700 ${TERMINAL_FONT_SIZE}px ${TERMINAL_FONT_FACE}`,
  // The symbol fallback is small and only shows up on scattered cells;
  // preloading it avoids a flash of tofu the first time one scrolls past.
  `${TERMINAL_FONT_SIZE}px "${TERMINAL_SYMBOL_FACE}"`,
]
