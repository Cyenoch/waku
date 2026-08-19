import { useQueryClient } from "@tanstack/react-query";
import type {
  AgentSession,
  ApiFormat,
  AuthPhase,
  DaemonSettings,
  ExternalProvider,
  LoginMethod,
  Project,
} from "@wakuwaku/client";
import { useEffect, useState, type ReactNode } from "react";
import { toast } from "sonner";
import { ControlMenu } from "@/components/control-menu";
import { SkillsSettings } from "@/components/skills-settings";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { UsageSettings } from "@/components/usage-settings";
import { WakuIcon, type WakuIconName } from "@/components/waku-icon";
import {
  useDaemonSettings,
  useProviderAuth,
  useTaskState,
} from "@/hooks/use-daemon-data";
import { useCopyFeedback } from "@/hooks/use-copy-feedback";
import {
  cancelLogin,
  completeApiKeyLogin,
  daemonKeys,
  logoutProvider,
  startLogin,
  updateDaemonSettings,
} from "@/lib/daemon-api";
import { useDaemon } from "@/lib/daemon-context";
import type { Translator } from "@/lib/transcript-presentation";
import {
  applyThemeChoice,
  readThemeChoice,
  type ThemeChoice,
} from "@/lib/appearance";
import { APP_LANGUAGES, languageLabel, useI18n } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import {
  browserComposerPreferenceStorage,
  readComposerPreferences,
  type ComposerPreferences,
} from "@/lib/composer-preferences";

export type SettingsPageId =
  "general" | "appearance" | "providers" | "skills" | "usage" | "daemon";

export const SETTINGS_PAGES: Array<{
  id: SettingsPageId;
  label: string;
  labelKey: string;
  icon: WakuIconName;
  keywords: string;
  keywordsKey: string;
}> = [
  {
    id: "general",
    label: "General",
    labelKey: "settings.general",
    icon: "settings",
    keywords:
      "general local projects conversations privacy analytics telemetry anonymous sharing",
    keywordsKey: "settings.general_keywords",
  },
  {
    id: "appearance",
    label: "Appearance",
    labelKey: "settings.appearance",
    icon: "appearance",
    keywords: "appearance theme system light dark language",
    keywordsKey: "settings.appearance_keywords",
  },
  {
    id: "providers",
    label: "Providers",
    labelKey: "settings.providers",
    icon: "bot",
    keywords: "providers endpoint models api url headers limits",
    keywordsKey: "settings.providers_keywords",
  },
  {
    id: "skills",
    label: "Skills",
    labelKey: "settings.skills",
    icon: "package",
    keywords: "skills library agent disable enable delete shared",
    keywordsKey: "settings.skills_keywords",
  },
  {
    id: "usage",
    label: "Usage",
    labelKey: "settings.usage",
    icon: "chartColumn",
    keywords:
      "usage tokens cost spend cache daily monthly project model history",
    keywordsKey: "settings.usage_keywords",
  },
  {
    id: "daemon",
    label: "Daemon",
    labelKey: "settings.daemon",
    icon: "server",
    keywords: "daemon server remote web network connection url token websocket",
    keywordsKey: "settings.daemon_keywords",
  },
];

export function isSettingsPageId(value: string): value is SettingsPageId {
  return SETTINGS_PAGES.some((page) => page.id === value);
}

