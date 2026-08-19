import { useQueryClient } from '@tanstack/react-query'
import { Popover } from '@base-ui/react/popover'
import type {
  AgentSession,
  BranchSnapshot,
  ComposerDraft,
  MessageAttachment,
  ModelCatalogEntry,
  Project,
} from '@wakuwaku/client'
import {
  useEffect,
  useRef,
  useState,
  type KeyboardEvent,
  type MouseEvent as ReactMouseEvent,
  type RefObject,
} from 'react'
import { Virtuoso, type VirtuosoHandle } from 'react-virtuoso'
import { toast } from 'sonner'
import { ControlMenu, type ControlMenuItem } from '@/components/control-menu'
import { DaemonFilePicker } from '@/components/daemon-file-picker'
import { PreviewableImage } from '@/components/image-preview'
import { ModelPicker } from '@/components/model-picker'
import { Button } from '@/components/ui/button'
import { Textarea } from '@/components/ui/textarea'
import { FileTypeIcon, WakuIcon } from '@/components/waku-icon'
import {
  useComposerCommands,
  useComposerFiles,
  useDaemonSettings,
  useModelCatalog,
  useWorkspaceBranches,
} from '@/hooks/use-daemon-data'
import { importDaemonPathAttachment, importFiles, readAttachmentImage } from '@/lib/attachments'
import {
  checkoutWorkspaceBranch,
  daemonKeys,
  selectableProjects,
  sessionCwd,
} from '@/lib/daemon-api'
import { useDaemon } from '@/lib/daemon-context'
import { useI18n } from '@/lib/i18n'
import { canonicalReasoningEffortForModel } from '@/lib/reasoning-effort'
import {
  composerAutocompleteRows,
  detectComposerTrigger,
  expandedComposerSubmission,
  mergeComposerCommands,
  replaceComposerTrigger,
  type ComposerAutocompleteRow,
} from '@/lib/composer-autocomplete'
import {
  browserComposerPreferenceStorage,
  readComposerPreferences,
  rememberedModelTraits,
  selectedModelTraits,
  rememberComposerSession,
  writeComposerPreferences,
} from '@/lib/composer-preferences'
import {
  isEscapeStopArmed,
  pressEscapeStop,
  sameEscapeStopArm,
  type EscapeStopArm,
} from '@/lib/escape-stop'
import { type PendingUserInput } from '@/lib/event-reducer'
import { usePrimaryShortcut } from '@/lib/platform'
import { isProjectlessProject, projectDisplayName } from '@/lib/project-presentation'
import { useRuntime } from '@/lib/runtime-context'
import { sessionHasStarted } from '@/lib/sidebar-presentation'
import { fastModeEnabled, serviceTierForModel, toggleFastMode } from '@/lib/service-tier'
import { cn } from '@/lib/utils'

type Translator = (key: string, params?: Record<string, string | number>) => string

function preserveComposerFocusOnMouseDown(event: ReactMouseEvent<HTMLElement>) {
  // Portal events still bubble through the React tree. Only cancel the native
  // focus transfer for controls physically inside this footer, not menu rows.
  if (event.button === 0 && event.currentTarget.contains(event.target as Node)) {
    event.preventDefault()
  }
}

