import { useQueries, useQuery } from '@tanstack/react-query'
import type { ModelCatalog, ProviderId } from '@waku/client'
import { useDaemon } from '@/lib/daemon-context'
import {
  daemonKeys,
  discoverComposerCommands,
  hydrateSession,
  inspectWorkspaceBranches,
  listModels,
  listSessionTurnRefs,
  listComposerFiles,
  loadAuthStatus,
  loadComposerDrafts,
  loadSkills,
  loadDaemonSettings,
  loadTaskState,
  loadUsageHistory,
} from '@/lib/daemon-api'
export function useTaskState() {
  const { client, config, phase } = useDaemon()
  return useQuery({
    queryKey: daemonKeys.taskState(config?.address ?? 'disconnected'),
    queryFn: () => loadTaskState(requireClient(client)),
    enabled: phase === 'connected' && Boolean(client && config),
  })
}

export function useComposerDrafts() {
  const { client, config, phase } = useDaemon()
  return useQuery({
    queryKey: daemonKeys.composerDrafts(config?.address ?? 'disconnected'),
    queryFn: () => loadComposerDrafts(requireClient(client)),
    enabled: phase === 'connected' && Boolean(client && config),
    staleTime: Number.POSITIVE_INFINITY,
  })
}

export function useWorkspaceBranches(cwd: string | undefined) {
  const { client, config, phase } = useDaemon()
  return useQuery({
    queryKey: daemonKeys.workspace(config?.address ?? 'disconnected', cwd ?? 'none'),
    queryFn: () => inspectWorkspaceBranches(requireClient(client), cwd!),
    enabled: phase === 'connected' && Boolean(client && config && cwd),
    staleTime: 5_000,
  })
}

export function useSessionTurnRefs(
  cwd: string | undefined,
  sessionId: string | undefined,
) {
  const { client, config, phase } = useDaemon()
  return useQuery({
    queryKey: daemonKeys.sessionTurnRefs(
      config?.address ?? 'disconnected',
      cwd ?? 'none',
      sessionId ?? 'none',
    ),
    queryFn: () => listSessionTurnRefs(requireClient(client), cwd!, sessionId!),
    enabled: phase === 'connected' && Boolean(client && config && cwd && sessionId),
    staleTime: 5_000,
  })
}

export function useComposerFiles(cwd: string | undefined) {
  const { client, config, phase } = useDaemon()
  return useQuery({
    queryKey: daemonKeys.composerFiles(config?.address ?? 'disconnected', cwd ?? 'none'),
    queryFn: () => listComposerFiles(requireClient(client), cwd!),
    enabled: phase === 'connected' && Boolean(client && config && cwd),
    staleTime: Number.POSITIVE_INFINITY,
  })
}

export function useComposerCommands(
  provider: ProviderId | undefined,
  cwd: string | undefined,
) {
  const { client, config, phase } = useDaemon()
  return useQuery({
    queryKey: daemonKeys.slashCommands(
      config?.address ?? 'disconnected',
      provider ?? '',
      cwd ?? 'none',
    ),
    queryFn: () => discoverComposerCommands(requireClient(client), provider!, cwd!),
    enabled: phase === 'connected' && Boolean(client && config && provider && cwd),
    staleTime: Number.POSITIVE_INFINITY,
  })
}

export function useSession(sessionId: string | undefined) {
  const { client, config, phase } = useDaemon()
  return useQuery({
    queryKey: daemonKeys.session(config?.address ?? 'disconnected', sessionId ?? 'none'),
    queryFn: () => hydrateSession(requireClient(client), sessionId!),
    enabled: phase === 'connected' && Boolean(client && config && sessionId),
    staleTime: 1_000,
  })
}

export function useDaemonSettings() {
  const { client, config, phase } = useDaemon()
  return useQuery({
    queryKey: daemonKeys.settings(config?.address ?? 'disconnected'),
    queryFn: () => loadDaemonSettings(requireClient(client)),
    enabled: phase === 'connected' && Boolean(client && config),
    staleTime: 60_000,
  })
}

export function useProviderAuth(provider?: ProviderId) {
  const { client, config, phase } = useDaemon()
  return useQuery({
    queryKey: daemonKeys.auth(config?.address ?? 'disconnected', provider),
    queryFn: () => loadAuthStatus(requireClient(client), provider),
    enabled: phase === 'connected' && Boolean(client && config),
    refetchInterval: (query) => query.state.data?.phases.length ? 1_000 : false,
  })
}

export function useModelCatalog(provider: ProviderId | undefined) {
  const { client, config, phase } = useDaemon()
  return useQuery<ModelCatalog>({
    queryKey: daemonKeys.models(config?.address ?? 'disconnected', provider ?? ''),
    queryFn: () => listModels(requireClient(client), provider!),
    enabled: phase === 'connected' && Boolean(client && config && provider),
    staleTime: Number.POSITIVE_INFINITY,
  })
}

export function useModelCatalogs(providerIds: readonly ProviderId[]) {
  const { client, config, phase } = useDaemon()
  return useQueries({
    queries: providerIds.map((provider) => ({
      queryKey: daemonKeys.models(config?.address ?? 'disconnected', provider),
      queryFn: () => listModels(requireClient(client), provider),
      enabled: phase === 'connected' && Boolean(client && config),
      staleTime: Number.POSITIVE_INFINITY,
    })),
  })
}

export function useSkills(projects: Parameters<typeof loadSkills>[1]) {
  const { client, config, phase } = useDaemon()
  return useQuery({
    queryKey: daemonKeys.skills(config?.address ?? 'disconnected'),
    queryFn: () => loadSkills(requireClient(client), projects),
    enabled: phase === 'connected' && Boolean(client && config),
  })
}

export function useUsageHistory(
  window: Parameters<typeof loadUsageHistory>[1],
) {
  const { client, config, phase } = useDaemon()
  return useQuery({
    queryKey: daemonKeys.usage(config?.address ?? 'disconnected', window),
    queryFn: () => loadUsageHistory(requireClient(client), window),
    enabled: phase === 'connected' && Boolean(client && config),
    placeholderData: (previous) => previous,
  })
}

function requireClient<T>(client: T | null): T {
  if (!client) throw new Error('Waku daemon is disconnected')
  return client
}