export function SettingsView({
  page,
  projects,
  onBack,
  onPageChange,
}: {
  page: SettingsPageId;
  projects: Project[];
  onBack: () => void;
  onPageChange: (page: SettingsPageId) => void;
}) {
  const { t } = useI18n();
  const [query, setQuery] = useState("");
  const localizedPages = SETTINGS_PAGES.map((candidate) => ({
    ...candidate,
    localizedLabel: t(candidate.labelKey),
    localizedKeywords:
      `${candidate.keywords} ${t(candidate.keywordsKey)}`.toLowerCase(),
  }));
  const pages = localizedPages.filter(
    (candidate) =>
      !query.trim() ||
      candidate.localizedKeywords.includes(query.trim().toLowerCase()),
  );
  const activePage = localizedPages.find((candidate) => candidate.id === page);

  useEffect(() => {
    const escape = (event: KeyboardEvent) => {
      if (event.key === "Escape" && query) setQuery("");
    };
    window.addEventListener("keydown", escape);
    return () => window.removeEventListener("keydown", escape);
  }, [query]);

  return (
    <div className="flex h-dvh min-w-0 flex-1 bg-background">
      <aside className="flex h-full w-[252px] shrink-0 flex-col bg-sidebar pt-3">
        <div className="px-3">
          <button
            className="flex h-[34px] w-full items-center gap-[9px] rounded-lg px-[9px] text-[13px] text-[var(--text-secondary)] outline-none hover:bg-sidebar-accent active:bg-accent focus-visible:ring-1 focus-visible:ring-ring"
            type="button"
            onClick={onBack}
          >
            <WakuIcon
              className="size-[15px] text-[var(--text-tertiary)]"
              name="arrowLeft"
            />
            {t("settings.back")}
          </button>
        </div>
        <div className="px-3 pt-2">
          <label className="flex h-8 items-center gap-2 rounded-lg border bg-[var(--inset)] px-2.5 focus-within:border-ring">
            <WakuIcon
              className="size-[13px] text-[var(--text-tertiary)]"
              name="search"
            />
            <input
              aria-label={t("settings.search")}
              className="min-w-0 flex-1 bg-transparent text-[12px] outline-none placeholder:text-[var(--text-ghost)]"
              placeholder={t("settings.search")}
              value={query}
              onChange={(event) => setQuery(event.target.value)}
              onKeyDown={(event) => {
                if (
                  !pages.length ||
                  !["ArrowDown", "ArrowUp"].includes(event.key)
                )
                  return;
                event.preventDefault();
                const current = pages.findIndex(
                  (candidate) => candidate.id === page,
                );
                const delta = event.key === "ArrowDown" ? 1 : -1;
                onPageChange(
                  pages[(current + delta + pages.length) % pages.length]!.id,
                );
              }}
            />
          </label>
        </div>
        <nav
          aria-label={t("common.settings")}
          className="mt-[18px] flex flex-col gap-[3px] px-3"
        >
          {pages.map((candidate) => (
            <button
              aria-current={page === candidate.id ? "page" : undefined}
              className={cn(
                "flex h-9 items-center gap-2.5 rounded-lg px-[11px] text-[13px] text-[var(--text-secondary)] outline-none hover:bg-sidebar-accent focus-visible:ring-1 focus-visible:ring-ring",
                page === candidate.id && "bg-sidebar-accent text-foreground",
              )}
              key={candidate.id}
              type="button"
              onClick={() => onPageChange(candidate.id)}
            >
              <WakuIcon
                className="size-[15px] text-[var(--text-tertiary)]"
                name={candidate.icon}
              />
              {candidate.localizedLabel}
            </button>
          ))}
        </nav>
      </aside>
      <main
        className={cn(
          "min-w-0 flex-1 border-l bg-background",
          page === "skills"
            ? "overflow-hidden"
            : "overflow-y-auto px-8 pb-12 pt-5",
        )}
      >
        {page === "skills" ? (
          <SkillsSettings projects={projects} />
        ) : (
          <div
            className={cn(
              "mx-auto w-full",
              page === "usage" ? "max-w-[1024px]" : "max-w-[760px]",
            )}
          >
            <h1 className="text-[18px] font-medium">
              {activePage?.localizedLabel}
            </h1>
            {page === "general" && <GeneralSettings />}
            {page === "appearance" && <AppearanceSettings />}
            {page === "providers" && <ProvidersSettings />}
            {page === "usage" && <UsageSettings projects={projects} />}
            {page === "daemon" && <DaemonSettings />}
          </div>
        )}
      </main>
    </div>
  );
}

function GeneralSettings() {
  const { t } = useI18n();
  const [analytics, setAnalytics] = useStoredBoolean(
    "wakuwaku.analytics-enabled",
    true,
  );
  return (
    <div>
      <SettingsCard>
        <SettingText
          title={t("settings.local_by_default")}
          description={t("settings.local_by_default_web_description")}
        />
      </SettingsCard>
      <SettingsCard row>
        <SettingText
          title={t("settings.share_anonymous_usage_data")}
          description={t("settings.share_anonymous_usage_data_description")}
        />
        <Toggle
          checked={analytics}
          label={t("settings.share_anonymous_usage_data")}
          onChange={setAnalytics}
        />
      </SettingsCard>
    </div>
  );
}

