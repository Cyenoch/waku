import type { ModelCatalogEntry } from '@wakuwaku/client'

export function reasoningEffortForModel(
  model: Pick<ModelCatalogEntry, 'supported' | 'capabilities' | 'reasoningEfforts'> | undefined,
  canonicalEffort: string | null | undefined,
): string | null {
  if (!model?.supported || !model.capabilities.reasoningEffort || !canonicalEffort) return null
  return model.reasoningEfforts?.find((effort) => effort.id === canonicalEffort)?.providerValue ?? null
}

export function canonicalReasoningEffortForModel(
  model: Pick<ModelCatalogEntry, 'supported' | 'capabilities' | 'reasoningEfforts'> | undefined,
  canonicalEffort: string | null | undefined,
): string | null {
  if (!model?.supported || !model.capabilities.reasoningEffort || !canonicalEffort) return null
  return model.reasoningEfforts?.some((effort) => effort.id === canonicalEffort)
    ? canonicalEffort
    : null
}
