import { useQuery, useQueryClient } from "@tanstack/react-query";
import { LogicalPosition, LogicalSize } from "@tauri-apps/api/dpi";
import { Webview } from "@tauri-apps/api/webview";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import * as api from "../api";
import { useUi } from "../store";
import { usePlayer } from "../player";
import { useArticleActions } from "../hooks/articleActions";
import { renderMarkdown } from "../lib/markdown";
import { fullDate } from "../lib/feedMeta";
import { isMac } from "../lib/platform";
import { openInternalBrowser } from "../lib/internalBrowser";
import { reportError, toast } from "../toast";
import type { ArticleDetail } from "../types";
import Icon from "./Icon";
import HighlightLayer from "./HighlightLayer";
import ContextMenu, { type MenuEntry } from "./ContextMenu";

interface Props {
  onToast: (msg: string) => void;
}

const READER_BROWSER_LABEL = "papr-reader-browser";
const BROWSER_PROFILE = "browser-profile";

function youtubeId(url: string | null): string | null {
  if (!url) return null;
  const m =
    url.match(/[?&]v=([\w-]{11})/) || url.match(/youtu\.be\/([\w-]{11})/);
  return m ? m[1] : null;
}

/** Plain, entity-decoded text of an HTML body — for the reading-time estimate.
 *  A bare `replace(/<[^>]+>/g, " ")` tag-strip leaves HTML entities intact, so
 *  `Tom &amp; Jerry &mdash; done` would be counted as 5 words / 28 chars when
 *  the real text ("Tom & Jerry — done") is 4 words / 18 chars — inflating the
 *  estimate on entity-heavy articles. Parsing into an inert document decodes
 *  every entity (`&amp;` → `&`, `&mdash;` → `—`) and drops markup cleanly. */
function bodyPlainText(html: string): string {
  if (!html) return "";
  // DOMParser documents are inert — nothing here executes or loads.
  return new DOMParser().parseFromString(html, "text/html").body.textContent ?? "";
}

/** CJK ideographs + Japanese kana + Korean Hangul — scripts read by the
 *  character, not the whitespace-delimited word. */
const CJK_CHAR = /[぀-ヿ㐀-鿿가-힯豈-﫿]/u;
/** Global-flagged variant of `CJK_CHAR` for stripping every CJK glyph. */
const CJK_CHAR_GLOBAL = new RegExp(CJK_CHAR.source, "gu");

/** Estimate reading time in minutes for an article body's plain text.
 *
 *  A mixed-script estimate: CJK scripts have no word spacing, so they are
 *  counted by the character (~480 chars/min); latin-script text is counted by
 *  the whitespace-delimited word (~220 wpm). The two contributions are *summed*
 *  — the previous `Math.max(words/220, chars/480)` always lost for English
 *  (a 1000-word article spans ~5500 chars, so `chars/480` ≈ 11 dwarfed the
 *  true `words/220` ≈ 4.5), inflating every latin-script article ~2-3×. */
function estimateReadMinutes(text: string): number {
  let cjkChars = 0;
  for (const ch of text) {
    if (CJK_CHAR.test(ch)) cjkChars++;
  }
  // Words, with CJK characters stripped so they are not also counted as
  // single-character "words" by the latin path.
  const latinWords = text
    .replace(CJK_CHAR_GLOBAL, " ")
    .trim()
    .split(/\s+/)
    .filter(Boolean).length;
  const minutes = cjkChars / 480 + latinWords / 220;
  return Math.max(2, Math.round(minutes));
}

/** Decode a URL fragment, tolerating a malformed `%` escape. A real-world
 *  anchor can carry a literal percent (`#100%-growth`, `#section-50%`), which
 *  is not a valid escape sequence — `decodeURIComponent` throws `URIError` on
 *  it. The bare value still works as an `id` lookup, so fall back to it rather
 *  than letting the throw escape the click handler and kill the link. */
function decodeFragment(frag: string): string {
  try {
    return decodeURIComponent(frag);
  } catch {
    return frag;
  }
}

/** Pull the in-page fragment out of a link click, or null if it isn't one.
 *
 *  Two shapes count as in-page: a bare `#frag` href, and — because the body
 *  HTML is sanitized with the article's URL as the rewrite base — an absolute
 *  `https://site/article#frag` that resolves to the very article being read.
 *  `sourceUrl` is the article's own URL, used to recognise that second case.
 */