export function Composer({
  session,
  project,
  projects,
  draft = false,
  onDraftChange,
  onActivated,
  focusSignal,
  modelPickerSignal,
  usagePanelSignal,
  prefillSignal,
  prefillText,
  initialComposerDraft,
  onComposerDraftChange,
  onComposerDraftSubmitted,
  onAddProject,
  onProjectless,
  onFocusSignalHandled,
  onModelPickerSignalHandled,
  onUsagePanelSignalHandled,
  onPrefillSignalHandled,
}: {
  session: AgentSession
  project: Project
  projects: Project[]
  draft?: boolean
  onDraftChange?: (session: AgentSession) => void
  onActivated?: (session: AgentSession) => void
  focusSignal?: number
  modelPickerSignal?: number
  usagePanelSignal?: number
  prefillSignal?: number
  prefillText?: string
  initialComposerDraft?: ComposerDraft
  onComposerDraftChange?: (draft: ComposerDraft) => void
  onComposerDraftSubmitted?: () => void
  onAddProject?: () => void
  onProjectless?: () => void
  onFocusSignalHandled?: () => void
  onModelPickerSignalHandled?: () => void
  onUsagePanelSignalHandled?: () => void
  onPrefillSignalHandled?: () => void
}) {
  const { t } = useI18n()
  const { client, config } = useDaemon()
  const queryClient = useQueryClient()
  const {
    sendPrompt,
    steerPrompt,
    cancel,
    respond,
    respondUserInput,
    saveSession,
    removeQueuedMessage,
    permissions,
    userInputs,
    runtimes,
  } = useRuntime()
  const cwd = sessionCwd(session, project)
  const branches = useWorkspaceBranches(cwd)
  const composerFiles = useComposerFiles(cwd)
  const composerCommands = useComposerCommands(session.provider, cwd)
  const modelCatalog = useModelCatalog(session.provider)
  const daemonSettings = useDaemonSettings()
  const [prompt, setPrompt] = useState(initialComposerDraft?.text ?? '')
  const [attachments, setAttachments] = useState<MessageAttachment[]>(
    () => initialComposerDraft?.attachments ?? [],
  )
  const [submitting, setSubmitting] = useState(false)
  const [uploading, setUploading] = useState(false)
  const [filePickerOpen, setFilePickerOpen] = useState(false)
  const [branchPending, setBranchPending] = useState(false)
  const [inputFocused, setInputFocused] = useState(false)
  const [cursor, setCursor] = useState(() => (initialComposerDraft?.text ?? '').length)
  const [autocompleteSelection, setAutocompleteSelection] = useState({ key: '', index: 0 })
  const [dismissedAutocomplete, setDismissedAutocomplete] = useState<string | null>(null)
  const [escapeStopArm, setEscapeStopArm] = useState<EscapeStopArm | null>(null)
  const composerInput = useRef<HTMLTextAreaElement>(null)
  const autocompleteList = useRef<VirtuosoHandle>(null)
  const pendingCursor = useRef<number | null>(null)
  const escapeStopTimer = useRef<number | null>(null)
  const mounted = useRef(true)
  const draftChange = useRef(onComposerDraftChange)
  draftChange.current = onComposerDraftChange
  const busy = ['connecting', 'working', 'waiting'].includes(session.status)
  const runningTurnId = [...session.turns].reverse().find((turn) => turn.status === 'running')?.id
  const escapeStopTarget = `${session.id}:${runningTurnId ?? ''}`
  const escapeStopArmed = busy && isEscapeStopArmed(escapeStopArm, escapeStopTarget, Date.now())
  const permission = permissions[session.id]
  const userInput = userInputs[session.id]
  const runtime = runtimes[session.id]
  const hasDraft = Boolean(prompt.trim() || attachments.length)
  const canSteer = busy && session.status !== 'connecting' && runtime?.supportsSteer
  const workspace = session.workspace ?? { kind: 'local' as const }
  const projectChoices = selectableProjects(projects, project)
  const projectless = isProjectlessProject(project)
  const projectName = projectDisplayName(project, t('project.no_project_name'))
  const workspaceLocal = workspace.kind === 'local'
  const workspaceLabel = workspace.kind === 'newWorktree'
    ? t('workspace.new_worktree')
    : workspace.kind === 'worktree'
      ? workspace.branch
      : t('workspace.local')
  const availableCommands = mergeComposerCommands(
    composerCommands.data ?? [],
    [],
  )
  const autocompleteTrigger = inputFocused ? detectComposerTrigger(prompt, cursor) : null
  const autocompleteKey = autocompleteTrigger
    ? `${autocompleteTrigger.kind}:${autocompleteTrigger.start}:${autocompleteTrigger.end}:${autocompleteTrigger.query}`
    : null
  const autocompleteRows = autocompleteTrigger
    ? composerAutocompleteRows(
        autocompleteTrigger,
        availableCommands,
        composerFiles.data ?? [],
      )
    : []
  const autocompleteOpen = Boolean(
    autocompleteKey
      && autocompleteKey !== dismissedAutocomplete
      && autocompleteRows.length,
  )

  function clearEscapeStop() {
    if (escapeStopTimer.current !== null) window.clearTimeout(escapeStopTimer.current)
    escapeStopTimer.current = null
    setEscapeStopArm(null)
  }

  function stopTurn() {
    clearEscapeStop()
    void cancel(session.id).catch((error) => toast.error(errorMessage(error)))
  }

  useEffect(() => {
    if (!busy) {
      clearEscapeStop()
      return
    }
    setEscapeStopArm((current) => current?.target === escapeStopTarget ? current : null)
  }, [busy, escapeStopTarget])

  useEffect(() => {
    if (!busy) return
    const onEscape = (event: globalThis.KeyboardEvent) => {
      if (
        event.key !== 'Escape'
        || event.repeat
        || event.defaultPrevented
        || event.metaKey
        || event.ctrlKey
        || event.altKey
        || event.shiftKey
      ) return
      if (document.querySelector('[role="dialog"], [role="menu"], [role="listbox"]')) return

      event.preventDefault()
      const press = pressEscapeStop(escapeStopArm, escapeStopTarget, Date.now())
      if (press.type === 'stop') {
        stopTurn()
        return
      }

      if (escapeStopTimer.current !== null) window.clearTimeout(escapeStopTimer.current)
      setEscapeStopArm(press.arm)
      escapeStopTimer.current = window.setTimeout(() => {
        setEscapeStopArm((current) => sameEscapeStopArm(current, press.arm) ? null : current)
        escapeStopTimer.current = null
      }, Math.max(0, press.arm.expiresAt - Date.now()))
    }
    window.addEventListener('keydown', onEscape)
    return () => window.removeEventListener('keydown', onEscape)
  }, [busy, escapeStopArm, escapeStopTarget])

  useEffect(() => () => {
    if (escapeStopTimer.current !== null) window.clearTimeout(escapeStopTimer.current)
  }, [])
  const autocompleteHighlight = autocompleteSelection.key === autocompleteKey
    ? Math.min(autocompleteSelection.index, Math.max(0, autocompleteRows.length - 1))
    : 0

  useEffect(() => {
    if (focusSignal) composerInput.current?.focus()
    if (focusSignal) onFocusSignalHandled?.()
  }, [focusSignal, onFocusSignalHandled])

  useEffect(() => {
    if (!prefillSignal) return
    const nextPrompt = prefillText ?? ''
    pendingCursor.current = nextPrompt.length
    setPrompt(nextPrompt)
    composerInput.current?.focus()
    onPrefillSignalHandled?.()
  }, [onPrefillSignalHandled, prefillSignal, prefillText])

  useEffect(() => {
    setFilePickerOpen(false)
    setDismissedAutocomplete(null)
    setAutocompleteSelection({ key: '', index: 0 })
  }, [cwd, session.id])

  useEffect(() => {
    const nextCursor = pendingCursor.current
    if (nextCursor === null) return
    pendingCursor.current = null
    composerInput.current?.focus()
    composerInput.current?.setSelectionRange(nextCursor, nextCursor)
    setCursor(nextCursor)
  }, [prompt])

  useEffect(() => {
    mounted.current = true
    return () => {
      mounted.current = false
    }
  }, [])

  useEffect(() => {
    draftChange.current?.({ text: prompt, attachments })
  }, [attachments, prompt])

  async function activateDraft() {
    if (!draft) return session
    return saveSession(session, project)
  }

  function providerPromptOverride(
    submittedPrompt: string,
    submittedAttachments: MessageAttachment[],
  ): string | undefined {
    const expanded = expandedComposerSubmission(submittedPrompt.trim(), availableCommands)
    if (expanded === null) return undefined
    return [
      expanded,
      submittedAttachments.map((attachment) => `@${attachment.mention}`).join(' '),
    ].filter(Boolean).join(' ')
  }

  async function submit() {
    if (submitting || (!prompt.trim() && attachments.length === 0)) return
    const submittedPrompt = prompt
    const submittedAttachments = attachments
    let cleared = false
    setSubmitting(true)
    try {
      const target = await activateDraft()
      const pending = sendPrompt(
        target,
        submittedPrompt,
        submittedAttachments,
        providerPromptOverride(submittedPrompt, submittedAttachments),
      )
      onComposerDraftSubmitted?.()
      setPrompt('')
      setCursor(0)
      setDismissedAutocomplete(null)
      setAttachments([])
      cleared = true
      if (draft) {
        const optimistic = config
          ? queryClient.getQueryData<AgentSession>(daemonKeys.session(config.address, target.id))
          : undefined
        onActivated?.(optimistic ?? target)
      }
      await pending
    } catch (error) {
      if (cleared && mounted.current) {
        setPrompt((current) => current || submittedPrompt)
        setAttachments((current) => current.length ? current : submittedAttachments)
      }
      toast.error(errorMessage(error))
    } finally {
      setSubmitting(false)
    }
  }

  async function steer() {
    if (!hasDraft || !canSteer) return
    const submittedPrompt = prompt
    const submittedAttachments = attachments
    let cleared = false
    setSubmitting(true)
    try {
      const pending = steerPrompt(
        session,
        submittedPrompt,
        submittedAttachments,
        providerPromptOverride(submittedPrompt, submittedAttachments),
      )
      onComposerDraftSubmitted?.()
      setPrompt('')
      setCursor(0)
      setDismissedAutocomplete(null)
      setAttachments([])
      cleared = true
      await pending
    } catch (error) {
      if (cleared && mounted.current) {
        setPrompt((current) => current || submittedPrompt)
        setAttachments((current) => current.length ? current : submittedAttachments)
      }
      toast.error(errorMessage(error))
    } finally {
      setSubmitting(false)
    }
  }

  async function addFiles(files: File[]) {
    if (!client || !files.length) return
    setUploading(true)
    try {
      const imported = await importFiles(client, files)
      if (!mounted.current) return
      setAttachments((current) => [...current, ...imported])
    } catch (error) {
      toast.error(errorMessage(error))
    } finally {
      setUploading(false)
    }
  }

  async function addDaemonFile(path: string): Promise<boolean> {
    if (!client) return false
    if (attachments.some((attachment) => attachment.mention === path)) return true
    setUploading(true)
    try {
      const imported = await importDaemonPathAttachment(client, path)
      if (!mounted.current) return false
      setAttachments((current) => current.some((attachment) => attachment.mention === imported.mention)
        ? current
        : [...current, imported])
      return true
    } catch (error) {
      toast.error(errorMessage(error))
      return false
    } finally {
      setUploading(false)
    }
  }
  function savePatch(
    patch: Partial<AgentSession>,
    selectedModel?: Pick<ModelCatalogEntry, 'supported' | 'capabilities' | 'apiFormat' | 'reasoningEfforts' | 'defaultReasoningEffort'>,
  ) {
    const candidate = { ...session, ...patch }
    const model = selectedModel
      ?? modelCatalog.data?.models.find((entry) => entry.id === candidate.model)
    const next = {
      ...candidate,
      service_tier: serviceTierForModel(model, candidate.service_tier),
      reasoning_effort: canonicalReasoningEffortForModel(model, candidate.reasoning_effort),
      updated_at: Math.floor(Date.now() / 1_000),
    }
    if (config && next.model) {
      const storage = browserComposerPreferenceStorage()
      const preferences = rememberComposerSession(
        readComposerPreferences(storage, config.address),
        next,
      )
      writeComposerPreferences(storage, config.address, preferences)
    }
    if (draft) onDraftChange?.(next)
    else void saveSession(next).catch((error) => toast.error(errorMessage(error)))
  }

  function acceptAutocomplete(index = autocompleteHighlight) {
    if (!autocompleteTrigger) return
    const row = autocompleteRows[index]
    if (!row) return
    const replacement = replaceComposerTrigger(prompt, autocompleteTrigger, row)
    pendingCursor.current = replacement.cursor
    setPrompt(replacement.text)
    setDismissedAutocomplete(null)
    setAutocompleteSelection({ key: '', index: 0 })
  }

  function moveAutocomplete(direction: -1 | 1) {
    if (!autocompleteKey || !autocompleteRows.length) return
    const next = (autocompleteHighlight + direction + autocompleteRows.length)
      % autocompleteRows.length
    setAutocompleteSelection({ key: autocompleteKey, index: next })
    autocompleteList.current?.scrollToIndex({ index: next, align: 'center' })
  }

  function keyDown(event: KeyboardEvent<HTMLTextAreaElement>) {
    if (autocompleteOpen) {
      if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
        event.preventDefault()
        moveAutocomplete(event.key === 'ArrowDown' ? 1 : -1)
        return
      }
      if (
        (event.key === 'Enter' && !event.shiftKey && !event.metaKey && !event.ctrlKey && !event.altKey)
        || (event.key === 'Tab' && !event.shiftKey)
      ) {
        event.preventDefault()
        acceptAutocomplete()
        return
      }
      if (event.key === 'Escape') {
        event.preventDefault()
        setDismissedAutocomplete(autocompleteKey)
        return
      }
    }
    if (event.key !== 'Enter' || event.shiftKey || event.nativeEvent.isComposing) return
    event.preventDefault()
    if ((event.metaKey || event.ctrlKey) && canSteer) void steer()
    else void submit()
  }

  async function switchBranch(branch: string, create = false) {
    if (!client || !config || !branch || branchPending) return
    if (!create && workspace.kind === 'newWorktree') {
      savePatch({ workspace: { kind: 'newWorktree', baseBranch: branch } })
      return
    }
    setBranchPending(true)
    try {
      await checkoutWorkspaceBranch(client, cwd, branch, create)
      await queryClient.invalidateQueries({
        queryKey: daemonKeys.workspace(config.address, cwd),
      })
    } catch (error) {
      toast.error(errorMessage(error))
    } finally {
      setBranchPending(false)
    }
  }

  return (
    <div className="shrink-0 px-3 pb-2 sm:px-5">
      <div className="mx-auto w-full max-w-[720px]">
        {permission && !userInput && (
          <section className="mb-2 rounded-xl border border-[color:var(--warning)]/30 bg-card p-3 shadow-lg">
            <div className="text-[13px] font-medium">{permission.title}</div>
            {permission.detail && (
              <p className="mt-1 max-h-24 overflow-auto whitespace-pre-wrap text-xs leading-5 text-[var(--text-tertiary)]">
                {permission.detail}
              </p>
            )}
            <div className="mt-3 flex flex-wrap justify-end gap-2">
              {permission.options.map((option) => (
                <Button
                  key={option.id}
                  size="sm"
                  variant={option.allow ? 'default' : 'outline'}
                  onClick={() => {
                    void respond(session.id, permission.requestId, option.id).catch((error) =>
                      toast.error(errorMessage(error)),
                    )
                  }}
                >
                  {option.allow && <WakuIcon name="check" />}
                  {option.label}
                </Button>
              ))}
            </div>
          </section>
        )}

        {userInput && (
          <UserInputPanel
            input={userInput}
            onSubmit={(answers) => respondUserInput(
              session.id,
              userInput.requestId,
              answers,
            )}
          />
        )}

        <QueuedMessages
          canSteer={Boolean(canSteer)}
          session={session}
          onEdit={(message) => {
            setPrompt(message.display_content ?? message.content)
            setAttachments(message.attachments ?? [])
            void removeQueuedMessage(session.id, message.id).catch((error) =>
              toast.error(errorMessage(error)),
            )
          }}
          onRemove={(messageId) =>
            void removeQueuedMessage(session.id, messageId).catch((error) =>
              toast.error(errorMessage(error)),
            )
          }
          onSteer={(message) => {
            void removeQueuedMessage(session.id, message.id).catch((error) =>
              toast.error(errorMessage(error)),
            )
            void steerPrompt(
              session,
              message.display_content ?? message.content,
              message.attachments ?? [],
              message.content,
            ).catch((error) => toast.error(errorMessage(error)))
          }}
        />

        <div className="relative">
          {autocompleteOpen && (
            <ComposerAutocomplete
              highlight={autocompleteHighlight}
              listRef={autocompleteList}
              rows={autocompleteRows}
              t={t}
              onAccept={acceptAutocomplete}
              onHighlight={(index) => {
                if (autocompleteKey) {
                  setAutocompleteSelection({ key: autocompleteKey, index })
                }
              }}
            />
          )}
          <section
            className="rounded-[13px] border bg-card p-2.5 focus-within:border-input"
            onDragOver={(event) => {
              event.preventDefault()
              event.dataTransfer.dropEffect = 'copy'
            }}
            onDrop={(event) => {
              event.preventDefault()
              void addFiles([...event.dataTransfer.files])
            }}
          >
          {attachments.length > 0 && (
            <div className="flex flex-wrap gap-2 px-1 pb-2 pt-0.5">
              {attachments.map((attachment, index) => (
                <ComposerAttachmentTile
                  attachment={attachment}
                  key={attachment.blob_reference ?? `${attachment.mention}-${index}`}
                  t={t}
                  onRemove={() => setAttachments((current) => current.filter((_, item) => item !== index))}
                />
              ))}
            </div>
          )}
            <Textarea
              aria-controls={autocompleteOpen ? 'composer-autocomplete' : undefined}
              aria-expanded={autocompleteOpen}
              aria-label={t('composer.message')}
              aria-activedescendant={autocompleteOpen
                ? `composer-autocomplete-${autocompleteHighlight}`
                : undefined}
              aria-autocomplete="list"
              className="max-h-48 min-h-[46px] resize-none border-0 bg-transparent px-1 pb-1 pt-0 text-[14px] leading-5 shadow-none focus-visible:ring-0"
              placeholder={t(busy ? 'composer.queue_placeholder' : 'composer.prompt_placeholder')}
              ref={composerInput}
              role="combobox"
              value={prompt}
              onBlur={() => setInputFocused(false)}
              onChange={(event) => {
                setPrompt(event.target.value)
                setCursor(event.target.selectionStart)
              }}
              onClick={(event) => setCursor(event.currentTarget.selectionStart)}
              onFocus={() => setInputFocused(true)}
              onKeyDown={keyDown}
              onSelect={(event) => setCursor(event.currentTarget.selectionStart)}
            />
          <div
            className="mt-2 flex min-w-0 items-center gap-1 pb-px text-[11.5px] leading-[14px]"
            onMouseDown={preserveComposerFocusOnMouseDown}
          >
            <ModelPicker
              openSignal={modelPickerSignal}
              onOpenSignalHandled={onModelPickerSignalHandled}
              returnFocus={composerInput}
              session={session}
              onChange={(provider, model) => {
                const preferences = readComposerPreferences(
                  browserComposerPreferenceStorage(),
                  config?.address ?? 'disconnected',
                )
                const remembered = rememberedModelTraits(preferences, provider, model.id)
                const traits = selectedModelTraits(model, remembered)
                savePatch({
                  provider,
                  model: model.id,
                  reasoning_effort: traits.reasoningEffort,
                  service_tier: traits.serviceTier,
                  context_window: traits.contextWindow,
                }, model)
              }}
            />
            <ReasoningEffortControl
              model={modelCatalog.data?.models.find((candidate) => candidate.id === session.model)}
              session={session}
              onPatch={savePatch}
            />
            <FastModeControl
              model={modelCatalog.data?.models.find((candidate) => candidate.id === session.model)}
              session={session}
              onPatch={savePatch}
            />
            <AccessControl returnFocus={composerInput} session={session} onPatch={savePatch} />
            <InteractionModeControl session={session} onPatch={savePatch} />
            <div className="flex-1" />
            <div className="flex items-center gap-2">
              {busy && (
                <Button
                  aria-label={t(escapeStopArmed ? 'composer.stop_confirm' : 'composer.stop')}
                  className="rounded-full"
                  size="icon-sm"
                  variant="secondary"
                  onClick={stopTurn}
                >
                  {escapeStopArmed
                    ? <span className="text-[10px] font-semibold">Esc</span>
                    : <WakuIcon className="size-[18px]" name="stopFilled" />}
                </Button>
              )}
              <Button
                aria-expanded={filePickerOpen}
                aria-haspopup="dialog"
                aria-label={t('composer.attach_daemon')}
                className="size-6 rounded-md text-[var(--text-secondary)]"
                disabled={!client || uploading}
                size="icon-sm"
                title={t('composer.attach_daemon_title')}
                type="button"
                variant="ghost"
                onClick={() => setFilePickerOpen(true)}
              >
                <WakuIcon className="size-[14px]" name="paperclip" />
              </Button>
              {busy ? (
                hasDraft && (
                  <Button
                    aria-label={t('composer.queue_followup')}
                    className="rounded-full"
                    disabled={submitting || uploading}
                    size="icon-sm"
                    onClick={() => void submit()}
                  >
                    <WakuIcon name="arrowUp" />
                  </Button>
                )
              ) : (
                <Button
                  aria-label={t('common.send')}
                  className="rounded-full"
                  disabled={submitting || uploading || !hasDraft}
                  size="icon-sm"
                  onClick={() => void submit()}
                >
                  <WakuIcon name="arrowUp" />
                </Button>
              )}
            </div>
            </div>
          </section>
        </div>

        {filePickerOpen && (
          <DaemonFilePicker
            root={cwd}
            returnFocus={composerInput}
            workspaceLabel={projectName}
            onClose={() => setFilePickerOpen(false)}
            onSelect={addDaemonFile}
          />
        )}

        <div
          className="flex h-8 min-w-0 items-center gap-1 px-2 text-[11px] text-[var(--text-tertiary)]"
          onMouseDown={preserveComposerFocusOnMouseDown}
        >
          <ControlMenu
            caret={false}
            disabled={busy || sessionHasStarted(session)}
            icon="folder"
            items={[
              ...projectChoices
                .filter((item) => item.name !== 'No project')
                .map((item) => ({
                  id: item.id,
                  label: item.name,
                  selected: item.id === session.project_id,
                  onSelect: () => savePatch({ project_id: item.id, workspace: { kind: 'local' } }),
                })),
              ...(onAddProject ? [{
                id: 'new-project',
                label: t('project.new_project'),
                icon: 'folderNew' as const,
                onSelect: onAddProject,
              }] : []),
              ...(onProjectless ? [{
                id: 'projectless',
                label: t('project.no_project'),
                icon: 'x' as const,
                selected: projectless,
                onSelect: projectless ? () => {} : onProjectless,
              }] : []),
            ]}
            label={projectless ? t('project.choose_project') : projectName}
            menuClassName="w-56"
            returnFocus={composerInput}
            triggerClassName="h-6 max-w-36 px-1.5 text-[11px]"
          />
          <ControlMenu
            caret={false}
            disabled={busy || sessionHasStarted(session)}
            icon={workspaceLocal ? 'laptop' : 'fork'}
            items={[
              { id: 'local', section: t('workspace.work_in'), label: t('workspace.local'), icon: 'laptop', selected: workspaceLocal, onSelect: () => savePatch({ workspace: { kind: 'local' } }) },
              { id: 'newWorktree', section: t('workspace.work_in'), label: t('workspace.new_worktree'), icon: 'fork', disabled: projectless, selected: !workspaceLocal, onSelect: () => savePatch({ workspace: { kind: 'newWorktree' } }) },
            ]}
            label={workspaceLabel}
            returnFocus={composerInput}
            triggerClassName="h-6 max-w-32 px-1.5 text-[11px]"
          />
          {!projectless && branches.data && (
            <BranchPicker
              disabled={busy || branchPending}
              pending={branchPending}
              returnFocus={composerInput}
              snapshot={branches.data}
              workspace={workspace}
              onCreate={(branch) => void switchBranch(branch, true)}
              onRefresh={() => void branches.refetch()}
              onSelect={(branch) => void switchBranch(branch)}
            />
          )}
          <div className="flex-1" />
          {runtime?.starting && <span>{t('composer.starting_agent')}</span>}
          <UsageMeter
            openSignal={usagePanelSignal}
            returnFocus={composerInput}
            session={session}
            onOpenSignalHandled={onUsagePanelSignalHandled}
          />
        </div>
      </div>
    </div>
  )
}

