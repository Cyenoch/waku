import { describe, expect, test } from 'bun:test'
import {
  readComposerPreferences,
  rememberedModelTraits,
  rememberComposerSession,
  writeComposerPreferences,
} from './composer-preferences'

describe('composer preferences', () => {
  test('remembers the last owned endpoint and model per daemon', () => {
    const storage = memoryStorage()
    const remembered = rememberComposerSession(
      readComposerPreferences(storage, 'ws://first'),
      {
        provider: 'custom-endpoint',
        model: 'model-a',
        reasoning_effort: 'high',
        service_tier: 'priority',
        context_window: '1m',
      },
    )
    writeComposerPreferences(storage, 'ws://first', remembered)

    expect(readComposerPreferences(storage, 'ws://first')).toMatchObject({
      lastProvider: 'custom-endpoint',
      lastModel: 'model-a',
      lastReasoningEffort: 'high',
      lastServiceTier: 'priority',
      lastContextWindow: '1m',
    })
    expect(rememberedModelTraits(remembered, 'custom-endpoint', 'model-a')).toEqual({
      reasoningEffort: 'high',
      serviceTier: 'priority',
      contextWindow: '1m',
    })
    expect(readComposerPreferences(storage, 'ws://second').lastModel).toBeNull()
  })

  test('does not erase an explicit model when a blank draft is selected', () => {
    const preferences = rememberComposerSession(
      readComposerPreferences(null, 'ws://first'),
      { provider: 'anthropic', model: 'claude-opus', reasoning_effort: null, service_tier: null, context_window: null },
    )
    expect(rememberComposerSession(preferences, {
      provider: 'custom-endpoint', model: null, reasoning_effort: null, service_tier: null, context_window: null,
    })).toBe(preferences)
  })

  test('rejects unknown persisted service-tier values', () => {
    const storage = memoryStorage()
    storage.setItem('waku.composer-preferences.v2', JSON.stringify({
      'ws://first': {
        lastProvider: 'custom-endpoint',
        lastModel: 'model-a',
        lastServiceTier: 'turbo',
        modelTraits: { 'custom-endpoint\u0000model-a': { serviceTier: 'turbo' } },
      },
    }))
    const preferences = readComposerPreferences(storage, 'ws://first')
    expect(preferences.lastServiceTier).toBeNull()
    expect(rememberedModelTraits(preferences, 'custom-endpoint', 'model-a')?.serviceTier).toBeNull()
  })
})

function memoryStorage() {
  const values = new Map<string, string>()
  return {
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, value: string) => { values.set(key, value) },
  }
}
