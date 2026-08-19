import type { ProviderId } from '@wakuwaku/client'

export function resolvedSessionModel(
  sessionModel: string | null | undefined,
  models: readonly { id: string; supported: boolean }[],
): string | undefined {
  const requested = sessionModel?.trim()
  if (requested && models.some((model) => model.id === requested && model.supported)) return requested
  return models.find((model) => model.supported)?.id
}

export function resolvedCatalogModel<T extends { id: string; supported: boolean }>(
  sessionModel: string | null | undefined,
  models: readonly T[] | undefined,
): T | undefined {
  if (!models?.length) return undefined
  const id = resolvedSessionModel(sessionModel, models)
  return id ? models.find((model) => model.id === id) : undefined
}

export function readyCatalogModels<T>(
  query: { isError: boolean; isFetched: boolean; data?: { models?: readonly T[] } },
): readonly T[] | undefined {
  if (query.isError || !query.isFetched || query.data === undefined) return undefined
  return query.data.models
}

export interface ModelPickerRow {
  provider: ProviderId
  model: { id: string }
}

export function selectedModelPickerIndex(
  rows: readonly ModelPickerRow[],
  provider: ProviderId,
  modelId: string | undefined,
) {
  if (!modelId) return -1
  return rows.findIndex((row) => row.provider === provider && row.model.id === modelId)
}

export function nextModelPickerHighlight(
  current: number | null,
  length: number,
  direction: 'next' | 'previous',
) {
  if (!length) return null
  if (current === null) return direction === 'next' ? 0 : length - 1
  return direction === 'next'
    ? (current + 1) % length
    : (current - 1 + length) % length
}
