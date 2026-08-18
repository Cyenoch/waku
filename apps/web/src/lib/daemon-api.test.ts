import { activeAuthPhase, isActiveAuthPhase, isAllowedExternalUrl } from '@/components/settings-view'
import { explicitModelFallback, resolvedSessionModel } from '@/components/model-picker'
import { authStatusPollIntervalMs } from '@/lib/daemon-api'
import { describe, expect, test } from 'bun:test'
import type { ComposerDraftChange, Project, WakuClient } from '@wakuwaku/client'
import {
  applyComposerDraftChanges,
  beginTurn,
  browseDaemonDirectory,
  captureTurnCheckpoint,
  captureTurnStart,
  completeApiKeyLogin,
  createProject,
  createSession,
  listModels,
  persistProject,
  persistSession,
  removeSession,
  selectableProjects,
  startLogin,
  writeWorkspaceTextFile,
  type DaemonDirectory,
} from './daemon-api'

describe('applyComposerDraftChanges', () => {
  test('sends keyed updates instead of replacing every client draft', async () => {
    let command: unknown
    const client = {
      request: async (next: unknown) => {
        command = next
        return { type: 'ack' }
      },
    } as unknown as WakuClient
    const changes: ComposerDraftChange[] = [{
      target: { type: 'session', sessionId: 'session' },
      draft: { text: 'keep this', attachments: [] },
    }]

    await expect(applyComposerDraftChanges(client, changes)).resolves.toBeUndefined()
    expect(command).toEqual({ type: 'applyComposerDraftChanges', changes })
  })
})

describe('provider auth and model catalog commands', () => {
  test('uses typed login id and clears no wire shape assumptions', async () => {
    const commands: unknown[] = []
    const client = { request: async (command: unknown) => { commands.push(command); return commands.length === 1 ? { type: 'login', phase: { type: 'awaitingApiKey', loginId: 'login-1', instructions: 'key' } } : { type: 'login', phase: { type: 'completed', loginId: 'login-1' } } } } as unknown as WakuClient
    await expect(startLogin(client, 'xai', 'apiKey')).resolves.toMatchObject({ type: 'awaitingApiKey', loginId: 'login-1' })
    await expect(completeApiKeyLogin(client, 'login-1', 'xai', 'secret')).resolves.toMatchObject({ type: 'completed', loginId: 'login-1' })
    expect(commands).toEqual([
      { type: 'startLogin', provider: 'xai', method: 'apiKey' },
      { type: 'completeApiKeyLogin', loginId: 'login-1', provider: 'xai', key: 'secret' },
    ])
  })

  test('returns catalog source and unsupported entries unchanged', async () => {
    const catalog = { provider: 'opencode-go', source: 'cache' as const, fetchedAtMs: 1, models: [{ id: 'google', name: 'Google', provider: 'opencode-go', apiFormat: 'openAiChat' as const, transport: 'standard' as const, baseUrl: 'https://example.test', contextWindow: 1, maxOutputTokens: 1, reasoning: false, capabilities: { serviceTier: false, reasoningEffort: false, reasoningSummary: false, sampling: false }, supported: false, unsupportedReason: 'googleFormat' as const }] }
    let command: unknown
    const client = { request: async (next: unknown) => { command = next; return { type: 'models', catalog } } } as unknown as WakuClient
    await expect(listModels(client, 'opencode-go')).resolves.toEqual(catalog)
    expect(command).toEqual({ type: 'listModels', provider: 'opencode-go' })
  })
})

