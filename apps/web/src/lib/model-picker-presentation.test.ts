import { describe, expect, test } from 'bun:test'
import { nextModelPickerHighlight, readyCatalogModels, resolvedCatalogModel, resolvedSessionModel, selectedModelPickerIndex } from './model-picker-presentation'
import { presentedReasoningEffort } from './reasoning-effort'
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

describe('resolved catalog model for picker and controls', () => {
  const grok = {
    id: 'grok-4.5',
    supported: true,
    capabilities: { serviceTier: false, reasoningEffort: true, reasoningSummary: false, sampling: false },
    reasoningEfforts: [
      { id: 'low', providerValue: 'low', label: 'low' },
      { id: 'high', providerValue: 'high', label: 'high' },
    ],
    defaultReasoningEffort: undefined as string | undefined,
  }
  const nano = {
    id: 'gpt-5-nano',
    supported: true,
    capabilities: { serviceTier: false, reasoningEffort: true, reasoningSummary: false, sampling: false },
    reasoningEfforts: [
      { id: 'minimal', providerValue: 'minimal', label: 'minimal' },
      { id: 'low', providerValue: 'low', label: 'low' },
    ],
  }
  const hidden = { id: 'grok-imagine', supported: false, capabilities: grok.capabilities, reasoningEfforts: grok.reasoningEfforts }

  test('stale or missing session model resolves to the first supported entry', () => {
    expect(resolvedSessionModel('kimi-k2.7-code', [hidden, grok])).toBe('grok-4.5')
    expect(resolvedCatalogModel('kimi-k2.7-code', [hidden, grok])).toEqual(grok)
    expect(resolvedCatalogModel(null, [hidden, grok])).toEqual(grok)
    expect(resolvedCatalogModel(undefined, [nano, grok])).toEqual(nano)
    expect(resolvedCatalogModel('grok-4.5', [nano, grok])).toEqual(grok)
  })

  test('controls receive the resolved catalog entry and stay closed without live catalog', () => {
    const catalog = [hidden, grok]
    const pending = readyCatalogModels<typeof grok | typeof hidden>({ isError: false, isFetched: false })
    const failed = readyCatalogModels<typeof grok | typeof hidden>({ isError: true, isFetched: true })
    const live = readyCatalogModels({ isError: false, isFetched: true, data: { models: catalog } })
    expect(resolvedCatalogModel('kimi-k2.7-code', pending)).toBeUndefined()
    expect(resolvedCatalogModel('kimi-k2.7-code', failed)).toBeUndefined()
    const controlModel = resolvedCatalogModel('kimi-k2.7-code', live)
    expect(controlModel).toEqual(grok)
    expect(controlModel?.id).toBe('grok-4.5')
    expect(presentedReasoningEffort(controlModel, null)).toBeNull()
    expect(presentedReasoningEffort({ ...controlModel!, defaultReasoningEffort: 'high' }, null)).toBe('high')
  })

  test('error with stale data and pending do not auto-select a model', () => {
    const stale = { models: [hidden, grok] }
    const errorWithStale = readyCatalogModels({ isError: true, isFetched: true, data: stale })
    const pendingWithData = readyCatalogModels({ isError: false, isFetched: false, data: stale })
    expect(errorWithStale).toBeUndefined()
    expect(pendingWithData).toBeUndefined()
    expect(resolvedCatalogModel(null, errorWithStale)).toBeUndefined()
    expect(resolvedCatalogModel('kimi-k2.7-code', pendingWithData)).toBeUndefined()
    expect(resolvedCatalogModel(undefined, errorWithStale)).toBeUndefined()
  })

  test('does not invent a first-effort selection without persisted or live default', () => {
    expect(presentedReasoningEffort(grok, null)).toBeNull()
    expect(presentedReasoningEffort(grok, 'stale')).toBeNull()
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