function UserInputPanel({
  input,
  onSubmit,
}: {
  input: PendingUserInput
  onSubmit: (answers: Array<{ questionId: string; answers: string[] }>) => Promise<void>
}) {
  const { t } = useI18n()
  const [questionIndex, setQuestionIndex] = useState(0)
  const [selections, setSelections] = useState<Record<string, string[]>>({})
  const [customAnswers, setCustomAnswers] = useState<Record<string, string>>({})
  const [submitting, setSubmitting] = useState(false)

  useEffect(() => {
    setQuestionIndex(0)
    setSelections({})
    setCustomAnswers({})
    setSubmitting(false)
  }, [input.requestId])

  const question = input.questions[questionIndex]
  if (!question) return null
  const selected = selections[question.id] ?? []
  const custom = customAnswers[question.id] ?? ''
  const canContinue = Boolean(custom.trim() || selected.length)
  const last = questionIndex + 1 === input.questions.length

  function select(label: string) {
    setCustomAnswers((current) => ({ ...current, [question.id]: '' }))
    setSelections((current) => {
      const previous = current[question.id] ?? []
      return {
        ...current,
        [question.id]: question.multiSelect
          ? previous.includes(label)
            ? previous.filter((answer) => answer !== label)
            : [...previous, label]
          : [label],
      }
    })
  }

  function setCustom(value: string) {
    setCustomAnswers((current) => ({ ...current, [question.id]: value }))
    if (value.trim()) {
      setSelections((current) => ({ ...current, [question.id]: [] }))
    }
  }

  async function advance() {
    if (!canContinue || submitting) return
    if (!last) {
      setQuestionIndex((current) => current + 1)
      return
    }
    setSubmitting(true)
    try {
      await onSubmit(input.questions.map((item) => {
        const custom = customAnswers[item.id]?.trim()
        return {
          questionId: item.id,
          answers: custom ? [custom] : selections[item.id] ?? [],
        }
      }))
    } catch (error) {
      toast.error(errorMessage(error))
      setSubmitting(false)
    }
  }

  return (
    <section className="mb-2 w-full rounded-[13px] border bg-card px-3.5 pb-2.5 pt-3">
      <div className="flex items-center gap-2">
        <div className="text-[11px] font-semibold text-[var(--text-tertiary)]">
          {question.header}
        </div>
        {input.questions.length > 1 && (
          <div className="flex h-[18px] items-center rounded-[5px] bg-muted/50 px-1.5 text-[10px] font-medium tabular-nums text-[var(--text-tertiary)]">
            {t('user_input.progress', {
              current: questionIndex + 1,
              total: input.questions.length,
            })}
          </div>
        )}
      </div>
      <p className="mt-1.5 whitespace-pre-wrap text-[13px] font-medium leading-[18px]">
        {question.question}
      </p>
      {!!question.options.length && (
        <div className="mt-2.5 grid gap-1" role={question.multiSelect ? 'group' : 'radiogroup'}>
          {question.options.map((option) => {
            const checked = selected.includes(option.label)
            return (
              <button
                aria-checked={checked}
                className={cn(
                  'flex min-h-9 w-full items-center gap-2 rounded-lg border border-transparent bg-muted/40 px-2.5 py-1.5 text-left outline-none transition-colors hover:border-border hover:bg-muted/60 focus-visible:border-ring focus-visible:ring-1 focus-visible:ring-ring/30',
                  checked && 'border-primary/35 bg-primary/[0.08]',
                )}
                key={option.label}
                role={question.multiSelect ? 'checkbox' : 'radio'}
                type="button"
                onClick={() => select(option.label)}
              >
                <span className="min-w-0 flex-1">
                  <span className="block text-[12px] font-medium">{option.label}</span>
                  {option.description && option.description !== option.label && (
                    <span className="mt-0.5 block text-[10.5px] leading-[15px] text-[var(--text-tertiary)]">
                      {option.description}
                    </span>
                  )}
                </span>
                {checked && <WakuIcon className="size-3 shrink-0 text-primary" name="check" />}
              </button>
            )
          })}
        </div>
      )}
      <div
        className={cn(
          'mt-1 flex h-[34px] items-center gap-2 rounded-lg border border-transparent bg-muted/40 px-2.5 focus-within:border-ring',
          custom.trim() && 'border-primary/35 bg-primary/[0.06]',
        )}
      >
        <WakuIcon
          className={cn('size-3 shrink-0 text-[var(--text-ghost)]', custom.trim() && 'text-primary')}
          name="pencil"
        />
        <input
          className="min-w-0 flex-1 bg-transparent text-[12px] outline-none placeholder:text-[var(--text-ghost)]"
          placeholder={t('user_input.other_placeholder')}
          value={custom}
          onChange={(event) => setCustom(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === 'Enter' && !event.nativeEvent.isComposing) {
              event.preventDefault()
              void advance()
            }
          }}
        />
      </div>
      <div className="mt-2 flex items-center gap-2">
        {questionIndex > 0 && (
          <Button
            className="h-7 px-2 text-[11px]"
            size="sm"
            variant="ghost"
            onClick={() => setQuestionIndex((value) => value - 1)}
          >
            {t('user_input.back')}
          </Button>
        )}
        <div className="flex-1" />
        <Button
          className="h-7 px-2.5 text-[11px]"
          disabled={!canContinue || submitting}
          size="sm"
          onClick={() => void advance()}
        >
          {last ? t('user_input.submit') : t('user_input.next')}
        </Button>
      </div>
    </section>
  )
}