function inPageFragment(raw: string, sourceUrl: string | null): string | null {
  if (raw[0] === "#") return decodeFragment(raw.slice(1));
  if (!sourceUrl) return null;
  try {
    const u = new URL(raw);
    const b = new URL(sourceUrl);
    if (u.hash && u.origin === b.origin && u.pathname === b.pathname) {
      return decodeFragment(u.hash.slice(1));
    }
  } catch {
    /* not a parseable absolute URL — treat as external */
  }
  return null;
}

/** Build a click handler for links inside injected HTML (article body, AI
 *  summary). In-page anchor links (footnotes, tables of contents) scroll to
 *  their target within the reader; everything else opens in the built-in
 *  browser — a bare <a> click would otherwise navigate the Tauri webview away
 *  from the app entirely (or, for a fragment link, to a bogus `app://…#frag`). */
function waitForWebview(view: Webview): Promise<void> {
  return new Promise<void>((resolve, reject) => {
    let settled = false;
    const finish = (fn: () => void) => {
      if (settled) return;
      settled = true;
      fn();
    };
    view.once("tauri://created", () => finish(resolve)).catch(reject);
    view
      .once("tauri://error", (event) =>
        finish(() => reject(new Error(String(event.payload)))),
      )
      .catch(reject);
  });
}

function canEmbedUrl(url: string): boolean {
  return /^https?:\/\//i.test(url);
}

function makeLinkClickHandler(
  sourceUrl: string | null,
  openUrl: (url: string) => void = (url) =>
    openInternalBrowser(url).catch(reportError),
) {
  return (e: React.MouseEvent) => {
    const link = (e.target as HTMLElement).closest("a");
    if (!link) return;
    const raw = link.getAttribute("href");
    if (!raw) return;
    e.preventDefault();

    const hash = inPageFragment(raw, sourceUrl);
    if (hash != null) {
      if (hash === "") return; // bare `#` — no element to reach
      const root = link.closest(".article-body, .ai-prose");
      // getElementById can't be scoped to the body, so match by id or the
      // legacy `<a name>` form within the rendered content.
      const target = root?.querySelector(
        `[id="${CSS.escape(hash)}"], a[name="${CSS.escape(hash)}"]`,
      );
      target?.scrollIntoView({ behavior: "smooth", block: "start" });
      return;
    }

    openUrl(link.href);
  };
}

