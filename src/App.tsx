import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { useQueryClient } from "@tanstack/react-query";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import * as api from "./api";
import { useUi, READER_FONTS } from "./store";
import { useArticleActions } from "./hooks/articleActions";
import { readCurrentItems } from "./lib/currentList";
import { openInternalBrowser } from "./lib/internalBrowser";
import { useToasts, toast as toastApi, reportError } from "./toast";
import type { ArticleSummary, Feed } from "./types";
import ArticleList from "./components/ArticleList";
import Reader from "./components/Reader";
import CommandPalette, { type CommandAction } from "./components/CommandPalette";
import SettingsDialog from "./components/SettingsDialog";
import PlayerBar from "./components/PlayerBar";
import Icon from "./components/Icon";

export default function App() {
  const { t } = useTranslation();
  const qc = useQueryClient();

  const theme = useUi((s) => s.theme);
  const density = useUi((s) => s.density);
  const readerFont = useUi((s) => s.readerFont);
  const readerSize = useUi((s) => s.readerSize);
  const readerLeading = useUi((s) => s.readerLeading);
  const readerWidth = useUi((s) => s.readerWidth);
  const reduceMotion = useUi((s) => s.prefs.reduceMotion);
  const focusMode = useUi((s) => s.focusMode);

  const activeToast = useToasts((s) => s.current);
  const dismissToast = useToasts((s) => s.dismiss);
  const [refreshing, setRefreshing] = useState(false);
  const [cpOpen, setCpOpen] = useState(false);
  const [settings, setSettings] = useState<{ open: boolean; section?: string }>({
    open: false,
  });

  // ── apply appearance to the document root ──
  useEffect(() => {
    const root = document.documentElement;
    root.dataset.theme = theme;
    root.dataset.density = density;
    const dark = theme === "dark";
    // Keep the native window/webview background on the themed paper colour, so
    // a live window resize never flashes a mismatched colour in the strip the
    // webview has not repainted yet. Mirrors --paper in styles.css.
    getCurrentWindow()
      .setBackgroundColor(dark ? "#121212" : "#F7F7F7")
      .catch(() => {});
  }, [theme, density]);

  // ── dismiss the boot splash once the app shell has mounted ──
  useEffect(() => {
    const el = document.getElementById("app-loading");
    if (!el) return;
    el.classList.add("hide");
    const timer = window.setTimeout(() => el.remove(), 360);
    return () => window.clearTimeout(timer);
  }, []);

  useEffect(() => {
    document.documentElement.dataset.reduceMotion = String(reduceMotion);
  }, [reduceMotion]);

  // Always start in the FreshRSS unread queue: unread only, oldest first.
  useEffect(() => {
    const st = useUi.getState();
    st.select({ kind: "unread" }, t("smart.unread"));
    if (!st.unreadOnly) st.toggleUnreadOnly();
    if (!st.sortOldest) st.toggleSort();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    const root = document.documentElement.style;
    const font = READER_FONTS[readerFont];
    root.setProperty("--reader-font", font.stack);
    root.setProperty("--reader-font-adjust", font.adjust);
    root.setProperty("--reader-size", `${readerSize}px`);
    root.setProperty("--reader-leading", String(readerLeading / 100));
    root.setProperty("--reader-width", `${readerWidth}px`);
  }, [readerFont, readerSize, readerLeading, readerWidth]);

  // ── toast ──
  // The store owns the queue; App owns only the dwell timer and the render.
  const showToast = toastApi.show;
  useEffect(() => {
    if (!activeToast) return;
    const timer = window.setTimeout(
      () => dismissToast(activeToast.id),
      activeToast.duration,
    );
    return () => window.clearTimeout(timer);
  }, [activeToast, dismissToast]);

  // Article-action failures route to an error toast, not a silent default one.
  const actions = useArticleActions(toastApi.error);

  // ── background refresh events from the Rust scheduler ──
  useEffect(() => {
    const un = listen("feeds-updated", () => {
      qc.invalidateQueries({ queryKey: ["feeds"] });
      qc.invalidateQueries({ queryKey: ["folders"] });
      qc.invalidateQueries({ queryKey: ["counts"] });
      qc.invalidateQueries({ queryKey: ["articles"] });
    });
    return () => {
      un.then((f) => f());
    };
  }, [qc]);

  // ── "Settings…" from the menu-bar tray ──
  useEffect(() => {
    const un = listen("tray-open-settings", () => setSettings({ open: true }));
    return () => {
      un.then((f) => f());
    };
  }, []);

  // A ref — not the `refreshing` state — is the concurrency guard: it must be
  // read-and-set synchronously, and the kick-off has side effects (a network
  // refresh, a toast). A setState updater must stay pure; React invokes it
  // twice under StrictMode, which previously fired the refresh twice in dev.
  // `refreshing` state is kept purely to drive the list-header refresh spinner.
  const refreshingRef = useRef(false);
  const doRefresh = useCallback(() => {
    if (refreshingRef.current) return;
    refreshingRef.current = true;
    setRefreshing(true);
    showToast(t("app.refreshing"));
    api
      .freshrssSync()
      .then((result) => {
        actions.refreshAfterFetch();
        showToast(
          result.newArticles > 0
            ? t("app.foundNew", { count: result.newArticles })
            : t("app.upToDate"),
        );
      })
      .catch(reportError)
      .finally(() => {
        refreshingRef.current = false;
        setRefreshing(false);
      });
  }, [actions, showToast, t]);

  const markAllRead = useCallback(async () => {
    try {
      const n = await api.markAllRead(useUi.getState().query);
      actions.refreshAfterBulk();
      showToast(n > 0 ? t("app.markedRead", { count: n }) : t("app.nothingToMark"));
    } catch (e) {
      reportError(e);
    }
  }, [actions, showToast, t]);

  const openSettings = (section?: string) => setSettings({ open: true, section });

  // ── command-palette actions ──
  const handleCommand = (action: CommandAction) => {
    switch (action) {
      case "mark-all-read": markAllRead(); break;
      case "toggle-theme":
        useUi.getState().setTheme(theme === "light" ? "dark" : "light");
        break;
      case "toggle-focus":
        useUi.getState().setFocusMode(!useUi.getState().focusMode);
        break;
      case "toggle-ai":
        if (useUi.getState().selectedArticleId != null)
          useUi.getState().setAiOpen(!useUi.getState().aiOpen);
        break;
      case "refresh": doRefresh(); break;
      case "open-settings": openSettings(); break;
    }
  };

  const navigateFeed = (feed: Feed) => {
    useUi.getState().select({ kind: "feed", value: feed.id }, feed.title);
  };
  const navigateArticle = (a: ArticleSummary) => {
    useUi.getState().select({ kind: "feed", value: a.feedId }, a.feedTitle);
    useUi.getState().openArticle(a.id);
  };

  // ── global keyboard shortcuts (design app.jsx parity) ──
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const tag = (e.target as HTMLElement)?.tagName;
      const inField = tag === "INPUT" || tag === "TEXTAREA";
      const mod = e.metaKey || e.ctrlKey;

      // The modifier-key shortcuts (⌘K / ⌘, / ⌘R) are *application-global* —
      // they must fire regardless of where focus sits. The INPUT/TEXTAREA
      // guard below only suppresses the single-key list/reader shortcuts so a
      // plain "j" typed into a search box doesn't navigate; it must not block
      // a ⌘-combo. Crucially, the command palette and Settings each own a
      // focused text field, so gating these on focus would make ⌘K / ⌘, fail
      // to *close* their own dialog — the one path Escape isn't the only key
      // for.

      // ⌘K / ⌘, open their own modal. Firing them while another modal is
      // already open would stack a second dialog on top — two focus traps
      // then fight over the keyboard, and dismissing the inner one drops
      // focus to nowhere. So suppress the *open* half when a blocking modal
      // is up; the *close* (toggle-off) half stays live so ⌘K still shuts
      // the command palette and ⌘, still shuts Settings.
      if (mod && e.key.toLowerCase() === "k") {
        e.preventDefault();
        const cpOpen = !!document.querySelector(".cp-backdrop");
        if (
          !cpOpen &&
          document.querySelector(
            ".settings-backdrop, .modal-backdrop, .tag-picker, .hl-popover",
          )
        )
          return;
        setCpOpen((o) => !o);
        return;
      }
      if (mod && e.key === ",") {
        e.preventDefault();
        const settingsOpen = !!document.querySelector(".settings-backdrop");
        if (
          !settingsOpen &&
          document.querySelector(
            ".cp-backdrop, .modal-backdrop, .tag-picker, .hl-popover",
          )
        )
          return;
        setSettings((s) => ({ open: !s.open }));
        return;
      }
      if (mod && e.key.toLowerCase() === "r") {
        e.preventDefault();
        doRefresh();
        return;
      }
      if (mod) return;

      // Past this point only the single-key list/reader shortcuts remain —
      // a bare "j" / "s" / "a" etc. Those must never fire while the user is
      // typing into a text field, so bail once the modifier combos above
      // have had their chance.
      if (inField) return;

      // Skip list/reader shortcuts while any overlay owns the keyboard.
      // `.hl-popover` is the highlight edit dialog inside the reader and
      // `.hl-toolbar` is the floating colour toolbar shown when text is
      // selected: without them here, j/k would navigate away (destroying the
      // overlay — and, for the toolbar, the live selection the user was about
      // to highlight), s/u/b would act on the article, and Escape would close
      // the AI drawer instead of just the overlay.
      if (
        document.querySelector(
          ".cp-backdrop, .settings-backdrop, .modal-backdrop, .ctx-menu, .tag-picker, .hl-popover, .hl-toolbar",
        )
      )
        return;

      const st = useUi.getState();

      const items = readCurrentItems(qc);
      const idx = items.findIndex((a) => a.id === st.selectedArticleId);
      const sel = idx >= 0 ? items[idx] : undefined;
      const go = (delta: number) => {
        if (items.length === 0) return;
        const next = items[Math.min(items.length - 1, Math.max(0, idx + delta))];
        if (next) st.openArticle(next.id);
      };

      switch (e.key.toLowerCase()) {
        case "j": e.preventDefault(); go(idx < 0 ? 0 : 1); break;
        case "k": e.preventDefault(); go(-1); break;
        case "o":
          if (sel?.url) {
            e.preventDefault();
            openInternalBrowser(sel.url, sel.title).catch(reportError);
          }
          break;
        case "u":
          if (sel) { e.preventDefault(); actions.setRead(sel.id, !sel.isRead); }
          break;
        case "i":
          if (st.selectedArticleId != null) {
            e.preventDefault();
            st.setAiOpen(!st.aiOpen);
          }
          break;
        case "f": e.preventDefault(); st.setFocusMode(!st.focusMode); break;
        case "a":
          if (e.shiftKey) { e.preventDefault(); markAllRead(); }
          break;
        case "d":
          if (e.shiftKey) {
            e.preventDefault();
            st.setTheme(st.theme === "light" ? "dark" : "light");
          }
          break;
        case "escape":
          st.setFocusMode(false);
          st.setAiOpen(false);
          break;
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // `t` is listed so the shortcut toasts re-bind after a language change.
    // `cpOpen` is intentionally absent — the handler only ever calls
    // setCpOpen (a functional update), so it doesn't depend on the value;
    // listing it would needlessly re-bind the listener on every ⌘K.
  }, [qc, actions, doRefresh, markAllRead, showToast, t]);

  return (
    <>
      <div className="app-shell">
        <div className={`window ${focusMode ? "focus" : ""}`}>
          <ArticleList
            onToast={showToast}
            onRefresh={doRefresh}
            refreshing={refreshing}
            onOpenSettings={() => openSettings()}
          />
          <Reader onToast={showToast} />
        </div>
        <PlayerBar />
      </div>

      <CommandPalette
        open={cpOpen}
        onClose={() => setCpOpen(false)}
        onAction={handleCommand}
        onNavigateFeed={navigateFeed}
        onNavigateArticle={navigateArticle}
      />

      {settings.open && (
        <SettingsDialog
          onClose={() => setSettings({ open: false })}
          onToast={showToast}
          initialSection={settings.section}
          onAddFeed={() => setSettings({ open: false })}
        />
      )}

      {/* A live region so screen readers announce each toast; the toast
          itself is position: fixed, so the wrapper adds no layout. */}
      <div role="status" aria-live="polite">
        {activeToast && (
          <div
            className={`toast${activeToast.tone === "error" ? " toast-error" : ""}`}
            key={activeToast.id}
          >
            {activeToast.tone === "error" && (
              <span className="toast-ico" aria-hidden="true">
                <Icon name="alert" size={14} />
              </span>
            )}
            <span className="toast-text">{activeToast.text}</span>
            {activeToast.kbd && <kbd aria-hidden="true">{activeToast.kbd}</kbd>}
            {activeToast.action && (
              <button
                className="toast-action"
                onClick={() => {
                  activeToast.action!.run();
                  dismissToast(activeToast.id);
                }}
              >
                {activeToast.action.label}
              </button>
            )}
            {(activeToast.tone === "error" || activeToast.action) && (
              <button
                className="toast-dismiss"
                aria-label={t("common.close")}
                onClick={() => dismissToast(activeToast.id)}
              >
                <Icon name="x" size={13} />
              </button>
            )}
          </div>
        )}
      </div>
    </>
  );
}