function AppearanceSettings() {
  const { language, locale, setLanguage, t } = useI18n();
  const [theme, setTheme] = useState<ThemeChoice>(() =>
    typeof window === "undefined"
      ? "system"
      : readThemeChoice(window.localStorage),
  );
  useEffect(() => {
    const systemAppearance = window.matchMedia("(prefers-color-scheme: dark)");
    const apply = () =>
      applyThemeChoice(
        document.documentElement,
        theme,
        systemAppearance.matches,
      );
    apply();
    window.localStorage.setItem("waku.theme", theme);
    systemAppearance.addEventListener("change", apply);
    return () => systemAppearance.removeEventListener("change", apply);
  }, [theme]);
  const themeLabel = t(`settings.theme_${theme}`);
  return (
    <div className="mt-[15px] w-full overflow-hidden rounded-[13px] bg-[var(--raised)]">
      <div className="flex min-h-[60px] items-center gap-6 px-5 py-3">
        <SettingText
          title={t("settings.theme")}
          description={t("settings.theme_description")}
        />
        <ControlMenu
          align="right"
          items={(["system", "light", "dark"] as ThemeChoice[]).map(
            (choice) => ({
              id: choice,
              label: t(`settings.theme_${choice}`),
              selected: choice === theme,
              onSelect: () => setTheme(choice),
            }),
          )}
          label={themeLabel}
          menuClassName="w-[140px]"
          placement="below"
          triggerClassName="h-8 w-[116px] max-w-none justify-between border bg-background px-3 text-[12px]"
        />
      </div>
      <div className="mx-5 border-t" />
      <div className="flex min-h-[60px] items-center gap-6 px-5 py-3">
        <SettingText
          title={t("language.title")}
          description={t("language.description")}
        />
        <ControlMenu
          align="right"
          items={APP_LANGUAGES.map((choice) => ({
            id: choice,
            label: languageLabel(choice, locale),
            selected: choice === language,
            onSelect: () => setLanguage(choice),
          }))}
          label={languageLabel(language, locale)}
          menuClassName="w-[160px]"
          placement="below"
          triggerClassName="h-8 w-[116px] max-w-none justify-between border bg-background px-3 text-[12px]"
        />
      </div>
    </div>
  );
}

export const BUILTIN_AUTH_PROVIDERS: Array<{
  id: string;
  name: string;
  method: LoginMethod;
  secondary?: LoginMethod;
}> = [
  {
    id: "openai-codex",
    name: "ChatGPT Codex",
    method: "oauthBrowser",
    secondary: "oauthDevice",
  },
  { id: "opencode-go", name: "OpenCode Go", method: "apiKey" },
  { id: "opencode-zen", name: "OpenCode Zen", method: "apiKey" },
  { id: "xai", name: "xAI API", method: "apiKey" },
  { id: "xai-oauth", name: "SuperGrok", method: "oauthDevice" },
];

const RESERVED_PROVIDER_IDS = new Set([
  "openai-responses",
  "openai-chat",
  "anthropic",
  ...BUILTIN_AUTH_PROVIDERS.map((provider) => provider.id),
]);

