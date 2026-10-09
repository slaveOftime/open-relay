/**
 * The sparkline model: everything the renderer needs to draw one frame,
 * computed from the activity series. Pure, so it can be tested without
 * mounting the component.
 */

import type { SparklinePoint } from './sparklineGeometry'
import { buildAreaPath, buildSmoothLinePath, buildSparklinePoints } from './sparklineGeometry'
import { sparklinePalette, type SparklinePalette } from './sparklinePalettes'

export interface SparklineModel {
  areaPath: string
  linePath: string
  lastPoint: SparklinePoint
  points: SparklinePoint[]
  baselineY: number
  palette: SparklinePalette
}

export function buildSparklineModel(
  series: number[],
  width: number,
  height: number,
  isRunning: boolean
): SparklineModel {
  const baselineY = Math.max(2, height - 3)
  const palette = sparklinePalette(isRunning)
  const points = buildSparklinePoints(series, width, height)
  const linePath = buildSmoothLinePath(points)
  const areaPath = buildAreaPath(points, baselineY)

  return {
    areaPath,
    linePath,
    lastPoint: points[points.length - 1] ?? { x: width, y: baselineY },
    points,
    baselineY,
    palette,
  }
}