export default function Reader({ onToast }: Props) {
  const { t } = useTranslation();
  const actions = useArticleActions(toast.error);
  const id = useUi((s) => s.selectedArticleId);
  const focusMode = useUi((s) => s.focusMode);
  const setFocusMode = useUi((s) => s.setFocusMode);
  const aiOpen = useUi((s) => s.aiOpen);
  const setAiOpen = useUi((s) => s.setAiOpen);
  const showReadingTime = useUi((s) => s.prefs.showReadingTime);

  const [scrolled, setScrolled] = useState(false);
  const [ctxMenu, setCtxMenu] = useState<{ x: number; y: number } | null>(null);
  const [heroBroken, setHeroBroken] = useState(false);
  const [progress, setProgress] = useState(0);
  const [browserUrl, setBrowserUrl] = useState<string | null>(null);
  const scrollRef = useRef<HTMLDivElement>(null);
  const bodyRef = useRef<HTMLDivElement>(null);
  const browserHostRef = useRef<HTMLDivElement>(null);
  const browserViewRef = useRef<Webview | null>(null);
  const playTrack = usePlayer((s) => s.play);
  const playingSrc = usePlayer((s) => (s.playing ? s.track?.src : null));

  const article = useQuery({
    queryKey: ["article", id],
    queryFn: () => api.getArticle(id as number),
    enabled: id != null,
  });
  const a: ArticleDetail | undefined = article.data;

  const readMinutes = useMemo(() => {
    return estimateReadMinutes(bodyPlainText(a?.contentHtml || ""));
  }, [a?.contentHtml]);

  // Reset reader view on article change.
  useEffect(() => {
    setAiOpen(false);
    setBrowserUrl(null);
    setScrolled(false);
    setHeroBroken(false);
    setProgress(0);
    if (scrollRef.current) scrollRef.current.scrollTop = 0;
  }, [id, setAiOpen]);

  const openBrowserPane = useCallback((url: string) => {
    if (!canEmbedUrl(url)) {
      openInternalBrowser(url).catch(reportError);
      return;
    }
    setAiOpen(false);
    setScrolled(false);
    setProgress(0);
    setBrowserUrl(url);
  }, [setAiOpen]);

  const closeBrowserPane = useCallback(() => {
    setBrowserUrl(null);
  }, []);

  useEffect(() => {
    if (!browserUrl) {
      const current = browserViewRef.current;
      browserViewRef.current = null;
      current?.close().catch(() => {});
      return;
    }

    let cancelled = false;
    let cleanupLayout = () => {};

    const setBounds = async (view: Webview, host: HTMLDivElement) => {
      const rect = host.getBoundingClientRect();
      await view.setPosition(
        new LogicalPosition(Math.max(0, Math.round(rect.left)), Math.max(0, Math.round(rect.top))),
      );
      await view.setSize(
        new LogicalSize(Math.max(320, Math.round(rect.width)), Math.max(240, Math.round(rect.height))),
      );
    };

    const mount = async () => {
      const host = browserHostRef.current;
      if (!host) return;

      const existing = await Webview.getByLabel(READER_BROWSER_LABEL);
      await existing?.close().catch(() => {});
      if (cancelled) return;

      const rect = host.getBoundingClientRect();
      const view = new Webview(getCurrentWindow(), READER_BROWSER_LABEL, {
        url: browserUrl,
        x: Math.max(0, Math.round(rect.left)),
        y: Math.max(0, Math.round(rect.top)),
        width: Math.max(320, Math.round(rect.width)),
        height: Math.max(240, Math.round(rect.height)),
        focus: true,
        dataDirectory: BROWSER_PROFILE,
      });
      browserViewRef.current = view;
      await waitForWebview(view);
      if (cancelled) {
        await view.close().catch(() => {});
        return;
      }
      await view.setFocus().catch(() => {});

      const resize = () => setBounds(view, host).catch(() => {});
      const observer = new ResizeObserver(resize);
      observer.observe(host);
      window.addEventListener("resize", resize);
      cleanupLayout = () => {
        observer.disconnect();
        window.removeEventListener("resize", resize);
      };
      resize();
    };

    mount().catch(reportError);

    return () => {
      cancelled = true;
      cleanupLayout();
      const current = browserViewRef.current;
      if (current?.label === READER_BROWSER_LABEL) {
        browserViewRef.current = null;
        current.close().catch(() => {});
      }
    };
  }, [browserUrl]);

  // Hide article-body images that fail to load — a broken-image icon in the
  // middle of an article is just noise. Runs whenever the body changes.
  useEffect(() => {
    const el = bodyRef.current;
    if (!el) return;
    const hide = (e: Event) => {
      (e.currentTarget as HTMLElement).style.display = "none";
    };
    const watched: HTMLImageElement[] = [];
    el.querySelectorAll("img").forEach((img) => {
      if (img.complete && img.naturalWidth === 0) {
        img.style.display = "none";
      } else {
        img.addEventListener("error", hide);
        watched.push(img);
      }
    });
    return () => watched.forEach((img) => img.removeEventListener("error", hide));
  }, [a?.id, a?.contentHtml]);

  const onScroll = () => {
    const el = scrollRef.current;
    if (!el) return;
    setScrolled(el.scrollTop > 8);
    const max = el.scrollHeight - el.clientHeight;
    setProgress(max > 0 ? Math.min(1, el.scrollTop / max) : 0);
  };


  const copyLink = () => {
    if (!a?.url) return;
    navigator.clipboard.writeText(a.url).then(() => onToast(t("reader.linkCopied")), () => {});
  };
  if (id == null) {
    const kbd = {
      fontFamily: "var(--mono)",
      fontSize: 10,
      padding: "1px 5px",
      border: "1px solid var(--hair)",
      borderRadius: 3,
    };
    return (
      <div className="reader" role="main">
        {isMac && <div className="reader-toolbar" data-tauri-drag-region />}
        <div className="empty" style={{ flex: 1 }}>
          <div className="glyph">
            <Icon name="rss" size={22} />
          </div>
          <div>{t("reader.emptySelectArticle")}</div>
          <div style={{ fontSize: 11.5, color: "var(--muted-2)" }}>
            {t("reader.emptyHintPrefix")} <kbd style={kbd}>J</kbd> /{" "}
            <kbd style={kbd}>K</kbd> {t("reader.emptyHintSuffix")}
          </div>
        </div>
      </div>
    );
  }

  // An article is selected but its detail isn't loaded yet — still fetching
  // or the fetch failed. Surface that explicitly instead of falling through
  // to the "select an article" empty state, which would be misleading.
  if (!a) {
    return (
      <div className="reader" role="main">
        {isMac && <div className="reader-toolbar" data-tauri-drag-region />}
        {article.isError ? (
          <div className="empty" style={{ flex: 1 }}>
            <div className="glyph">
              <Icon name="alert" size={22} />
            </div>
            <div>{t("reader.loadError")}</div>
            <button
              className="empty-retry"
              onClick={() => article.refetch()}
              disabled={article.isFetching}
            >
              <Icon name="refresh" size={12} />
              {t("common.retry")}
            </button>
          </div>
        ) : (
          <div className="reader-scroll">
            <div className="article reader-content" aria-hidden="true">
              <div className="sk-line" style={{ width: "28%" }} />
              <div
                className="sk-line"
                style={{ width: "82%", height: 24, marginBottom: 18 }}
              />
              <div
                className="sk-line"
                style={{ width: "44%", marginBottom: 30 }}
              />
              {Array.from({ length: 9 }).map((_, i) => (
                <div
                  key={i}
                  className="sk-line"
                  style={{ width: i % 3 === 2 ? "58%" : "100%", height: 12 }}
                />
              ))}
            </div>
          </div>
        )}
      </div>
    );
  }

  const body = a.contentHtml || "";

  const ytId = a.sourceType === "youtube" ? youtubeId(a.url) : null;

  return (
    <div className="reader" role="main">
      <div
        className={`reader-toolbar ${scrolled ? "scrolled" : ""}`}
        {...(isMac && { "data-tauri-drag-region": true })}
      >
        <button
          className="tb-btn"
          onClick={() => actions.setRead(a.id, !a.isRead)}
          title={
            a.isRead
              ? t("articleList.menuMarkUnread")
              : t("articleList.menuMarkRead")
          }
          aria-label={
            a.isRead
              ? t("articleList.menuMarkUnread")
              : t("articleList.menuMarkRead")
          }
        >
          <Icon name={a.isRead ? "circle" : "check"} size={16} />
        </button>
        <button
          className={`tb-btn ${aiOpen ? "on" : ""}`}
          onClick={() => {
            if (browserUrl) closeBrowserPane();
            setAiOpen(!aiOpen);
          }}
          title={t("reader.tbAiSummary")}
          aria-label={t("reader.tbAiSummary")}
          aria-pressed={aiOpen}
        >
          <Icon name={aiOpen ? "sparkle-fill" : "sparkle"} size={16} />
        </button>
        <button
          className="tb-btn"
          title={t("reader.tbOpenInBrowser")}
          aria-label={t("reader.tbOpenInBrowser")}
          onClick={() => a.url && openBrowserPane(a.url)}
          disabled={!a.url}
        >
          <Icon name="open" size={16} />
        </button>
        <HighlightLayer
          // Keyed by article id so the export menu / popovers reset cleanly
          // when the reader switches articles.
          key={a.id}
          articleId={a.id}
          bodyRef={bodyRef}
          bodyVersion={body}
        />
        <div className="tb-btn spacer" />
      </div>

      <div className="read-progress-track" aria-hidden="true">
        <div
          className="read-progress"
          style={{ transform: `scaleX(${progress})` }}
        />
      </div>

      {browserUrl ? (
        <div className="reader-browser">
          <div className="reader-browser-bar">
            <button
              className="reader-browser-back"
              onClick={closeBrowserPane}
              title={t("reader.backToArticle")}
              aria-label={t("reader.backToArticle")}
            >
              <Icon name="chevron-right" size={14} className="back-icon" />
              <span>{t("reader.backToArticle")}</span>
            </button>
            <span className="reader-browser-url" title={browserUrl}>
              {browserUrl}
            </span>
          </div>
          <div className="reader-browser-host" ref={browserHostRef} />
        </div>
      ) : (
        <div
          className="reader-scroll"
          ref={scrollRef}
          onScroll={onScroll}
          onContextMenu={(e) => {
            e.preventDefault();
            setCtxMenu({ x: e.clientX, y: e.clientY });
          }}
        >
          <article className="article reader-content" key={a.id}>
          <span className="article-feed">
            <Icon name="rss" size={13} />
            {a.feedTitle}
          </span>
          <h1 className="article-title">{a.title}</h1>
          <div className="article-meta">
            {a.author && <span className="author">{a.author}</span>}
            {a.author && a.publishedAt && <span>·</span>}
            {a.publishedAt && <span>{fullDate(a.publishedAt)}</span>}
            {showReadingTime && (
              <>
                <span>·</span>
                <span>{t("reader.readMinutes", { count: readMinutes })}</span>
              </>
            )}
          </div>

          {ytId ? (
            <iframe
              style={{ width: "100%", aspectRatio: "16 / 9" }}
              // Privacy-enhanced host: YouTube sets no tracking cookies
              // until the viewer actually starts the video.
              src={`https://www.youtube-nocookie.com/embed/${ytId}`}
              title={a.title}
              referrerPolicy="strict-origin-when-cross-origin"
              allowFullScreen
            />
          ) : (
            a.imageUrl &&
            !heroBroken &&
            // Skip the hero when the body already embeds the same image, so
            // feeds that repeat their lead image don't show it twice.
            !body.includes(a.imageUrl) && (
              <img
                className="article-hero"
                src={a.imageUrl}
                alt=""
                onError={() => setHeroBroken(true)}
              />
            )
          )}

          {a.enclosures
            .filter((e) => e.mimeType?.startsWith("audio"))
            .map((e, i) => {
              const isPlaying = playingSrc === e.url;
              return (
                <button
                  className={`episode ${isPlaying ? "playing" : ""}`}
                  key={`a${i}`}
                  onClick={() =>
                    playTrack({
                      articleId: a.id,
                      title: a.title,
                      feedTitle: a.feedTitle,
                      src: e.url,
                    })
                  }
                >
                  <span className="episode-play">
                    <Icon name={isPlaying ? "pause" : "play"} size={15} />
                  </span>
                  <span className="episode-text">
                    {isPlaying
                      ? t("reader.episodePlaying")
                      : t("reader.episodePlay")}
                  </span>
                </button>
              );
            })}
          {a.enclosures
            .filter((e) => e.mimeType?.startsWith("video"))
            .map((e, i) => (
              <div className="enclosure" key={`v${i}`}>
                <video controls src={e.url} />
              </div>
            ))}

          <div
            className="article-body"
            ref={bodyRef}
            onClick={makeLinkClickHandler(a.url, openBrowserPane)}
            dangerouslySetInnerHTML={{
              __html: body || `<p><em>${t("reader.noContent")}</em></p>`,
            }}
          />
          </article>
        </div>
      )}

      <AIDrawer
        // Keyed by article id so switching articles remounts the drawer:
        // its `text` state then re-initialises from the new article's
        // summary, rather than carrying the previous one's across.
        key={a.id}
        open={aiOpen}
        article={a}
        onClose={() => setAiOpen(false)}
      />

      {ctxMenu && (
        <ContextMenu
          x={ctxMenu.x}
          y={ctxMenu.y}
          items={[
            {
              icon: aiOpen ? "sparkle-fill" : "sparkle",
              label: t("reader.tbAiSummary"),
              onClick: () => setAiOpen(!aiOpen),
            },
            ...(a.url
              ? [
                  {
                    icon: "open",
                    label: t("reader.tbOpenInBrowser"),
                    onClick: () => openBrowserPane(a.url!),
                  },
                ]
              : []),
            { separator: true },
            ...(a.url
              ? [{ icon: "copy", label: t("reader.tbCopyLink"), onClick: copyLink }]
              : []),
            { separator: true },
            {
              icon: focusMode ? "eye-off" : "focus",
              label: t("reader.tbFocusMode"),
              onClick: () => setFocusMode(!focusMode),
            },
          ] as MenuEntry[]}
          onClose={() => setCtxMenu(null)}
        />
      )}
    </div>
  );
}

