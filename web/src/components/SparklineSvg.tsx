import { buildSparklineSvg } from '@/utils/format'

interface Props {
  series: number[]
  width?: number
  height?: number
}

export default function SparklineSvg({ series, width = 80, height = 22 }: Props) {
  const svg = buildSparklineSvg(series, width, height)
  return <span className="inline-block align-middle" dangerouslySetInnerHTML={{ __html: svg }} />
}