function ProvidersSettings() {
  const { t } = useI18n();
  const { client, config } = useDaemon();
  const queryClient = useQueryClient();
  const settings = useDaemonSettings();
  const taskState = useTaskState();
  const providers = settings.data?.external_providers ?? [];
  const [editor, setEditor] = useState<{
    form: ProviderForm;
    existingId: string | null;
  } | null>(null);

  function providerIsReferenced(providerId: string) {
    if (!config || !taskState.data) return true;
    let favorites: string[];
    try {
      const parsed: unknown = JSON.parse(
        window.localStorage.getItem("waku.favorite-models") ?? "[]",
      );
      if (
        !Array.isArray(parsed) ||
        !parsed.every((value): value is string => typeof value === "string")
      ) {
        return true;
      }
      favorites = parsed;
    } catch {
      return true;
    }
    return providerIdIsReferenced(
      providerId,
      taskState.data.sessions,
      favorites,
      readComposerPreferences(
        browserComposerPreferenceStorage(),
        config.address,
      ),
    );
  }

  async function apply(
    next: DaemonSettings,
    affectedProviderIds: readonly string[] = [],
  ) {
    if (!client || !config) return false;
    try {
      await updateDaemonSettings(client, next);
      queryClient.setQueryData(daemonKeys.settings(config.address), next);
      await queryClient.invalidateQueries({
        queryKey: ["daemon", config.address, "models"],
      });
      await Promise.all(
        affectedProviderIds.map((provider) =>
          queryClient.invalidateQueries({
            queryKey: daemonKeys.models(config.address, provider),
          }),
        ),
      );
      return true;
    } catch (error) {
      toast.error(errorMessage(error));
      return false;
    }
  }

  function closeEditor() {
    setEditor(null);
  }
  function beginAdd() {
    setEditor({ form: emptyProviderForm(), existingId: null });
  }
  function beginEdit(provider: ExternalProvider) {
    setEditor({ form: providerToForm(provider), existingId: provider.id });
  }
  async function save() {
    if (!settings.data || !editor) return;
    const { provider: next, error } = formToProvider(editor.form);
    if (error) {
      toast.error(t(error));
      return;
    }
    if (!next) return;
    if (!next.id || !next.name || !next.baseUrl) {
      toast.error(t("providers.required_fields"));
      return;
    }
    if (
      editor.existingId &&
      next.id !== editor.existingId &&
      providerIsReferenced(editor.existingId)
    ) {
      toast.error(t("providers.in_use"));
      return;
    }
    if (providerIdsConflict(providers, next.id, editor.existingId)) {
      toast.error(t("providers.duplicate_id"));
      return;
    }
    const affectedProviderIds = [editor.existingId, next.id].filter(
      (id): id is string => id !== null,
    );
    if (
      await apply(
        {
          ...settings.data,
          external_providers: replaceProviderById(
            providers,
            editor.existingId,
            next,
          ),
        },
        affectedProviderIds,
      )
    ) {
      closeEditor();
    }
  }
  async function remove(provider: ExternalProvider) {
    if (!settings.data) return;
    if (providerIsReferenced(provider.id)) {
      toast.error(t("providers.in_use"));
      return;
    }
    if (
      (await apply({
        ...settings.data,
        external_providers: providers.filter(
          (candidate) => candidate.id !== provider.id,
        ),
      })) &&
      editor?.existingId === provider.id
    ) {
      closeEditor();
    }
  }
  const update = (patch: Partial<ProviderForm>) => {
    setEditor((current) =>
      current ? { ...current, form: { ...current.form, ...patch } } : current,
    );
  };
  return (
    <div className="mt-[15px] space-y-3">
      <SettingsCard>
        <div className="flex items-start justify-between gap-4">
          <SettingText
            title={t("providers.title")}
            description={t("providers.web_description")}
          />
          <Button className="shrink-0" onClick={beginAdd}>
            <WakuIcon className="mr-1.5 size-3.5" name="plus" />
            {t("providers.add")}
          </Button>
        </div>
      </SettingsCard>
      {BUILTIN_AUTH_PROVIDERS.map((provider) => (
        <BuiltinProviderCard key={provider.id} provider={provider} t={t} />
      ))}
      {providers.map((provider) => (
        <SettingsCard key={provider.id}>
          <div className="flex items-center gap-3">
            <span className="min-w-0 flex-1">
              <span className="block truncate text-[13px] font-medium">
                {provider.name}
              </span>
              <span className="block truncate text-[11px] text-[var(--text-tertiary)]">
                {provider.baseUrl}
              </span>
            </span>
            <Button
              variant="ghost"
              size="sm"
              onClick={() => beginEdit(provider)}
            >
              {t("common.edit")}
            </Button>
            <Button
              variant="ghost"
              size="sm"
              onClick={() => void remove(provider)}
            >
              {t("common.delete")}
            </Button>
          </div>
        </SettingsCard>
      ))}
      <Dialog
        open={editor !== null}
        onOpenChange={(open) => {
          if (!open) closeEditor();
        }}
      >
        <DialogContent className="flex max-h-[min(90dvh,40rem)] flex-col overflow-hidden">
          <DialogTitle>
            {editor?.existingId
              ? t("providers.edit_title")
              : t("providers.add")}
          </DialogTitle>
          {editor ? (
            <ProviderFormView
              form={editor.form}
              update={update}
              onSave={() => void save()}
              onCancel={closeEditor}
              t={t}
            />
          ) : null}
        </DialogContent>
      </Dialog>
    </div>
  );
}