function ComposerAutocomplete({
  rows,
  highlight,
  listRef,
  t,
  onAccept,
  onHighlight,
}: {
  rows: ComposerAutocompleteRow[]
  highlight: number
  listRef: RefObject<VirtuosoHandle | null>
  t: Translator
  onAccept: (index: number) => void
  onHighlight: (index: number) => void
}) {
  return (
    <div
      aria-label={t('composer.suggestions')}
      className="waku-popover-surface absolute bottom-[calc(100%+6px)] left-0 z-[70] w-full overflow-hidden rounded-[11px] p-1"
      id="composer-autocomplete"
      role="listbox"
      style={{ height: Math.min(302, rows.length * 30 + 8) }}
    >
      <Virtuoso
        className="size-full outline-none"
        computeItemKey={(_, row) => row.kind === 'command'
          ? `command:${row.command.scope}:${row.command.name}`
          : `file:${row.file.path}`}
        data={rows}
        fixedItemHeight={30}
        increaseViewportBy={90}
        ref={listRef}
        itemContent={(index, row) => (
          <button
            aria-selected={highlight === index}
            className={cn(
              'flex h-[30px] w-full items-center gap-2 rounded-md px-2 text-left outline-none',
              highlight === index ? 'bg-accent' : 'hover:bg-accent/70',
            )}
            id={`composer-autocomplete-${index}`}
            role="option"
            tabIndex={-1}
            type="button"
            onMouseDown={(event) => {
              event.preventDefault()
              onAccept(index)
            }}
            onMouseEnter={() => onHighlight(index)}
          >
            <AutocompleteRowContents row={row} />
          </button>
        )}
      />
    </div>
  )
}

