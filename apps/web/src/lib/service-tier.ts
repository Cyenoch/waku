import type { ModelCatalogEntry, ServiceTier } from '@waku/client'
export interface ServiceTierChoice {
  value: ServiceTier
  labelKey: string
  descriptionKey: string
}

export const SERVICE_TIER_OPTIONS: readonly ServiceTierChoice[] = [
  { value: 'auto', labelKey: 'model_option.auto', descriptionKey: 'model_option.auto_description' },
  { value: 'default', labelKey: 'model_option.default', descriptionKey: 'model_option.default_description' },
  { value: 'flex', labelKey: 'model_option.flex', descriptionKey: 'model_option.flex_description' },
  { value: 'priority', labelKey: 'model_option.priority', descriptionKey: 'model_option.priority_description' },
]


export function serviceTierForModel(
  model: Pick<ModelCatalogEntry, 'capabilities' | 'apiFormat'> | undefined,
  serviceTier: ServiceTier | null | undefined,
): ServiceTier | null {
  if (!model?.capabilities.serviceTier) return null
  return serviceTier ?? null
}