function BuiltinProviderCard({
  provider,
  t,
}: {
  provider: (typeof BUILTIN_AUTH_PROVIDERS)[number];
  t: Translator;
}) {
  const { client, config } = useDaemon();
  const queryClient = useQueryClient();
  const auth = useProviderAuth(provider.id);
  const [key, setKey] = useState("");
  const [pending, setPending] = useState<AuthPhase | null>(null);
  const status = auth.data?.statuses.find(
    (candidate) => candidate.provider === provider.id,
  );
  const phase =
    pending && isActiveAuthPhase(pending)
      ? pending
      : activeAuthPhase(auth.data?.phases ?? [], provider.id);

  useEffect(() => {
    setPending(null);
    setKey("");
  }, [client]);

  useEffect(() => {
    if (!pending || pending.type === "idle") return;
    const remote = auth.data?.phases ?? [];
    const settled = remote.some(
      (candidate) =>
        (candidate.type === "completed" || candidate.type === "failed") &&
        candidate.provider === provider.id &&
        candidate.loginId === pending.loginId,
    );
    const connected = Boolean(status?.method && status.method !== "none");
    if (!settled && !connected) return;
    setPending(null);
    if (connected && config) {
      void queryClient.invalidateQueries({
        queryKey: daemonKeys.models(config.address, provider.id),
      });
    }
  }, [auth.data, config, pending, provider.id, queryClient, status]);

  async function login(method: LoginMethod) {
    if (!client) return;
    try {
      const next = await startLogin(client, provider.id, method);
      setPending(isActiveAuthPhase(next) ? next : null);
      if (next.type === "awaitingBrowser") openExternal(next.url);
      if (next.type === "awaitingDevice") openExternal(next.verificationUrl);
      await auth.refetch();
    } catch (error) {
      toast.error(errorMessage(error));
    }
  }

  async function complete() {
    if (!client || !pending || pending.type !== "awaitingApiKey" || !key)
      return;
    try {
      const next = await completeApiKeyLogin(
        client,
        pending.loginId,
        provider.id,
        key,
      );
      setPending(isActiveAuthPhase(next) ? next : null);
      setKey("");
      await auth.refetch();
    } catch (error) {
      setKey("");
      toast.error(errorMessage(error));
    }
  }

  async function cancel() {
    if (!client || !phase) return;
    try {
      await cancelLogin(client, phase.loginId);
      setPending(null);
      setKey("");
      await auth.refetch();
    } catch (error) {
      toast.error(errorMessage(error));
    }
  }

  async function logout() {
    if (!client) return;
    try {
      await logoutProvider(client, provider.id);
      setPending(null);
      setKey("");
      await auth.refetch();
      if (config)
        await queryClient.invalidateQueries({
          queryKey: daemonKeys.models(config.address, provider.id),
        });
    } catch (error) {
      toast.error(errorMessage(error));
    }
  }

  return (
    <SettingsCard>
      <div className="flex items-center gap-3">
        <SettingText
          title={provider.name}
          description={
            status?.email ||
            status?.accountId ||
            t("providers.not_authenticated")
          }
        />
        <span className="text-[11px] text-[var(--text-tertiary)]">
          {phase?.type ?? status?.method ?? "none"}
        </span>
        {status?.method === "none" && !phase && (
          <>
            <Button size="sm" onClick={() => void login(provider.method)}>
              {t("providers.connect")}
            </Button>
            {provider.secondary ? (
              <Button
                size="sm"
                variant="ghost"
                onClick={() => {
                  const method = provider.secondary;
                  if (method) void login(method);
                }}
              >
                {t("providers.sign_in")}
              </Button>
            ) : null}
          </>
        )}
        {status?.method !== "none" && (
          <Button size="sm" variant="ghost" onClick={() => void logout()}>
            {t("providers.logout")}
          </Button>
        )}
      </div>
      {phase?.type === "awaitingApiKey" && (
        <div className="mt-3 flex gap-2">
          <Input
            autoComplete="off"
            placeholder={t("providers.api_key_placeholder")}
            type="password"
            value={key}
            onChange={(event) => setKey(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter") void complete();
            }}
          />
          <Button disabled={!key} onClick={() => void complete()}>
            {t("common.continue")}
          </Button>
          <Button variant="ghost" onClick={() => void cancel()}>
            {t("common.cancel")}
          </Button>
        </div>
      )}
    </SettingsCard>
  );
}

