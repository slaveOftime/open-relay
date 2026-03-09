import { useEffect, useRef, useState } from 'react'
import { buildSparklineSvg } from '@/utils/format'

interface Props {
  series: number[]
  width?: number
  height?: number
  fullWidth?: boolean
  className?: string
  enableAnimation: boolean
}

export default function SparklineSvg({
  series,
  width = 80,
  height = 22,
  fullWidth = false,
  className,
  enableAnimation,

}: Props) {
  const hostRef = useRef<HTMLSpanElement | null>(null)
  const [measuredWidth, setMeasuredWidth] = useState<number>(width)

  useEffect(() => {
    if (!fullWidth) return
    const el = hostRef.current
    if (!el) return
    const update = () => {
      const next = Math.max(1, Math.round(el.getBoundingClientRect().width))
      setMeasuredWidth((prev) => (prev === next ? prev : next))
    }
    update()
    if (typeof ResizeObserver === 'undefined') return
    const observer = new ResizeObserver(() => update())
    observer.observe(el)
    return () => observer.disconnect()
  }, [fullWidth])

  const renderWidth = fullWidth ? measuredWidth : width
  const svg = buildSparklineSvg(series, renderWidth, height, enableAnimation)
  const classes = fullWidth
    ? `block w-full align-middle ${className ?? ''}`.trim()
    : `inline-block align-middle ${className ?? ''}`.trim()
  return <span ref={hostRef} className={classes} dangerouslySetInnerHTML={{ __html: svg }} />
}
