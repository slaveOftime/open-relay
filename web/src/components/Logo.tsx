export default function Logo({ size = 36 }: { size?: number }) {
  return <img src="/icon.svg" alt="" aria-hidden="true" width={size} height={size} />
}