describe('web auth and catalog guards', () => {
  test('ignores completed and failed phases when rendering active login controls', () => {
    const completed = { type: 'completed', loginId: 'done', provider: 'xai' } as const
    const failed = { type: 'failed', loginId: 'bad', provider: 'xai', message: 'nope' } as const
    expect(isActiveAuthPhase(completed)).toBe(false)
    expect(isActiveAuthPhase(failed)).toBe(false)
    expect(activeAuthPhase([completed, failed], 'xai')).toBeNull()
  })

  test('polls only while a browser or device phase is active', () => {
    const awaiting = { type: 'awaitingDevice', loginId: 'login', provider: 'xai-oauth', userCode: 'ABCD', verificationUrl: 'https://example.test', instructions: 'code' } as const
    const completed = { type: 'completed', loginId: 'login', provider: 'xai-oauth' } as const
    const apiKey = { type: 'awaitingApiKey', loginId: 'login', provider: 'xai', instructions: 'key' } as const
    expect(authStatusPollIntervalMs([awaiting])).toBe(1_000)
    expect(authStatusPollIntervalMs([completed])).toBe(false)
    expect(authStatusPollIntervalMs([apiKey])).toBe(false)
    expect(authStatusPollIntervalMs([])).toBe(false)
  })

  test('only allows secure external URLs and localhost HTTP fixtures', () => {
    expect(isAllowedExternalUrl('https://auth.example.test/callback')).toBe(true)
    expect(isAllowedExternalUrl('http://localhost:3000/callback')).toBe(true)
    expect(isAllowedExternalUrl('http://127.0.0.1:3000/callback')).toBe(true)
    expect(isAllowedExternalUrl('http://[::1]:3000/callback')).toBe(true)
    expect(isAllowedExternalUrl('http://example.test/callback')).toBe(false)
    expect(isAllowedExternalUrl('https://user:pass@example.test/callback')).toBe(false)
    expect(isAllowedExternalUrl('file:///tmp/callback')).toBe(false)
    expect(isAllowedExternalUrl('not a url')).toBe(false)
  })

  test('uses explicit model fallback only for custom endpoint errors', () => {
    const provider = {
      id: 'custom', name: 'Custom', baseUrl: 'https://example.test', apiFormat: 'openAiChat' as const,
      defaultModel: 'default', contextWindow: 1, maxOutputTokens: 1, models: ['manual-model'],
    }
    expect(explicitModelFallback('custom', provider, { isError: true, isFetched: true })).toHaveLength(1)
    expect(explicitModelFallback('openai-chat', provider, { isError: true, isFetched: true })).toHaveLength(0)
    expect(explicitModelFallback('custom', provider, { isError: false, isFetched: true, data: { provider: 'custom' } })).toHaveLength(0)
  })

  test('does not keep a Go model on a SuperGrok session', () => {
    const grok = { id: 'grok-4.5', name: 'Grok 4.5', supported: true, unsupportedReason: null, capabilities: { serviceTier: false, reasoningEffort: true, reasoningSummary: false, sampling: false }, apiFormat: 'openAiResponses' as const, source: 'live' as const }
    expect(resolvedSessionModel('kimi-k2.7-code', [grok], 'grok-4.5')).toBe('grok-4.5')
    expect(resolvedSessionModel('grok-4.5', [grok], 'grok-4.5')).toBe('grok-4.5')
    expect(resolvedSessionModel('kimi-k2.7-code', [], 'grok-4.5')).toBeUndefined()
  })
})

describe('beginTurn', () => {
  test('puts the submitted prompt in the transcript before runtime startup', () => {
    const draft = createSession('project', 'openai-codex', false)
    const active = beginTurn(draft, 'Build the feature')

    expect(active.status).toBe('connecting')
    expect(active.messages).toHaveLength(1)
    expect(active.messages[0]).toMatchObject({
      role: 'user',
      content: 'Build the feature',
      streaming: false,
    })
    expect(active.turns).toHaveLength(1)
    expect(draft.messages).toHaveLength(0)
  })
})

describe('browseDaemonDirectory', () => {
  test('lists an absolute directory on the daemon host', async () => {
    let command: unknown
    const result: DaemonDirectory = {
      type: 'directory',
      path: '/Users/me',
      parent: '/Users',
      home: '/Users/me',
      filesystem_root: '/',
      entries: [],
    }
    const client = {
      request: async (next: unknown) => {
        command = next
        return { type: 'workspace', result }
      },
    } as unknown as WakuClient

    await expect(browseDaemonDirectory(client, '/Users/me')).resolves.toEqual(result)
    expect(command).toEqual({
      type: 'workspace',
      operation: { type: 'browseDirectory', path: '/Users/me' },
    })
  })

  test('uses the daemon home when no path is provided', async () => {
    let command: unknown
    const result: DaemonDirectory = {
      type: 'directory',
      path: '/Users/me',
      parent: '/Users',
      home: '/Users/me',
      filesystem_root: '/',
      entries: [],
    }
    const client = {
      request: async (next: unknown) => {
        command = next
        return { type: 'workspace', result }
      },
    } as unknown as WakuClient

    await expect(browseDaemonDirectory(client, null)).resolves.toEqual(result)
    expect(command).toEqual({
      type: 'workspace',
      operation: { type: 'browseDirectory', path: null },
    })
  })
})

describe('turn checkpoints', () => {
  test('captures the immutable starting ref on the daemon host', async () => {
    let command: unknown
    const client = {
      request: async (next: unknown) => {
        command = next
        return { type: 'workspace', result: { type: 'ack' } }
      },
    } as unknown as WakuClient

    await expect(captureTurnStart(client, '/srv/wakuwaku', 'session', 2)).resolves.toBeUndefined()
    expect(command).toEqual({
      type: 'workspace',
      operation: {
        type: 'captureTurnStart',
        cwd: '/srv/wakuwaku',
        session_id: 'session',
        turn_count: 2,
      },
    })
  })

  test('returns the ending checkpoint captured by the daemon', async () => {
    let command: unknown
    const checkpoint = {
      turn_count: 2,
      git_ref: 'refs/wakuwaku/session-session-turn-2',
      status: 'ready' as const,
      files: [],
      additions: 0,
      deletions: 0,
      created_at: 1,
    }
    const client = {
      request: async (next: unknown) => {
        command = next
        return { type: 'workspace', result: { type: 'checkpoint', checkpoint } }
      },
    } as unknown as WakuClient

    await expect(captureTurnCheckpoint(client, '/srv/wakuwaku', 'session', 2))
      .resolves.toEqual(checkpoint)
    expect(command).toEqual({
      type: 'workspace',
      operation: {
        type: 'captureTurn',
        cwd: '/srv/wakuwaku',
        session_id: 'session',
        turn_count: 2,
      },
    })
  })
})

