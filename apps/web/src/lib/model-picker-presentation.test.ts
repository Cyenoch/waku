import { describe, expect, test } from 'bun:test'
import { nextModelPickerHighlight, selectedModelPickerIndex } from './model-picker-presentation'
import { fastModeEnabled, serviceTierForModel, toggleFastMode } from './service-tier'

describe('model picker presentation', () => {
  test('selects a configured endpoint model', () => {
    const rows = [{ provider: 'local', model: { id: 'model-a' } }, { provider: 'remote', model: { id: 'model-b' } }]
    expect(selectedModelPickerIndex(rows, 'remote', 'model-b')).toBe(1)
    expect(selectedModelPickerIndex(rows, 'remote', 'missing')).toBe(-1)
  })

  test('cycles keyboard highlight', () => {
    expect(nextModelPickerHighlight(null, 3, 'next')).toBe(0)
    expect(nextModelPickerHighlight(0, 3, 'previous')).toBe(2)
  })
})

describe('catalog service tier gating', () => {
  test('uses only the selected catalog capability', () => {
    const supported = { apiFormat: 'openAiResponses' as const, capabilities: { serviceTier: true, reasoningEffort: false, reasoningSummary: false, sampling: false } }
    const unsupported = { apiFormat: 'openAiChat' as const, capabilities: { serviceTier: false, reasoningEffort: false, reasoningSummary: false, sampling: false } }
    expect(serviceTierForModel(supported, 'priority')).toBe('priority')
    expect(serviceTierForModel(unsupported, 'priority')).toBeNull()
    expect(serviceTierForModel(undefined, 'priority')).toBeNull()
    expect(fastModeEnabled(supported, 'priority')).toBe(true)
    expect(fastModeEnabled(supported, 'default')).toBe(false)
    expect(toggleFastMode(supported, null)).toBe('priority')
    expect(toggleFastMode(supported, 'priority')).toBeNull()
    expect(toggleFastMode(unsupported, 'priority')).toBeNull()
  })
})
