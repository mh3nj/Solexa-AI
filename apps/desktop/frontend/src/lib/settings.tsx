import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
  type ReactNode,
} from "react";
import {
  getAppSettings,
  getStartupEnabled,
  setAppSetting,
  setStartupMode,
  setTrayMenuLanguage,
} from "./api";
import {
  DEFAULT_THEME_ID,
  applyTheme,
  findTheme,
  sanitizeTheme,
  type Theme,
} from "./theme";

export type Language = "en" | "de" | "fa";
export type StartupMode = "off" | "app" | "tray";

export interface AppSettings {
  themeId: string;
  customThemes: Theme[];
  language: Language;
  closeToTray: boolean;
  minimizeToTray: boolean;
  startupMode: StartupMode;
}

interface SettingsContextValue {
  settings: AppSettings;
  activeTheme: Theme;
  transientTheme: Theme | null;
  hydrated: boolean;
  startupRegistered: boolean;
  setLanguage: (language: Language) => void;
  setThemeId: (id: string) => void;
  applyTransientTheme: (theme: Theme) => void;
  clearTransientTheme: () => void;
  saveCustomTheme: (theme: Theme) => Theme;
  deleteCustomTheme: (id: string) => void;
  setCloseToTray: (enabled: boolean) => void;
  setMinimizeToTray: (enabled: boolean) => void;
  setStartup: (mode: StartupMode) => Promise<void>;
}

const CACHE_KEY = "solexa.ui.settings.v1";

const DEFAULTS: AppSettings = {
  themeId: DEFAULT_THEME_ID,
  customThemes: [],
  language: "en",
  closeToTray: false,
  minimizeToTray: false,
  startupMode: "off",
};

const BACKEND_KEYS: Record<keyof AppSettings, string> = {
  themeId: "ui.theme_id",
  customThemes: "ui.custom_themes",
  language: "ui.language",
  closeToTray: "ui.close_to_tray",
  minimizeToTray: "ui.minimize_to_tray",
  startupMode: "ui.startup_mode",
};

function parseCustomThemes(raw: string | undefined): Theme[] {
  if (!raw) return [];
  try {
    const parsed = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    return parsed
      .map((entry) => sanitizeTheme(entry as Partial<Theme>))
      .filter((entry): entry is Theme => entry !== null);
  } catch {
    return [];
  }
}

function isLanguage(value: string | undefined): value is Language {
  return value === "en" || value === "de" || value === "fa";
}

function isStartupMode(value: string | undefined): value is StartupMode {
  return value === "off" || value === "app" || value === "tray";
}

function readCache(): AppSettings {
  if (typeof window === "undefined") return DEFAULTS;
  try {
    const parsed = JSON.parse(window.localStorage.getItem(CACHE_KEY) ?? "{}");
    return {
      themeId: typeof parsed.themeId === "string" ? parsed.themeId : DEFAULTS.themeId,
      customThemes: parseCustomThemes(JSON.stringify(parsed.customThemes ?? [])),
      language: isLanguage(parsed.language) ? parsed.language : DEFAULTS.language,
      closeToTray: parsed.closeToTray === true,
      minimizeToTray: parsed.minimizeToTray === true,
      startupMode: isStartupMode(parsed.startupMode) ? parsed.startupMode : DEFAULTS.startupMode,
    };
  } catch {
    return DEFAULTS;
  }
}

const SettingsContext = createContext<SettingsContextValue | null>(null);