function AutocompleteRowContents({ row }: { row: ComposerAutocompleteRow }) {
  if (row.kind === 'command') {
    const { command } = row
    return (
      <>
        {command.scope === 'Skill'
          ? <WakuIcon className="size-3 text-[var(--text-tertiary)]" name="sparkle" />
          : <WakuIcon className="size-3 text-[var(--text-tertiary)]" name="command" />}
        <span className="max-w-[260px] shrink-0 truncate text-[12px] font-medium">
          /{command.name}
        </span>
        {command.argument_hint && (
          <span className="shrink-0 text-[11px] text-[var(--text-ghost)]">
            {command.argument_hint}
          </span>
        )}
        <span className="min-w-0 flex-1 truncate text-[11px] text-[var(--text-tertiary)]">
          {command.description}
        </span>
        <span className="flex h-4 shrink-0 items-center rounded border px-1.5 text-[9px] font-semibold text-[var(--text-tertiary)]">
          {command.scope}
        </span>
      </>
    )
  }

  const trimmedLength = row.file.path.replace(/\/+$/u, '').length
  const nameStart = row.file.path.lastIndexOf('/', Math.max(0, trimmedLength - 1)) + 1
  const name = row.file.path.slice(nameStart)
  const parent = row.file.path.slice(0, Math.max(0, nameStart - 1))
  return (
    <>
      {row.file.is_dir
        ? <WakuIcon className="size-[13px] text-[var(--text-tertiary)]" name="folder" />
        : <FileTypeIcon className="size-[13px]" path={row.file.path} />}
      <span className="max-w-[300px] shrink-0 truncate text-[12px]">{name}</span>
      {parent && (
        <span className="min-w-0 flex-1 truncate text-[11px] text-[var(--text-ghost)]">
          {parent}
        </span>
      )}
    </>
  )
}