type ActiveAuthPhase = Extract<
  AuthPhase,
  { type: "awaitingBrowser" | "awaitingDevice" | "awaitingApiKey" }
>;

export function isActiveAuthPhase(phase: AuthPhase): phase is ActiveAuthPhase {
  return (
    phase.type === "awaitingBrowser" ||
    phase.type === "awaitingDevice" ||
    phase.type === "awaitingApiKey"
  );
}

export function activeAuthPhase(
  phases: readonly AuthPhase[],
  provider: string,
): ActiveAuthPhase | null {
  return (
    phases.find(
      (phase): phase is ActiveAuthPhase =>
        isActiveAuthPhase(phase) && phase.provider === provider,
    ) ?? null
  );
}

export function isAllowedExternalUrl(value: string): boolean {
  let url: URL;
  try {
    url = new URL(value);
  } catch {
    return false;
  }
  if (url.username || url.password) return false;
  if (url.protocol === "https:") return true;
  if (url.protocol !== "http:") return false;
  return (
    url.hostname.toLowerCase() === "localhost" ||
    url.hostname === "127.0.0.1" ||
    url.hostname === "[::1]"
  );
}

function openExternal(url: string) {
  if (!isAllowedExternalUrl(url)) return;
  window.open(url, "_blank", "noopener,noreferrer");
}
export function providerIdIsReferenced(
  providerId: string,
  sessions: readonly Pick<AgentSession, "provider">[],
  favorites: readonly string[],
  preferences: Pick<ComposerPreferences, "lastProvider" | "modelTraits">,
) {
  return (
    sessions.some((session) => session.provider === providerId) ||
    preferences.lastProvider === providerId ||
    favorites.some((favorite) => favorite.startsWith(`${providerId}:`)) ||
    Object.keys(preferences.modelTraits).some((key) =>
      key.startsWith(`${providerId}\u0000`),
    )
  );
}

export function providerIdsConflict(
  providers: readonly Pick<ExternalProvider, "id">[],
  nextId: string,
  existingId: string | null,
) {
  return (
    (RESERVED_PROVIDER_IDS.has(nextId) && nextId !== existingId) ||
    providers.some(
      (provider) => provider.id === nextId && provider.id !== existingId,
    )
  );
}

export function replaceProviderById(
  providers: readonly ExternalProvider[],
  existingId: string | null,
  next: ExternalProvider,
) {
  return providers
    .filter((provider) => provider.id !== existingId && provider.id !== next.id)
    .concat(next);
}

type ProviderForm = {
  id: string;
  name: string;
  baseUrl: string;
  apiFormat: ApiFormat;
  apiKey: string;
  headers: string;
};

export const CUSTOM_PROVIDER_API_FORMATS: Array<{
  id: ApiFormat;
  label: string;
}> = [
  { id: "openAiResponses", label: "providers.api_format_openai_responses" },
  { id: "openAiChat", label: "providers.api_format_openai_chat" },
  { id: "anthropic", label: "providers.api_format_anthropic" },
];

function emptyProviderForm(): ProviderForm {
  return {
    id: "",
    name: "",
    baseUrl: "",
    apiFormat: "openAiResponses",
    apiKey: "",
    headers: "",
  };
}

function providerToForm(provider: ExternalProvider): ProviderForm {
  return {
    id: provider.id,
    name: provider.name,
    baseUrl: provider.baseUrl,
    apiFormat: provider.apiFormat,
    apiKey: "",
    headers: (provider.headers ?? [])
      .map(([key, value]) => `${key}: ${value}`)
      .join("\n"),
  };
}