export function SettingsProvider({ children }: { children: ReactNode }) {
  const [settings, setSettings] = useState<AppSettings>(readCache);
  const [transientTheme, setTransientTheme] = useState<Theme | null>(null);
  const [hydrated, setHydrated] = useState(false);
  const [startupRegistered, setStartupRegistered] = useState(false);

  const activeTheme = useMemo(
    () => transientTheme ?? findTheme(settings.themeId, settings.customThemes),
    [transientTheme, settings.themeId, settings.customThemes],
  );

  // Paint the cached theme before the first frame, then ask the engine for
  // the real values (the cache can be stale after a reinstall or a clear).
  useEffect(() => {
    applyTheme(activeTheme);
  }, [activeTheme]);

  useEffect(() => {
    let cancelled = false;
    getAppSettings()
      .then((raw) => {
        if (cancelled) return;
        const next: AppSettings = { ...settings };
        if (isLanguage(raw["ui.language"])) next.language = raw["ui.language"];
        if (isStartupMode(raw["ui.startup_mode"])) next.startupMode = raw["ui.startup_mode"];
        if (raw["ui.close_to_tray"] === "true" || raw["ui.close_to_tray"] === "false") {
          next.closeToTray = raw["ui.close_to_tray"] === "true";
        }
        if (raw["ui.minimize_to_tray"] === "true" || raw["ui.minimize_to_tray"] === "false") {
          next.minimizeToTray = raw["ui.minimize_to_tray"] === "true";
        }
        if (typeof raw["ui.theme_id"] === "string" && raw["ui.theme_id"]) {
          next.themeId = raw["ui.theme_id"];
        }
        if (typeof raw["ui.custom_themes"] === "string") {
          const custom = parseCustomThemes(raw["ui.custom_themes"]);
          if (custom.length > 0 || raw["ui.custom_themes"].trim() === "[]") next.customThemes = custom;
        }
        setSettings(next);
      })
      .catch(() => {
        // A missing backend is not fatal: the cached values still apply.
      })
      .finally(() => {
        if (!cancelled) setHydrated(true);
      });
    getStartupEnabled()
      .then((enabled) => !cancelled && setStartupRegistered(enabled))
      .catch(() => {});
    return () => {
      cancelled = true;
    };
    // Deliberately runs once: this is the startup hydration step.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Cache for fast painting on the next launch.
  useEffect(() => {
    try {
      window.localStorage.setItem(CACHE_KEY, JSON.stringify(settings));
    } catch {
      // Private mode or a full disk: settings still work for this session.
    }
  }, [settings]);

  // Language also drives document direction and the tray menu labels.
  useEffect(() => {
    const root = document.documentElement;
    root.lang = settings.language;
    root.dir = settings.language === "fa" ? "rtl" : "ltr";
    setTrayMenuLanguage(settings.language).catch(() => {});
  }, [settings.language]);

  // Keep the engine's copy of the accent in sync, so the tray icon follows
  // the theme even before the next restart.
  useEffect(() => {
    setAppSetting("ui.theme_accent", activeTheme.accent).catch(() => {});
  }, [activeTheme.accent]);

  const persist = useCallback((key: keyof AppSettings, value: string) => {
    setAppSetting(BACKEND_KEYS[key], value).catch(() => {});
  }, []);

  const update = useCallback(
    (patch: Partial<AppSettings>) => setSettings((prev) => ({ ...prev, ...patch })),
    [],
  );

  const setLanguage = useCallback(
    (language: Language) => {
      update({ language });
      persist("language", language);
    },
    [persist, update],
  );

  const setThemeId = useCallback(
    (id: string) => {
      setTransientTheme(null);
      update({ themeId: id });
      persist("themeId", id);
    },
    [persist, update],
  );

  const applyTransientTheme = useCallback((theme: Theme) => {
    setTransientTheme(theme);
  }, []);

  const clearTransientTheme = useCallback(() => {
    setTransientTheme(null);
  }, []);

  const saveCustomTheme = useCallback(
    (theme: Theme) => {
      const next = [...settings.customThemes.filter((entry) => entry.id !== theme.id), theme];
      update({ customThemes: next });
      persist("customThemes", JSON.stringify(next));
      return theme;
    },
    [persist, settings.customThemes, update],
  );

  const deleteCustomTheme = useCallback(
    (id: string) => {
      const next = settings.customThemes.filter((entry) => entry.id !== id);
      update({ customThemes: next, themeId: settings.themeId === id ? DEFAULT_THEME_ID : settings.themeId });
      persist("customThemes", JSON.stringify(next));
      if (settings.themeId === id) persist("themeId", DEFAULT_THEME_ID);
      setTransientTheme((current) => (current?.id === id ? null : current));
    },
    [persist, settings.customThemes, settings.themeId, update],
  );

  const setCloseToTray = useCallback(
    (enabled: boolean) => {
      update({ closeToTray: enabled });
      persist("closeToTray", String(enabled));
    },
    [persist, update],
  );

  const setMinimizeToTray = useCallback(
    (enabled: boolean) => {
      update({ minimizeToTray: enabled });
      persist("minimizeToTray", String(enabled));
    },
    [persist, update],
  );

  const setStartup = useCallback(
    async (mode: StartupMode) => {
      await setStartupMode(mode);
      update({ startupMode: mode });
      persist("startupMode", mode);
      setStartupRegistered(mode !== "off");
    },
    [persist, update],
  );

  const value: SettingsContextValue = {
    settings,
    activeTheme,
    transientTheme,
    hydrated,
    startupRegistered,
    setLanguage,
    setThemeId,
    applyTransientTheme,
    clearTransientTheme,
    saveCustomTheme,
    deleteCustomTheme,
    setCloseToTray,
    setMinimizeToTray,
    setStartup,
  };

  return <SettingsContext.Provider value={value}>{children}</SettingsContext.Provider>;
}

export function useSettings(): SettingsContextValue {
  const value = useContext(SettingsContext);
  if (!value) throw new Error("useSettings must be used inside SettingsProvider");
  return value;
}
