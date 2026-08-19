import type { AgentSession, ModelCatalogEntry, ProviderId, ServiceTier } from '@wakuwaku/client'
import { canonicalReasoningEffortForModel } from './reasoning-effort'
import { serviceTierForModel } from './service-tier'

const STORAGE_KEY = 'wakuwaku.composer-preferences.v2'
type StorageLike = Pick<Storage, 'getItem' | 'setItem'>

export interface RememberedModelTraits {
  reasoningEffort: string | null
  serviceTier: ServiceTier | null
  contextWindow: string | null
}

export interface ComposerPreferences {
  lastProvider: ProviderId
  lastModel: string | null
  lastReasoningEffort: string | null
  lastServiceTier: ServiceTier | null
  lastContextWindow: string | null
  modelTraits: Record<string, RememberedModelTraits>
}

const DEFAULT_PREFERENCES: ComposerPreferences = {
  lastProvider: '',
  lastModel: null,
  lastReasoningEffort: null,
  lastServiceTier: null,
  lastContextWindow: null,
  modelTraits: {},
}

export function browserComposerPreferenceStorage(): StorageLike | null {
  if (typeof window === 'undefined') return null
  try { return window.localStorage } catch { return null }
}

export function readComposerPreferences(storage: StorageLike | null, daemonAddress: string): ComposerPreferences {
  if (!storage) return { ...DEFAULT_PREFERENCES, modelTraits: {} }
  try {
    const entries = JSON.parse(storage.getItem(STORAGE_KEY) ?? '{}') as Record<string, unknown>
    return parsePreferences(entries[daemonAddress])
  } catch {
    return { ...DEFAULT_PREFERENCES, modelTraits: {} }
  }
}

export function writeComposerPreferences(storage: StorageLike | null, daemonAddress: string, preferences: ComposerPreferences): void {
  if (!storage) return
  let entries: Record<string, unknown> = {}
  try {
    const parsed = JSON.parse(storage.getItem(STORAGE_KEY) ?? '{}')
    if (isRecord(parsed)) entries = parsed
  } catch {
    // Replace malformed disposable app state with the current preference.
  }
  entries[daemonAddress] = preferences
  try { storage.setItem(STORAGE_KEY, JSON.stringify(entries)) } catch { /* storage is optional */ }
}

export function rememberComposerSession(
  preferences: ComposerPreferences,
  session: Pick<AgentSession, 'provider' | 'model' | 'reasoning_effort' | 'service_tier' | 'context_window'>,
): ComposerPreferences {
  if (!session.model) return preferences
  const reasoningEffort = session.reasoning_effort ?? null
  const serviceTier = session.service_tier ?? null
  const contextWindow = session.context_window ?? null
  return {
    ...preferences,
    lastProvider: session.provider,
    lastModel: session.model,
    lastReasoningEffort: reasoningEffort,
    lastServiceTier: serviceTier,
    lastContextWindow: contextWindow,
    modelTraits: {
      ...preferences.modelTraits,
      [modelKey(session.provider, session.model)]: { reasoningEffort, serviceTier, contextWindow },
    },
  }
}

export function rememberedModelTraits(preferences: ComposerPreferences, provider: ProviderId, model: string): RememberedModelTraits | undefined {
  return preferences.modelTraits[modelKey(provider, model)]
}

export function selectedModelTraits(
  model: Pick<ModelCatalogEntry, 'supported' | 'capabilities' | 'apiFormat' | 'reasoningEfforts' | 'defaultReasoningEffort'>,
  remembered: RememberedModelTraits | undefined,
): RememberedModelTraits {
  const defaultEffort = canonicalReasoningEffortForModel(model, model.defaultReasoningEffort)
  return {
    reasoningEffort: canonicalReasoningEffortForModel(model, remembered?.reasoningEffort)
      ?? defaultEffort,
    serviceTier: serviceTierForModel(model, remembered?.serviceTier),
    contextWindow: remembered?.contextWindow ?? null,
  }
}

function parsePreferences(value: unknown): ComposerPreferences {
  if (!isRecord(value) || typeof value.lastProvider !== 'string' || !value.lastProvider.trim()) {
    return { ...DEFAULT_PREFERENCES, modelTraits: {} }
  }
  const modelTraits: Record<string, RememberedModelTraits> = {}
  if (isRecord(value.modelTraits)) {
    for (const [key, traits] of Object.entries(value.modelTraits)) {
      if (!isRecord(traits)) continue
      const reasoningEffort = nullableString(traits.reasoningEffort) ?? null
      const serviceTier = nullableServiceTier(traits.serviceTier) ?? null
      const contextWindow = nullableString(traits.contextWindow) ?? null
      modelTraits[key] = { reasoningEffort, serviceTier, contextWindow }
    }
  }
  return {
    lastProvider: value.lastProvider,
    lastModel: nullableString(value.lastModel) ?? null,
    lastReasoningEffort: nullableString(value.lastReasoningEffort) ?? null,
    lastServiceTier: nullableServiceTier(value.lastServiceTier) ?? null,
    lastContextWindow: nullableString(value.lastContextWindow) ?? null,
    modelTraits,
  }
}

function nullableString(value: unknown): string | null | undefined {
  return value === null || typeof value === 'string' ? value : undefined
}
function nullableServiceTier(value: unknown): ServiceTier | null | undefined {
  return value === null || value === undefined || value === 'auto' || value === 'default' || value === 'flex' || value === 'priority'
    ? value as ServiceTier | null | undefined
    : undefined
}

function modelKey(provider: ProviderId, model: string): string {
  return `${provider}\u0000${model}`
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}