export function formToProvider(form: ProviderForm): {
  provider: ExternalProvider;
  error: string | null;
} {
  const headerLines = form.headers
    .split("\n")
    .map((line) => line.trim())
    .filter(Boolean);
  const headers: [string, string][] = [];
  const headerNames = new Set<string>();
  for (const line of headerLines) {
    const separator = line.indexOf(":");
    if (separator <= 0) {
      return {
        provider: {} as ExternalProvider,
        error: "providers.invalid_header_format",
      };
    }
    const name = line.slice(0, separator).trim();
    const value = line.slice(separator + 1).trim();
    const lowered = name.toLowerCase();
    if (
      !name ||
      !value ||
      headerNames.has(lowered) ||
      [
        "authorization",
        "x-api-key",
        "host",
        "content-length",
        "content-type",
        "anthropic-version",
      ].includes(lowered)
    ) {
      return {
        provider: {} as ExternalProvider,
        error: "providers.invalid_headers",
      };
    }
    headerNames.add(lowered);
    headers.push([name, value]);
  }
  return {
    provider: {
      id: form.id.trim(),
      name: form.name.trim(),
      baseUrl: form.baseUrl.trim(),
      apiFormat: form.apiFormat,
      headers,
    },
    error: null,
  };
}

function ProviderFormView({
  form,
  update,
  onSave,
  onCancel,
  t,
}: {
  form: ProviderForm;
  update: (patch: Partial<ProviderForm>) => void;
  onSave: () => void;
  onCancel: () => void;
  t: Translator;
}) {
  const field = (key: keyof ProviderForm, label: string, type = "text") => (
    <label className="space-y-1">
      <span className="text-[11px] font-medium">{label}</span>
      <Input
        type={type}
        value={form[key]}
        onChange={(event) => update({ [key]: event.target.value })}
      />
    </label>
  );
  return (
    <div className="mt-4 flex min-h-0 flex-1 flex-col">
      <div className="min-h-0 flex-1 overflow-y-auto pr-1">
        <div className="grid gap-3 sm:grid-cols-2">
          {field("id", t("providers.id"))}
          {field("name", t("providers.name"))}
          {field("baseUrl", t("providers.base_url"), "url")}
          <label className="space-y-1">
            <span className="text-[11px] font-medium">
              {t("providers.api_format")}
            </span>
            <select
              className="h-8 w-full rounded-md border bg-background px-2 text-xs"
              value={form.apiFormat}
              onChange={(event) =>
                update({ apiFormat: event.target.value as ApiFormat })
              }
            >
              {CUSTOM_PROVIDER_API_FORMATS.map((format) => (
                <option key={format.id} value={format.id}>
                  {t(format.label)}
                </option>
              ))}
            </select>
          </label>
          {field("apiKey", t("providers.api_key"))}
          <p className="self-end pb-1 text-[11px] text-[var(--text-tertiary)] sm:col-span-2">
            {t("providers.models_auto_fetch")}
          </p>
          {field("headers", t("providers.headers"))}
        </div>
      </div>
      <div className="flex items-end justify-end gap-2 pt-4">
        <Button variant="ghost" onClick={onCancel}>
          {t("common.cancel")}
        </Button>
        <Button onClick={onSave}>{t("common.save")}</Button>
      </div>
    </div>
  );
}