function ComposerAttachmentTile({
  attachment,
  onRemove,
  t,
}: {
  attachment: MessageAttachment
  onRemove: () => void
  t: Translator
}) {
  const { client, config, phase } = useDaemon()
  const [source, setSource] = useState<string | null>(null)

  useEffect(() => {
    if (!attachment.is_image || phase !== 'connected' || !client || !config) {
      setSource(null)
      return
    }
    let active = true
    void readAttachmentImage(client, attachment)
      .then((value) => active && setSource(value))
      .catch(() => active && setSource(null))
    return () => { active = false }
  }, [attachment.blob_reference, attachment.is_image, attachment.name, attachment.path, client, config?.address, phase])

  const contents = attachment.is_image && source ? (
    <PreviewableImage
      buttonClassName="size-full"
      imageClassName="size-full object-cover"
      name={attachment.name}
      source={source}
    />
  ) : (
    <div className="flex size-full flex-col items-center justify-center gap-[5px] px-[5px]">
      {attachment.is_dir
        ? <WakuIcon className="size-4 text-[var(--text-tertiary)]" name="folder" />
        : <FileTypeIcon className="size-4" path={attachment.mention || attachment.name} />}
      {!attachment.is_image && (
        <span className="w-full truncate text-center text-[8.5px] text-[var(--text-tertiary)]">
          {attachment.name}
        </span>
      )}
    </div>
  )

  return (
    <div
      className="relative size-16 overflow-hidden rounded-lg border bg-[var(--inset)] outline-none focus-within:border-ring"
      title={`@${attachment.mention}`}
    >
      {contents}
      <button
        aria-label={t('composer.remove_attachment', { name: attachment.name })}
        className="absolute right-[3px] top-[3px] z-10 grid size-4 place-items-center rounded-[5px] bg-background/80 text-[var(--text-secondary)] outline-none hover:bg-background focus-visible:ring-1 focus-visible:ring-ring"
        type="button"
        onClick={onRemove}
        onMouseDown={(event) => event.preventDefault()}
      >
        <WakuIcon className="size-[9px]" name="x" />
      </button>
    </div>
  )
}

const ACCESS_MODES: Array<{
  id: AgentSession['runtime_mode']
  labelKey: string
  descriptionKey: string
  icon: 'lock' | 'pencil' | 'lockOpen'
}> = [
  { id: 'ask', labelKey: 'mode.supervised', descriptionKey: 'mode.supervised_description', icon: 'lock' },
  { id: 'autoAcceptEdits', labelKey: 'mode.auto_accept_edits', descriptionKey: 'mode.auto_accept_edits_description', icon: 'pencil' },
  { id: 'fullAccess', labelKey: 'mode.full_access', descriptionKey: 'mode.full_access_description', icon: 'lockOpen' },
]

function AccessControl({
  session,
  onPatch,
  returnFocus,
}: {
  session: AgentSession
  onPatch: (patch: Partial<AgentSession>) => void
  returnFocus: RefObject<HTMLElement | null>
}) {
  const { t } = useI18n()
  const selected = ACCESS_MODES.find((mode) => mode.id === session.runtime_mode) ?? ACCESS_MODES[0]!
  return (
    <ControlMenu
      caret={false}
      icon={selected.icon}
      items={ACCESS_MODES.map((mode) => ({
        id: mode.id,
        icon: mode.icon,
        label: t(mode.labelKey),
        description: t(mode.descriptionKey),
        selected: mode.id === selected.id,
        onSelect: () => onPatch({ runtime_mode: mode.id }),
      }))}
      label={t(selected.labelKey)}
      menuClassName="w-[304px]"
      returnFocus={returnFocus}
    />
  )
}

function InteractionModeControl({
  session,
  onPatch,
}: {
  session: AgentSession
  onPatch: (patch: Partial<AgentSession>) => void
}) {
  const { t } = useI18n()
  const plan = session.interaction_mode === 'plan'
  return (
    <button
      aria-label={t('mode.switch_to', { mode: t(plan ? 'mode.build' : 'mode.plan') })}
      className={cn(
        'flex h-6 shrink-0 items-center gap-1.5 rounded-md px-[7px] text-[11.5px] text-[var(--text-secondary)] outline-none focus-visible:ring-1 focus-visible:ring-ring',
        plan && 'text-ring',
        'hover:bg-accent',
      )}
      type="button"
      onClick={() => onPatch({ interaction_mode: plan ? 'build' : 'plan' })}
    >
      <WakuIcon className={cn('size-[10.5px] text-[var(--text-tertiary)]', plan && 'text-ring')} name={plan ? 'list' : 'wrench'} />
      {t(plan ? 'mode.plan' : 'mode.build')}
    </button>
  )
}

function ReasoningEffortControl({
  model,
  session,
  onPatch,
}: {
  model: ModelCatalogEntry | undefined
  session: AgentSession
  onPatch: (patch: Partial<AgentSession>) => void
}) {
  const { t } = useI18n()
  const efforts = model?.reasoningEfforts ?? []
  if (!model?.supported || !model.capabilities.reasoningEffort || !efforts.length) return null
  const selected = efforts.some((effort) => effort.id === session.reasoning_effort)
    ? session.reasoning_effort!
    : model.defaultReasoningEffort && efforts.some((effort) => effort.id === model.defaultReasoningEffort)
      ? model.defaultReasoningEffort
      : efforts[0]!.id
  const selectedLabel = efforts.find((effort) => effort.id === selected)?.label ?? selected
  return (
    <ControlMenu
      icon="sparkle"
      items={efforts.map((effort) => ({
        id: effort.id,
        label: effort.label,
        selected: effort.id === selected,
        onSelect: () => onPatch({ reasoning_effort: effort.id }),
      }))}
      label={t('models.reasoning')}
      menuClassName="w-[220px]"
      selectionMode="choice"
    >
      <span className="truncate">{selectedLabel}</span>
    </ControlMenu>
  )
}

function FastModeControl({
  model,
  session,
  onPatch,
}: {
  model: ModelCatalogEntry | undefined
  session: AgentSession
  onPatch: (patch: Partial<AgentSession>) => void
}) {
  const { t } = useI18n()
  if (!model?.supported || !model.capabilities.serviceTier) return null
  const enabled = fastModeEnabled(model, session.service_tier)
  return (
    <button
      aria-pressed={enabled}
      className={cn(
        'flex h-6 shrink-0 items-center gap-1.5 rounded-md px-[7px] text-[11.5px] text-[var(--text-secondary)] outline-none hover:bg-accent focus-visible:ring-1 focus-visible:ring-ring',
        enabled && 'bg-accent text-ring',
      )}
      type="button"
      onClick={() => onPatch({ service_tier: toggleFastMode(model, session.service_tier) })}
    >
      <WakuIcon className={cn('size-[10.5px] text-[var(--text-tertiary)]', enabled && 'text-ring')} name="zap" />
      {t('models.fast_mode')}
    </button>
  )
}

