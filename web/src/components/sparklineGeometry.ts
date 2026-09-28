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
