import { describe, expect, test } from 'bun:test'
import type { ModelCatalogEntry } from '@wakuwaku/client'
import { canonicalReasoningEffortForModel, reasoningEffortForModel } from './reasoning-effort'

const model = {
  supported: true,
  capabilities: {
    serviceTier: false,
    reasoningEffort: true,
    reasoningSummary: false,
    sampling: false,
  },
  reasoningEfforts: [
    { id: 'low', providerValue: 'quick-pass', label: 'Quick Pass' },
    { id: 'high', providerValue: 'deep-thought', label: 'Deep Thought' },
  ],
} satisfies Pick<ModelCatalogEntry, 'supported' | 'capabilities' | 'reasoningEfforts'>

describe('reasoning effort mapping', () => {
  test('emits the selected model provider value', () => {
    expect(reasoningEffortForModel(model, 'high')).toBe('deep-thought')
  })

  test('rejects a stale canonical effort', () => {
    expect(reasoningEffortForModel(model, 'medium')).toBeNull()
    expect(canonicalReasoningEffortForModel(model, 'medium')).toBeNull()
  })

  test('preserves a catalog-backed canonical effort', () => {
    expect(canonicalReasoningEffortForModel(model, 'low')).toBe('low')
  })
})
