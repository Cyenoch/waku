import { areaY, crosshair, defineChart, lineY } from '@tanstack/charts'
import { decorative } from '@tanstack/charts/mark/decorative'
import { Chart } from '@tanstack/charts/react'
import { scaleLinear } from '@tanstack/charts/scales/linear'
import { tooltip } from '@tanstack/charts/tooltip'
import type { ProviderId, UsageHistory } from '@wakuwaku/client'
import { scaleUtc } from 'd3-scale'
import { useI18n, type AppLocale } from '@/lib/i18n'
import { providerMeta } from '@/components/waku-icon'

export type UsageMetric = 'cost' | 'tokens'

interface UsageChartRow {
  id: string
  date: Date
  provider: string
  value: number
}

export function UsageTrendChart({
  history,
  metric,
}: {
  history: UsageHistory
  metric: UsageMetric
}) {
  const { locale, t } = useI18n()
  const providers = history.providers.map((slice) => slice.provider)
  const rows: UsageChartRow[] = history.daily.flatMap((day) => {
    const date = parseUsageDay(day.day)
    return providers.map((provider) => {
      const entry = day.byProvider.find((candidate) => candidate.provider === provider)
      return {
        id: `${provider}-${day.day}`,
        date,
        provider: providerMeta(provider).name,
        value: metric === 'cost' ? (entry?.costUsd ?? 0) : (entry?.totalTokens ?? 0),
      }
    })
  })

  const definition = defineChart({
    marks: [
      decorative(areaY(rows, {
        id: 'usage-area',
        x: 'date',
        y: 'value',
        z: 'provider',
        color: 'provider',
        key: 'id',
        fillOpacity: 0.1,
      })),
      lineY(rows, {
        id: 'usage-line',
        x: 'date',
        y: 'value',
        z: 'provider',
        color: 'provider',
        key: 'id',
        strokeWidth: 1.8,
      }),
      crosshair({
        x: { label: false, stroke: 'var(--text-ghost)', strokeOpacity: 0.7 },
        y: false,
      }),
    ],
    x: {
      scale: scaleUtc,
      axis: {
        line: false,
        ticks: {
          count: 3,
          size: 0,
          padding: 8,
          format: (value) => formatChartDay(value, locale),
        },
        tickLabels: { thin: { priority: 'ends', minGap: 24 }, fontSize: 10 },
      },
    },
    y: {
      scale: scaleLinear,
      nice: true,
      grid: true,
      axis: {
        line: false,
        ticks: {
          count: 4,
          size: 0,
          padding: 8,
          format: (value) => metric === 'cost'
            ? formatCompactMoney(value, locale)
            : formatCompactNumber(value, locale),
        },
        tickLabels: { fontSize: 10 },
      },
    },
    color: {
      domain: providers.map((provider) => providerMeta(provider).name),
      range: providers.map((provider) => providerSeriesColor(provider)),
    },
    focus: 'group-x',
    maxFocusDistance: Number.POSITIVE_INFINITY,
    tooltip,
  })

  if (!rows.length) {
    return (
      <div className="grid h-[188px] place-items-center text-[11.5px] text-[var(--text-tertiary)]">
        {t('usage.no_activity_window')}
      </div>
    )
  }

  return (
    <Chart
      ariaDescription={t('usage.chart_description', {
        metric: t(metric === 'cost' ? 'usage.cost' : 'usage.processed_tokens'),
      })}
      ariaLabel={t(metric === 'cost' ? 'usage.daily_cost' : 'usage.daily_processed_tokens')}
      className="waku-usage-chart"
      definition={definition}
      height={188}
      initialWidth={640}
    />
  )
}

const KNOWN_PROVIDER_SERIES_COLORS: Record<string, string> = {
  anthropic: '#d97757',
  'openai-responses': '#10a37f',
  'openai-chat': '#0d8f6e',
  'openai-codex': '#6366f1',
  xai: '#0ea5e9',
  'xai-oauth': '#38bdf8',
  'opencode-zen': '#a855f7',
  'opencode-go': '#f59e0b',
}

const PROVIDER_SERIES_PALETTE = [
  '#d97757',
  '#10a37f',
  '#0ea5e9',
  '#a855f7',
  '#f59e0b',
  '#ef4444',
  '#14b8a6',
  '#6366f1',
] as const

export function providerSeriesColor(provider: ProviderId) {
  const id = provider.toLowerCase()
  const known = KNOWN_PROVIDER_SERIES_COLORS[id]
  if (known) return known
  let hash = 2166136261
  for (let index = 0; index < id.length; index += 1) {
    hash ^= id.charCodeAt(index)
    hash = Math.imul(hash, 16777619)
  }
  return PROVIDER_SERIES_PALETTE[(hash >>> 0) % PROVIDER_SERIES_PALETTE.length]
}

function parseUsageDay(day: string) {
  return new Date(`${day}T00:00:00Z`)
}

function formatChartDay(value: Date, locale: AppLocale) {
  return new Intl.DateTimeFormat(locale, { month: 'short', day: 'numeric', timeZone: 'UTC' }).format(value)
}

function formatCompactMoney(value: number, locale: AppLocale) {
  if (!value) return '$0'
  return new Intl.NumberFormat(locale, {
    style: 'currency',
    currency: 'USD',
    notation: value >= 1_000 ? 'compact' : 'standard',
    maximumFractionDigits: value < 10 ? 1 : 0,
  }).format(value)
}

function formatCompactNumber(value: number, locale: AppLocale) {
  if (!value) return '0'
  return new Intl.NumberFormat(locale, { notation: 'compact', maximumFractionDigits: 1 }).format(value)
}