function DaemonSettings() {
  const { t } = useI18n();
  const { config, phase, reconnect, disconnect, forget } = useDaemon();
  const [error, setError] = useState<string | null>(null);
  return (
    <div>
      <SettingsCard>
        <SettingText
          title={t("daemon.external_title")}
          description={t("daemon.web_external_description")}
        />
      </SettingsCard>
      <SettingsCard>
        <SettingText
          title={t("daemon.credentials_title")}
          description={t("daemon.web_connection_description")}
        />
        <div className="mt-4 divide-y rounded-xl border bg-background px-3">
          <DetailRow
            label={t("daemon.websocket_url")}
            value={config?.address ?? t("daemon.not_configured")}
            copy
          />
          <DetailRow
            copy={Boolean(config?.token)}
            label={t("daemon.token")}
            secret={Boolean(config?.token)}
            value={config?.token ?? t("daemon.not_configured")}
          />
          <DetailRow
            label={t("daemon.status")}
            value={t(`daemon.phase_${phase}`)}
          />
        </div>
        {error && (
          <p className="mt-3 text-[11.5px] text-destructive">{error}</p>
        )}
        <div className="mt-4 flex flex-wrap justify-end gap-2">
          <Button variant="ghost" onClick={disconnect}>
            {t("daemon.disconnect")}
          </Button>
          <Button variant="destructive" onClick={forget}>
            {t("daemon.forget")}
          </Button>
          <Button
            disabled={phase === "connecting"}
            onClick={() => {
              setError(null);
              void reconnect().catch((cause) => setError(errorMessage(cause)));
            }}
          >
            {t("daemon.reconnect")}
          </Button>
        </div>
      </SettingsCard>
    </div>
  );
}

function SettingsCard({
  children,
  row = false,
}: {
  children: ReactNode;
  row?: boolean;
}) {
  return (
    <section
      className={cn(
        "mt-[15px] w-full rounded-[13px] bg-[var(--raised)] px-5 py-[14px]",
        row && "flex items-center gap-6",
      )}
    >
      {children}
    </section>
  );
}

function SettingText({
  title,
  description,
}: {
  title: string;
  description: string;
}) {
  return (
    <div className="min-w-0 flex-1">
      <div className="text-[13.5px] font-medium">{title}</div>
      <p className="mt-[5px] text-[12.5px] leading-[18px] text-[var(--text-secondary)]">
        {description}
      </p>
    </div>
  );
}

function Toggle({
  checked,
  label,
  onChange,
}: {
  checked: boolean;
  label: string;
  onChange: (checked: boolean) => void;
}) {
  return (
    <button
      aria-checked={checked}
      aria-label={label}
      className={cn(
        "flex h-5 w-9 shrink-0 items-center rounded-full border p-0.5 outline-none transition-colors focus-visible:ring-1 focus-visible:ring-ring",
        checked
          ? "justify-end border-foreground bg-foreground"
          : "justify-start border-input bg-[var(--inset)]",
      )}
      role="switch"
      type="button"
      onClick={() => onChange(!checked)}
    >
      <span
        className={cn(
          "size-3.5 rounded-full",
          checked ? "bg-background" : "bg-[var(--text-tertiary)]",
        )}
      />
    </button>
  );
}

function DetailRow({
  label,
  value,
  copy = false,
  secret = false,
}: {
  label: string;
  value: string;
  copy?: boolean;
  secret?: boolean;
}) {
  const { t } = useI18n();
  const copyFeedback = useCopyFeedback();
  const [revealed, setRevealed] = useState(false);
  return (
    <div className="flex min-h-12 items-center gap-4 text-[11.5px]">
      <span className="w-28 shrink-0 text-[var(--text-tertiary)]">{label}</span>
      <span className="min-w-0 flex-1 truncate font-mono">
        {secret && !revealed ? "••••••••••••••••••••••••" : value}
      </span>
      {secret && (
        <Button
          aria-label={t(revealed ? "daemon.hide_token" : "daemon.reveal_token")}
          aria-pressed={revealed}
          size="icon-sm"
          title={t(revealed ? "daemon.hide_token" : "daemon.reveal_token")}
          type="button"
          variant="outline"
          onClick={() => setRevealed((current) => !current)}
        >
          <WakuIcon name={revealed ? "eyeOff" : "eye"} />
        </Button>
      )}
      {copy && (
        <Button
          size="sm"
          variant="outline"
          onClick={() => void copyFeedback.copyText(value)}
        >
          <WakuIcon name={copyFeedback.copied ? "check" : "copy"} />
          {t(copyFeedback.copied ? "common.copied" : "common.copy")}
        </Button>
      )}
    </div>
  );
}

function useStoredBoolean(key: string, fallback: boolean) {
  const [value, setValue] = useState(() =>
    typeof window === "undefined"
      ? fallback
      : window.localStorage.getItem(key) !== "false",
  );
  const update = (next: boolean) => {
    setValue(next);
    window.localStorage.setItem(key, String(next));
  };
  return [value, update] as const;
}

function errorMessage(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}
