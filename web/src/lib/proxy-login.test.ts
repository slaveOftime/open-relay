import { describe, expect, it } from 'vitest'
import { proxyLoginDestination } from './proxy-login'

describe('proxyLoginDestination', () => {
  it('restores a proxied app deep link after login', () => {
    expect(
      proxyLoginDestination(
        'https://relay.example/login?next=%2Fapps%2Freport%2Fentry%3Ftab%3Dlogs%26page%3D2'
      )
    ).toBe('/apps/report/entry?tab=logs&page=2')
  })

  it.each([
    'https://relay.example/login?next=https%3A%2F%2Fevil.example%2Fapps%2Ffoo',
    'https://relay.example/login?next=%2F%2Fevil.example%2Fapps%2Ffoo',
    'https://relay.example/login?next=%2Fapps%2F..%2Fapi%2Fhealth',
    'https://relay.example/login?next=%2Fapps%2Ffoo%5C%5Cevil.example',
    'https://relay.example/login?next=%2Fsession%2F123',
    'https://relay.example/login',
    'https://relay.example/?next=%2Fapps%2Ffoo',
  ])('does not navigate to an untrusted or absent destination: %s', (url) => {
    expect(proxyLoginDestination(url)).toBeNull()
  })
})
