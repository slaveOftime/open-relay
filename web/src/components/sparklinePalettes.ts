/**
 * Sparkline palettes. Kept out of SparklineSvg.tsx so the renderer holds
 * rendering only, and the colors can be checked without mounting anything.
 */

export type SparklinePalette = {
  stroke: string
  strokeHighlight: string
  glow: string
  fillTop: string
  fillBottom: string
  dot: string
  baseline: string
}

export const RUNNING_PALETTE: SparklinePalette = {
  stroke: '#34C85B',
  strokeHighlight: '#8EF5AB',
  glow: '#2BC851A6',
  fillTop: '#34C85B',
  fillBottom: '#34c85b86',
  dot: '#34C85B',
  baseline: '#34c85b86',
}

export const IDLE_PALETTE: SparklinePalette = {
  stroke: '#7D8B97',
  strokeHighlight: '#C5D0D8',
  glow: '#32404B66',
  fillTop: '#7D8B9724',
  fillBottom: '#11181D00',
  dot: '#D6DEE4',
  baseline: '#24303A',
}

export function sparklinePalette(isRunning: boolean): SparklinePalette {
  return isRunning ? RUNNING_PALETTE : IDLE_PALETTE
}