function QueuedMessages({
  session,
  canSteer,
  onEdit,
  onSteer,
  onRemove,
}: {
  session: AgentSession
  canSteer: boolean
  onEdit: (message: NonNullable<AgentSession['queued_messages']>[number]) => void
  onSteer: (message: NonNullable<AgentSession['queued_messages']>[number]) => void
  onRemove: (messageId: string) => void
}) {
  const { t } = useI18n()
  const messages = session.queued_messages ?? []
  if (!messages.length) return null
  return (
    <div className="px-3.5">
      {/* The card tucks against the composer's top edge: rounded top corners
          only, open bottom, and overflow-hidden so row hover fills clip to
          the rounding. */}
      <div className="overflow-hidden rounded-t-xl border border-b-0 bg-card py-1">
        {messages.map((message) => (
          <div
            className="flex h-[30px] w-full items-center gap-2 pr-1.5 text-[12.5px] hover:bg-accent"
            key={message.id}
          >
            <button
              className="flex min-w-0 flex-1 items-center gap-2 self-stretch pl-3 text-left outline-none focus-visible:ring-1 focus-visible:ring-inset focus-visible:ring-ring"
              title={t('composer.edit_in_composer')}
              type="button"
              onClick={() => onEdit(message)}
            >
              <WakuIcon className="size-3 shrink-0 text-[var(--text-tertiary)]" name="queue" />
              <span className="min-w-0 flex-1 truncate">
                {message.display_content || message.content || message.attachments?.map((item) => item.name).join(', ')}
              </span>
            </button>
            <div className="flex shrink-0 items-center gap-0.5">
              {canSteer && (
                <button
                  className="flex h-6 items-center gap-1.5 rounded-md px-1.5 text-[11.5px] text-[var(--text-secondary)] outline-none hover:bg-accent focus-visible:ring-1 focus-visible:ring-ring"
                  title={t('composer.steer_current')}
                  type="button"
                  onClick={() => onSteer(message)}
                >
                  <WakuIcon className="size-[11px]" name="cornerDownRight" />
                  {t('composer.steer')}
                </button>
              )}
              <button
                aria-label={t('composer.remove_followup')}
                className="grid size-6 shrink-0 place-items-center rounded-md text-[var(--text-secondary)] outline-none hover:bg-accent focus-visible:ring-1 focus-visible:ring-ring"
                type="button"
                onClick={() => onRemove(message.id)}
              >
                <WakuIcon className="size-3" name="trash" />
              </button>
              <ControlMenu
                caret={false}
                items={[
                  {
                    id: 'edit',
                    label: t('composer.edit_in_composer'),
                    icon: 'pencil',
                    onSelect: () => onEdit(message),
                  },
                  {
                    id: 'remove',
                    label: t('composer.remove_followup'),
                    icon: 'trash',
                    onSelect: () => onRemove(message.id),
                  },
                ]}
                align="right"
                label={t('composer.queued_message_actions')}
                placement="below"
                selectionMode="status"
                triggerClassName="grid size-6 place-items-center px-0 rounded-md"
              >
                <WakuIcon className="size-3" name="ellipsis" />
              </ControlMenu>
            </div>
          </div>
        ))}
      </div>
    </div>
  )
}

function BranchPicker({
  snapshot,
  workspace,
  disabled,
  pending,
  returnFocus,
  onSelect,
  onCreate,
  onRefresh,
}: {
  snapshot: BranchSnapshot
  workspace: NonNullable<AgentSession['workspace']>
  disabled: boolean
  pending: boolean
  returnFocus: RefObject<HTMLElement | null>
  onSelect: (branch: string) => void
  onCreate: (branch: string) => void
  onRefresh: () => void
}) {
  const { t } = useI18n()
  const [open, setOpen] = useState(false)
  const [mode, setMode] = useState<'browse' | 'create'>('browse')
  const [query, setQuery] = useState('')
  const [branchName, setBranchName] = useState('')
  const [active, setActive] = useState(0)
  const input = useRef<HTMLInputElement>(null)
  const plannedWorktree = workspace.kind === 'newWorktree'
  const selected = workspace.kind === 'newWorktree'
    ? workspace.baseBranch ?? snapshot.default_branch ?? snapshot.current ?? snapshot.detached_head
    : workspace.kind === 'worktree'
      ? snapshot.current ?? workspace.branch ?? snapshot.detached_head
      : snapshot.current ?? snapshot.detached_head
  const normalized = query.trim().toLowerCase()
  const visible = [...snapshot.branches]
    .filter((branch) => normalized.split(/\s+/).filter(Boolean).every((part) => branch.name.toLowerCase().includes(part)))
    .sort((left, right) => {
      if (left.name === selected) return -1
      if (right.name === selected) return 1
      return left.name.localeCompare(right.name)
    })
  const actions = [
    ...visible
      .filter((branch) => plannedWorktree || !branch.checked_out_elsewhere || branch.name === selected)
      .map((branch) => ({ kind: 'branch' as const, branch })),
    ...(!plannedWorktree ? [{ kind: 'create' as const }] : []),
  ]

  function updateOpen(next: boolean) {
    setOpen(next)
    if (!next) return
    setMode('browse')
    setQuery('')
    setBranchName('')
    setActive(0)
    onRefresh()
    requestAnimationFrame(() => input.current?.focus())
  }

  function choose(branch: string) {
    onSelect(branch)
    setOpen(false)
  }

  function submitCreate() {
    const next = branchName.trim()
    if (!next) return
    onCreate(next)
    setOpen(false)
  }

  return (
    <Popover.Root modal={false} open={open} onOpenChange={updateOpen}>
      <Popover.Trigger
        className={cn(
          'flex h-6 max-w-52 shrink-0 items-center gap-1.5 rounded-[6px] px-[7px] text-[11px] text-[var(--text-secondary)] outline-none hover:bg-accent focus-visible:ring-1 focus-visible:ring-ring disabled:opacity-45',
          open && 'bg-accent text-foreground',
        )}
        disabled={disabled}
        onKeyDown={(event) => {
          if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
            event.preventDefault()
            updateOpen(true)
          }
        }}
      >
        <WakuIcon className="size-[11px] text-[var(--text-tertiary)]" name="gitBranch" />
        <span className="truncate">{pending ? t('branches.switching') : selected ?? t('branches.detached_head')}</span>
      </Popover.Trigger>
      <Popover.Portal>
        <Popover.Positioner
          align="start"
          className="z-[100] outline-none"
          collisionPadding={8}
          side="top"
          sideOffset={4}
        >
          <Popover.Popup
            aria-label={t('branches.choose')}
            className="waku-popover-surface flex max-h-[390px] w-[360px] flex-col overflow-hidden rounded-[13px] outline-none"
            finalFocus={(closeType) => closeType === 'keyboard' ? true : returnFocus.current}
            initialFocus={input}
            role="dialog"
            onKeyDown={(event) => {
              if (mode === 'create') {
                if (event.key === 'Enter') {
                  event.preventDefault()
                  submitCreate()
                }
                return
              }
              if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
                event.preventDefault()
                if (!actions.length) return
                setActive((current) => (
                  event.key === 'ArrowDown'
                    ? (current + 1) % actions.length
                    : (current - 1 + actions.length) % actions.length
                ))
              } else if (event.key === 'Home') {
                event.preventDefault()
                setActive(0)
              } else if (event.key === 'End') {
                event.preventDefault()
                setActive(Math.max(0, actions.length - 1))
              } else if (event.key === 'Enter') {
                event.preventDefault()
                const action = actions[active] ?? actions[0]
                if (action?.kind === 'branch') choose(action.branch.name)
                else if (action?.kind === 'create') {
                  setMode('create')
                  requestAnimationFrame(() => input.current?.focus())
                }
              }
            }}
          >
            {mode === 'create' ? (
              <div className="p-3.5">
                <div className="flex items-center gap-2 text-[13px] font-medium">
                  <WakuIcon className="size-3.5 text-[var(--text-secondary)]" name="plus" />
                  {t('branches.create_and_checkout')}
                </div>
                <input
                  autoFocus
                  className="mt-3 h-9 w-full rounded-[9px] border bg-background px-2.5 text-[12px] outline-none focus:border-ring"
                  placeholder={t('input.new_branch_name')}
                  ref={input}
                  value={branchName}
                  onChange={(event) => setBranchName(event.target.value)}
                />
                <div className="mt-2 text-[10.5px] text-[var(--text-tertiary)]">{t('branches.create_hint')}</div>
              </div>
            ) : (
              <>
                <div className="h-[52px] shrink-0 px-3 pb-2 pt-2.5">
                  <label className="flex h-[34px] items-center gap-2 rounded-[9px] bg-background px-2.5 focus-within:ring-1 focus-within:ring-ring">
                    <WakuIcon className="size-[15px] text-[var(--text-secondary)]" name="search" />
                    <input
                      className="min-w-0 flex-1 bg-transparent text-[12px] outline-none"
                      placeholder={t('input.search_branches')}
                      ref={input}
                      value={query}
                      onChange={(event) => {
                        setQuery(event.target.value)
                        setActive(0)
                      }}
                    />
                  </label>
                </div>
                <div className="px-3.5 pb-1 pt-0.5 text-[12px] font-medium text-[var(--text-tertiary)]">{t('branches.title')}</div>
                <div className="min-h-0 max-h-[260px] overflow-y-auto px-1">
                  {!visible.length && (
                    <div className="grid h-16 place-items-center text-[11.5px] text-[var(--text-ghost)]">{t('branches.none_found')}</div>
                  )}
                  {visible.map((branch) => {
                    const disabledBranch = branch.checked_out_elsewhere && !plannedWorktree && branch.name !== selected
                    const actionIndex = actions.findIndex((action) => action.kind === 'branch' && action.branch.name === branch.name)
                    return (
                      <button
                        className={cn(
                          'flex h-8 w-full items-center gap-2 rounded-[6px] px-2 text-left text-[11.5px] outline-none hover:bg-accent focus-visible:bg-accent',
                          disabledBranch && 'text-[var(--text-ghost)] hover:bg-transparent',
                          actionIndex >= 0 && actionIndex === active && 'bg-accent',
                        )}
                        disabled={disabledBranch}
                        key={branch.name}
                        title={disabledBranch ? t('branches.checked_out_elsewhere') : undefined}
                        type="button"
                        onMouseEnter={() => actionIndex >= 0 && setActive(actionIndex)}
                        onClick={() => choose(branch.name)}
                      >
                        <WakuIcon className="size-3 text-[var(--text-tertiary)]" name="gitBranch" />
                        <span className="min-w-0 flex-1 truncate">{branch.name}</span>
                        {branch.name === selected && <WakuIcon className="size-[11px] text-[var(--text-tertiary)]" name="check" />}
                      </button>
                    )
                  })}
                </div>
                {!plannedWorktree && (
                  <>
                    <div className="mx-1.5 my-1 h-px bg-border" />
                    <button
                      className={cn(
                        'mx-1 mb-1 flex h-8 items-center gap-2 rounded-[6px] px-2 text-left text-[11.5px] outline-none hover:bg-accent focus-visible:bg-accent',
                        actions[active]?.kind === 'create' && 'bg-accent',
                      )}
                      type="button"
                      onMouseEnter={() => setActive(Math.max(0, actions.length - 1))}
                      onClick={() => {
                        setMode('create')
                        requestAnimationFrame(() => input.current?.focus())
                      }}
                    >
                      <WakuIcon className="size-3 text-[var(--text-secondary)]" name="plus" />
                      {t('branches.create_and_checkout_ellipsis')}
                    </button>
                  </>
                )}
              </>
            )}
          </Popover.Popup>
        </Popover.Positioner>
      </Popover.Portal>
    </Popover.Root>
  )
}