function AIDrawer({
  open,
  article,
  onClose,
}: {
  open: boolean;
  article: ArticleDetail;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const qc = useQueryClient();
  // Initialised from the article's stored summary (if any). The parent keys
  // this component by article id, so a switch remounts it and re-runs this
  // initialiser — no separate "reset on article change" effect is needed.
  const [text, setText] = useState<string | null>(article.aiSummary);
  const [busy, setBusy] = useState(false);
  const [failed, setFailed] = useState(false);
  const [retry, setRetry] = useState(0);
  // Identifies the latest summarize run. Closing the drawer mid-stream cancels
  // an effect run but the component stays mounted (it is only moved off-screen),
  // so the underlying request keeps streaming and its promise settles later.
  // Only the run whose generation still matches may touch `busy` on settle —
  // otherwise a stale run's `finally` would either wedge the drawer on the
  // loading state or clobber a newer run's `busy` flag.
  const runRef = useRef(0);

  // Generate a summary the first time the drawer opens for an article, and
  // again whenever the user hits Retry. `failed` is in the guard so a failed
  // run isn't silently re-attempted just because the drawer was reopened.
  useEffect(() => {
    if (!open || busy || text || failed) return;
    const run = ++runRef.current;
    let cancelled = false;
    // Whether the stream settled (resolved or rejected) on its own. If the
    // cleanup runs while this is still false, the drawer was closed mid-stream
    // — the accumulated `text` is then a truncated fragment.
    let settled = false;
    // An error raised inside the stream surfaces twice: once as an `error`
    // channel event (carrying the precise provider message) and again as the
    // command's rejected promise. Toast only the first so the user does not
    // see the same failure reported twice; the `.catch` still toasts for
    // failures that abort before streaming starts (no key, bad config) and so
    // never emit an `error` event.
    let sawErrorEvent = false;
    setBusy(true);
    setText("");
    api
      .aiSummarize(article.id, (ev) => {
        if (cancelled) return;
        if (ev.type === "delta") setText((s) => (s ?? "") + ev.data);
        else if (ev.type === "error") {
          sawErrorEvent = true;
          setFailed(true);
          toast.error(ev.data);
        }
      })
      .then(() => {
        if (!cancelled) qc.invalidateQueries({ queryKey: ["article", article.id] });
      })
      .catch((e) => {
        if (!cancelled && !sawErrorEvent) {
          setFailed(true);
          reportError(e);
        }
      })
      .finally(() => {
        settled = true;
        // Clear `busy` for the current run even if it was cancelled — the
        // component is still mounted, and leaving `busy` true would wedge the
        // drawer on the loading state. Skip if a newer run has superseded us.
        if (runRef.current === run) setBusy(false);
      });
    return () => {
      cancelled = true;
      // Closed mid-stream: the backend discards an interrupted generation
      // (it is never persisted), so drop the partial fragment held here too.
      // Reopening then re-generates from scratch instead of showing — and
      // permanently freezing on — a truncated half-summary.
      if (!settled) setText(article.aiSummary);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, article.id, retry]);

  const loading = busy && !text;
  const onRetry = () => {
    setText("");
    setFailed(false);
    setRetry((n) => n + 1);
  };
  // Parse + sanitize the summary only when the text changes, not on every
  // AIDrawer re-render (e.g. each open/close toggle).
  const html = useMemo(() => (text ? renderMarkdown(text) : ""), [text]);

  return (
    <div
      className={`ai-drawer ${open ? "open" : ""}`}
      // A labelled complementary landmark so screen-reader users can jump
      // straight to the summary.
      role="complementary"
      aria-label={t("reader.aiSummaryTitle")}
      // When closed the drawer is only moved off-screen — `inert` keeps its
      // close button and content out of the tab order and the a11y tree.
      inert={!open}
    >
      <div className="ai-head">
        <span className="accent-ico">
          <Icon name="sparkle-fill" size={15} />
        </span>
        <h3>{t("reader.aiSummaryTitle")}</h3>
        <button
          className={`tb-btn ${busy ? "spinning" : ""}`}
          onClick={onRetry}
          disabled={busy}
          title={t("reader.aiRegenerate")}
          aria-label={t("reader.aiRegenerate")}
        >
          <Icon name="refresh" size={14} />
        </button>
        <button
          className="tb-btn close"
          onClick={onClose}
          title={t("common.close")}
          aria-label={t("common.close")}
        >
          <Icon name="x" size={14} />
        </button>
      </div>
      <div className="ai-body" aria-live="polite" aria-busy={busy}>
        {loading && (
          <div className="ai-loading">
            <span className="ai-dot" />
            <span className="ai-dot" />
            <span className="ai-dot" />
            <span style={{ marginLeft: 4 }}>{t("reader.aiReadingFullText")}</span>
          </div>
        )}
        {failed && !busy && (
          <div className="ai-error">
            <Icon name="alert" size={18} />
            <span>{t("reader.aiError")}</span>
            <button className="empty-retry" onClick={onRetry}>
              <Icon name="refresh" size={12} />
              {t("common.retry")}
            </button>
          </div>
        )}
        {text && !failed && (
          <>
            <div
              className="ai-prose"
              onClick={makeLinkClickHandler(article.url)}
              dangerouslySetInnerHTML={{ __html: html }}
            />
            <div
              style={{
                fontSize: 11,
                color: "var(--muted-2)",
                marginTop: 24,
                lineHeight: 1.5,
              }}
            >
              {t("reader.aiDisclaimer")}
            </div>
          </>
        )}
      </div>
    </div>
  );
}
