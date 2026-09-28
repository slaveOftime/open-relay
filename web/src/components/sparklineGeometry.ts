export type SparklinePoint = { x: number; y: number }

export function sameSparklinePoints(left: SparklinePoint[], right: SparklinePoint[]): boolean {
  return (
    left.length === right.length &&
    left.every((point, index) => point.x === right[index].x && point.y === right[index].y)
  )
}

export function interpolateSparklinePoints(
  from: SparklinePoint[],
  to: SparklinePoint[],
  progress: number
): SparklinePoint[] {
  return to.map((target, index) => ({
    x: from[index].x + (target.x - from[index].x) * progress,
    y: from[index].y + (target.y - from[index].y) * progress,
  }))
}

export function easeOutCubic(progress: number): number {
  return 1 - (1 - progress) ** 3
}

/** A tick shifted the rolling history left one bucket without a new byte total. */
export function isBucketShift(previous: readonly number[], next: readonly number[]): boolean {
  return (
    previous.length === next.length &&
    previous.length > 1 &&
    next[next.length - 1] === 0 &&
    previous.slice(1).every((value, index) => value === next[index])
  )
}

/** Keep horizontal bucket motion on its original deadline when bytes arrive mid-scroll. */
export function animationWindow(
  now: number,
  bucketShifted: boolean,
  previousScrollEnd: number | null,
  bucketMs: number,
  changeMs: number
): { duration: number; scrollEnd: number | null } {
  const scrollEnd = bucketShifted ? now + bucketMs : previousScrollEnd
  return {
    duration: scrollEnd !== null && scrollEnd > now ? scrollEnd - now : changeMs,
    scrollEnd,
  }
}

/** Rolling motion stays linear across bucket boundaries; new output eases in. */
export function animationEase(progress: number, scrolling: boolean): number {
  return scrolling ? progress : easeOutCubic(progress)
}