describe('writeWorkspaceTextFile', () => {
  test('writes the edited contents through the daemon workspace API', async () => {
    let command: unknown
    const client = {
      request: async (next: unknown) => {
        command = next
        return { type: 'workspace', result: { type: 'ack' } }
      },
    } as unknown as WakuClient

    await expect(
      writeWorkspaceTextFile(client, '/srv/wakuwaku', 'src/app.ts', 'export const ready = true\n'),
    ).resolves.toBeUndefined()
    expect(command).toEqual({
      type: 'workspace',
      operation: {
        type: 'writeTextFile',
        root: '/srv/wakuwaku',
        relative_path: 'src/app.ts',
        content: 'export const ready = true\n',
      },
    })
  })
})

describe('createProject', () => {
  test('normalizes a remote absolute path without collapsing the root', () => {
    expect(createProject('/').path).toBe('/')
    expect(createProject('/srv/wakuwaku/').path).toBe('/srv/wakuwaku')
    expect(createProject('/srv/wakuwaku/').name).toBe('wakuwaku')
  })

  test('rejects paths that depend on the browser process cwd', () => {
    expect(() => createProject('relative/project')).toThrow('absolute path')
  })
})

describe('persistProject', () => {
  test('adds a daemon-host project without creating a session', async () => {
    const existing = project('existing', 'existing', '/srv/existing')
    const candidate = project('new', 'wakuwaku', '/srv/wakuwaku')
    const commands: unknown[] = []
    const client = {
      request: async (command: unknown) => {
        commands.push(command)
        if ((command as { type: string }).type === 'loadTaskState') {
          return {
            type: 'taskState',
            projects: [existing],
            sessions: [{ id: 'session' }],
            defaultCwd: '/srv',
            projectlessRoot: '/srv/.wakuwaku/projects',
          }
        }
        return { type: 'taskStateSaved', sessions: [] }
      },
    } as unknown as WakuClient

    const result = await persistProject(client, candidate)

    expect(result.project).toEqual(candidate)
    expect(result.taskState.projects).toEqual([existing, candidate])
    expect(commands).toEqual([
      { type: 'loadTaskState' },
      {
        type: 'saveTaskState',
        projects: [existing, candidate],
        liveSessionIds: ['session'],
        sessions: [],
      },
    ])
  })

  test('reuses a project already persisted for the same daemon path', async () => {
    const existing = project('existing', 'wakuwaku', '/srv/wakuwaku')
    const commands: unknown[] = []
    const client = {
      request: async (command: unknown) => {
        commands.push(command)
        return {
          type: 'taskState',
          projects: [existing],
          sessions: [],
          defaultCwd: '/srv',
          projectlessRoot: '/srv/.wakuwaku/projects',
        }
      },
    } as unknown as WakuClient

    const result = await persistProject(client, project('duplicate', 'wakuwaku', '/srv/wakuwaku'))

    expect(result.project).toEqual(existing)
    expect(commands).toEqual([{ type: 'loadTaskState' }])
  })
})

describe('persistSession', () => {
  test('checkpoints one session without reloading or replacing the catalog', async () => {
    const saved = createSession('project', 'openai-codex', false)
    const commands: unknown[] = []
    const client = {
      request: async (command: unknown) => {
        commands.push(command)
        return { type: 'taskStateSaved', sessions: [saved] }
      },
    } as unknown as WakuClient

    await expect(persistSession(client, saved)).resolves.toEqual(saved)
    expect(commands).toEqual([{
      type: 'saveTaskState',
      projects: [],
      liveSessionIds: [saved.id],
      sessions: [saved],
    }])
  })
})

describe('selectableProjects', () => {
  test('represents projectless tasks as one choice while preserving the selected workspace', () => {
    const ordinary = project('repo', 'wakuwaku', '/srv/wakuwaku')
    const first = project('one', 'No project', '/home/me/.wakuwaku/projects/one')
    const selected = project('two', 'No project', '/home/me/.wakuwaku/projects/two')

    expect(selectableProjects([ordinary, first, selected], selected)).toEqual([
      selected,
      ordinary,
    ])
  })
})

describe('removeSession', () => {
  test('removes only the selected session through the daemon', async () => {
    const commands: unknown[] = []
    const client = {
      request: async (next: unknown) => {
        commands.push(next)
        if ((next as { type: string }).type === 'removeSession') {
          return { type: 'ack' }
        }
        if ((next as { type: string }).type === 'loadTaskState') {
          return {
            type: 'taskState',
            projects: [],
            sessions: [{ id: 'keep' }],
            defaultCwd: '/srv',
            projectlessRoot: '/srv/.wakuwaku/projects',
          }
        }
        throw new Error('unexpected command')
      },
    } as unknown as WakuClient

    const next = await removeSession(client, 'remove')

    expect(next.sessions.map((session) => session.id)).toEqual(['keep'])
    expect(commands).toEqual([
      { type: 'removeSession' },
      { type: 'loadTaskState' },
    ])
  })
})

function project(id: string, name: string, path: string): Project {
  return { id, name, path, created_at: 0 }
}
