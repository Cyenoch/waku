import { describe, expect, test } from 'bun:test'
import { providerSeriesColor } from './usage-chart'

const BUILTIN_PROVIDERS = [
  'anthropic',
  'openai-responses',
  'openai-chat',
  'openai-codex',
  'xai',
  'xai-oauth',
  'opencode-zen',
  'opencode-go',
] as const

describe('providerSeriesColor', () => {
  test('maps the same provider id to the same color', () => {
    expect(providerSeriesColor('custom-endpoint')).toBe(providerSeriesColor('custom-endpoint'))
    expect(providerSeriesColor('Anthropic')).toBe(providerSeriesColor('anthropic'))
  })

  test('keeps the eight built-in providers visually distinct', () => {
    const colors = BUILTIN_PROVIDERS.map((provider) => providerSeriesColor(provider))
    expect(new Set(colors).size).toBe(BUILTIN_PROVIDERS.length)
  })

  test('does not collapse unknown providers onto the Anthropic or Codex slots', () => {
    const anthropic = providerSeriesColor('anthropic')
    const codex = providerSeriesColor('openai-codex')
    const custom = ['acme', 'local-llama', 'together', 'groq'].map((id) =>
      providerSeriesColor(id),
    )
    expect(custom.every((color) => color === anthropic)).toBe(false)
    expect(custom.every((color) => color === codex)).toBe(false)
  })
})