function UsageMeter({
  session,
  openSignal,
  onOpenSignalHandled,
  returnFocus,
}: {
  session: AgentSession
  openSignal?: number
  onOpenSignalHandled?: () => void
  returnFocus: RefObject<HTMLElement | null>
}) {
  const { t } = useI18n()
  const [open, setOpen] = useState(false)
  const usageShortcut = usePrimaryShortcut('⌘U', 'Ctrl+U')
  const context = session.context_usage
  const contextPercent = context?.window
    ? context.tokens * 100 / context.window
    : null

  useEffect(() => {
    if (!openSignal) return
    setOpen((current) => !current)
    onOpenSignalHandled?.()
  }, [openSignal, onOpenSignalHandled])

  const tooltip = contextPercent == null
      ? t('usage.shortcut', { shortcut: usageShortcut })
      : t('usage.context_used', { percent: contextPercent.toFixed(0), shortcut: usageShortcut })

  return (
    <Popover.Root modal={false} open={open} onOpenChange={setOpen}>
      <Popover.Trigger
        aria-label={tooltip}
        className={cn(
          'grid h-5 w-[23px] place-items-center rounded-[5px] outline-none hover:bg-accent focus-visible:ring-1 focus-visible:ring-ring',
          open && 'bg-accent',
        )}
        title={tooltip}
      >
        <ContextGauge percent={contextPercent} />
      </Popover.Trigger>
      <Popover.Portal>
        <Popover.Positioner
          align="end"
          className="z-[100] outline-none"
          collisionPadding={8}
          side="top"
          sideOffset={4}
        >
          <Popover.Popup
            aria-label={t('settings.usage')}
            className="waku-popover-surface flex w-80 flex-col gap-3 rounded-[10px] p-3.5 text-xs text-popover-foreground outline-none"
            finalFocus={(closeType) => closeType === 'keyboard' ? true : returnFocus.current}
            initialFocus={false}
            role="dialog"
          >
            <UsageLane
              label={t('usage.context_window')}
              percent={contextPercent ?? 0}
              value={context?.window && contextPercent != null
                ? `${formatTokens(context.tokens)} / ${formatTokens(context.window)} (${contextPercent.toFixed(0)}%)`
                : formatTokens(context?.tokens ?? 0)}
            />
          </Popover.Popup>
        </Popover.Positioner>
      </Popover.Portal>
    </Popover.Root>
  )
}

function ContextGauge({ percent }: { percent: number | null }) {
  const radius = 5.25
  const circumference = 2 * Math.PI * radius
  const bounded = percent == null ? 0 : Math.min(100, Math.max(5, percent))
  const color = percent != null && percent >= 95
    ? 'var(--destructive)'
    : percent != null && percent >= 80
      ? 'var(--warning)'
      : 'var(--text-secondary)'
  return (
    <svg aria-hidden="true" className="size-[13px] -rotate-90" viewBox="0 0 13 13">
      <circle
        className="text-input"
        cx="6.5"
        cy="6.5"
        fill="none"
        r={radius}
        stroke="currentColor"
        strokeWidth="2.5"
      />
      {percent != null && (
        <circle
          cx="6.5"
          cy="6.5"
          fill="none"
          r={radius}
          stroke={color}
          strokeDasharray={circumference}
          strokeDashoffset={circumference * (1 - bounded / 100)}
          strokeLinecap="round"
          strokeWidth="2.5"
        />
      )}
    </svg>
  )
}

function UsageLane({
  label,
  percent,
  reset,
  value,
}: {
  label: string
  percent: number
  reset?: string
  value: string
}) {
  return (
    <div className="space-y-[7px]">
      <div className="flex min-w-0 items-center gap-2">
        <span className="min-w-0 flex-1 truncate">{label}</span>
        {reset && <span className="shrink-0 text-[11px] text-[var(--text-tertiary)]">{reset}</span>}
        <span className="shrink-0 text-[11px] text-[var(--text-secondary)]">{value}</span>
      </div>
      <div className="h-[3px] overflow-hidden rounded-full bg-accent">
        <div
          className={cn(
            'h-full rounded-full bg-[var(--text-secondary)]',
            percent >= 80 && 'bg-[var(--warning)]',
            percent >= 95 && 'bg-destructive',
          )}
          style={{ width: `${Math.min(100, Math.max(0, percent))}%` }}
        />
      </div>
    </div>
  )
}

function errorMessage(error: unknown) {
  return error instanceof Error ? error.message : String(error)
}

function formatTokens(value: number) {
  return new Intl.NumberFormat(undefined, {
    notation: value >= 10_000 ? 'compact' : 'standard',
    maximumFractionDigits: 1,
  }).format(value)
}
