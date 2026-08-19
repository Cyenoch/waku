import type { ModelCatalogEntry, ServiceTier } from '@wakuwaku/client'

export function fastModeEnabled(
  model: Pick<ModelCatalogEntry, 'capabilities'> | undefined,
  serviceTier: ServiceTier | null | undefined,
): boolean {
  return Boolean(model?.capabilities.serviceTier && serviceTier === 'priority')
}

export function toggleFastMode(
  model: Pick<ModelCatalogEntry, 'capabilities'> | undefined,
  serviceTier: ServiceTier | null | undefined,
): ServiceTier | null {
  if (!model?.capabilities.serviceTier) return null
  return fastModeEnabled(model, serviceTier) ? null : 'priority'
}

export function serviceTierForModel(
  model: Pick<ModelCatalogEntry, 'capabilities'> | undefined,
  serviceTier: ServiceTier | null | undefined,
): ServiceTier | null {
  if (!model?.capabilities.serviceTier) return null
  return serviceTier ?? null
}
