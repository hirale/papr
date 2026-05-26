import { useInfiniteQuery, useQuery } from "@tanstack/react-query";
import { useVirtualizer } from "@tanstack/react-virtual";
import { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import * as api from "../api";
import { useUi } from "../store";
import { useArticleActions } from "../hooks/articleActions";
import { relTime } from "../lib/feedMeta";
import { isMac, modCombo } from "../lib/platform";
import { openInternalBrowser } from "../lib/internalBrowser";
import { reportError, toast } from "../toast";
import type { ArticleSummary, Feed } from "../types";
import Icon from "./Icon";
import ContextMenu, { type MenuEntry } from "./ContextMenu";

const PAGE = 60;

interface Props {
  onToast: (msg: string) => void;
  onRefresh: () => void;
  refreshing: boolean;
  onOpenSettings: () => void;
}

export default function ArticleList({
  onToast,
  onRefresh,
  refreshing,
  onOpenSettings,
}: Props) {
  const { t } = useTranslation();
  const actions = useArticleActions(toast.error);
  const query = useUi((s) => s.query);
  const queryLabel = useUi((s) => s.queryLabel);
  const viewMode = useUi((s) => s.viewMode);
  const density = useUi((s) => s.density);
  const showCardThumbs = useUi((s) => s.prefs.showCardThumbs);
  const selectedId = useUi((s) => s.selectedArticleId);
  const openArticle = useUi((s) => s.openArticle);
  const unreadOnly = true;
  const sortOldest = true;

  const feeds = useQuery({ queryKey: ["feeds"], queryFn: api.listFeeds });
  const feedById = useMemo(() => {
    const m: Record<number, Feed> = {};
    for (const f of feeds.data ?? []) m[f.id] = f;
    return m;
  }, [feeds.data]);

  const [menu, setMenu] = useState<{
    x: number;
    y: number;
    article: ArticleSummary;
  } | null>(null);

  const browse = useInfiniteQuery({
    queryKey: ["articles", query, unreadOnly, sortOldest],
    initialPageParam: 0,
    queryFn: ({ pageParam }) =>
      api.listArticles(query, unreadOnly, null, sortOldest, PAGE, pageParam as number),
    getNextPageParam: (last, all) =>
      last.length < PAGE ? undefined : all.length * PAGE,
  });

  const items: ArticleSummary[] = useMemo(
    () => browse.data?.pages.flat() ?? [],
    [browse.data],
  );

  const scrollRef = useRef<HTMLDivElement>(null);
  const rowEstimate =
    viewMode === "card"
      ? 320
      : density === "compact"
        ? 112
        : density === "spacious"
          ? 164
          : 136;
  const virt = useVirtualizer({
    count: items.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => rowEstimate,
    overscan: 8,
  });

  // Load the next page as the end approaches. Keyed on the last visible index
  // (a primitive) rather than `getVirtualItems()` — which returns a fresh
  // array every render and would re-run this effect unconditionally.
  useEffect(() => {
    const last = virt.getVirtualItems().at(-1);
    if (
      last &&
      last.index >= items.length - 6 &&
      browse.hasNextPage &&
      !browse.isFetchingNextPage
    ) {
      browse.fetchNextPage();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [virt.range?.endIndex, items.length, browse.hasNextPage, browse.isFetchingNextPage]);

  // Keep the keyboard-selected article visible.
  useEffect(() => {
    if (selectedId == null) return;
    const i = items.findIndex((a) => a.id === selectedId);
    if (i >= 0) virt.scrollToIndex(i, { align: "auto" });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selectedId]);

  // Jump back to the top of the list whenever the sidebar selection changes.
  // The scroll container stays mounted across the query swap, so without this
  // a new feed/folder/tag opens scrolled to wherever the *previous* list was
  // left — burying its newest articles below the fold. `scrollToOffset(0)`
  // also resets the virtualizer's internal offset, keeping its rendered window
  // in sync with the DOM scroll position.
  useEffect(() => {
    virt.scrollToOffset(0);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [query]);

  const markAll = async () => {
    try {
      const n = await api.markAllRead(query);
      actions.refreshAfterBulk();
      onToast(
        n > 0
          ? t("articleList.markedReadToast", { count: n })
          : t("articleList.nothingToMark"),
      );
    } catch (e) {
      reportError(e);
    }
  };

  const articleMenu = (a: ArticleSummary): MenuEntry[] => [
    { icon: "open", label: t("articleList.menuOpen"), shortcut: "⏎", onClick: () => openArticle(a.id) },
    ...(a.url
      ? ([
          {
            icon: "globe",
            label: t("articleList.menuOpenInBrowser"),
            shortcut: modCombo("O"),
            onClick: () => openInternalBrowser(a.url!, a.title).catch(reportError),
          },
        ] as MenuEntry[])
      : []),
    { separator: true },
    {
      icon: a.isRead ? "circle" : "check",
      label: a.isRead ? t("articleList.menuMarkUnread") : t("articleList.menuMarkRead"),
      shortcut: "U",
      onClick: () => actions.setRead(a.id, !a.isRead),
    },
    ...(a.url
      ? ([
          { separator: true },
          {
            icon: "copy",
            label: t("articleList.menuCopyLink"),
            onClick: () =>
              navigator.clipboard
                .writeText(a.url!)
                .then(() => onToast(t("articleList.linkCopied")), () => {}),
          },
        ] as MenuEntry[])
      : []),
  ];

  const vItems = virt.getVirtualItems();
  const showCount = t("articleList.countArticles", {
    count: items.length,
    suffix: browse.hasNextPage ? "+" : "",
  });

  // Arrow-key navigation for the listbox (in addition to the global j/k).
  const onListKeyDown = (e: React.KeyboardEvent) => {
    if (!["ArrowDown", "ArrowUp", "Home", "End"].includes(e.key)) return;
    if (items.length === 0) return;
    e.preventDefault();
    const cur = items.findIndex((x) => x.id === selectedId);
    const next =
      e.key === "Home"
        ? 0
        : e.key === "End"
          ? items.length - 1
          : e.key === "ArrowDown"
            ? Math.min(items.length - 1, cur < 0 ? 0 : cur + 1)
            : Math.max(0, cur < 0 ? 0 : cur - 1);
    openArticle(items[next].id);
  };

  return (
    <div className="list" role="region" aria-labelledby="article-list-title">
      <div className="list-header" {...(isMac && { "data-tauri-drag-region": true })}>
        <div className="list-title-row">
          <h1 className="list-title" id="article-list-title">
            {/* Smart views re-translate live; feed/folder/tag keep their own title. */}
            {query.kind === "feed" ||
            query.kind === "folder" ||
            query.kind === "tag"
              ? queryLabel
              : t(`smart.${query.kind}`)}
            <span className="count">{browse.isLoading ? t("common.loading") : showCount}</span>
          </h1>
          <div className="list-header-actions">
            <button
              className={`list-icon-btn ${refreshing ? "spinning" : ""}`}
              onClick={onRefresh}
              title={t("settings.sync.syncNow")}
              aria-label={t("settings.sync.syncNow")}
              disabled={refreshing}
            >
              <Icon name="refresh" size={15} />
            </button>
            <button
              className="list-icon-btn"
              onClick={onOpenSettings}
              title={t("settings.title")}
              aria-label={t("settings.title")}
            >
              <Icon name="settings" size={15} />
            </button>
          </div>
        </div>
        <div className="list-meta">
          <span className="list-fixed-filter">
            <Icon name="arrow-up" size={12} />
            {t("articleList.oldestFirst")}
          </span>
          <div style={{ flex: 1 }} />
          <button
            className="list-meta-btn"
            onClick={markAll}
            title={t("articleList.markAllRead")}
          >
            <Icon name="check-all" size={12} />
            {t("articleList.markRead")}
          </button>
        </div>
      </div>

      <div
        className="list-scroll"
        ref={scrollRef}
      >
        {browse.isLoading && (
          <div>
            {Array.from({ length: 7 }).map((_, i) => (
              <div className="sk-art" key={i}>
                <div className="sk-line" style={{ width: "40%" }} />
                <div className="sk-line" style={{ width: "92%", height: 12 }} />
                <div className="sk-line" style={{ width: "70%" }} />
              </div>
            ))}
          </div>
        )}

        {/* A failed fetch must not masquerade as "all caught up". */}
        {!browse.isLoading && browse.isError && items.length === 0 && (
          <div className="empty" style={{ height: 240 }}>
            <div className="glyph">
              <Icon name="alert" size={22} />
            </div>
            <div>{t("articleList.loadError")}</div>
            <button
              className="empty-retry"
              onClick={() => browse.refetch()}
              disabled={browse.isFetching}
            >
              <Icon name="refresh" size={12} />
              {t("common.retry")}
            </button>
          </div>
        )}

        {!browse.isLoading && !browse.isError && items.length === 0 && (
          <div className="empty" style={{ height: 240 }}>
            <div className="glyph">
              <Icon name="check" size={22} />
            </div>
            <div>{t("articleList.emptyState")}</div>
          </div>
        )}

        {!browse.isLoading && items.length > 0 && (
          <div
            role="listbox"
            tabIndex={0}
            aria-labelledby="article-list-title"
            aria-activedescendant={
              selectedId != null ? `option-article-${selectedId}` : undefined
            }
            onKeyDown={onListKeyDown}
            style={{
              height: virt.getTotalSize(),
              position: "relative",
              width: "100%",
            }}
          >
            {vItems.map((vi) => {
              const a = items[vi.index];
              const feed = feedById[a.feedId];
              const hasThumb = showCardThumbs && !!a.imageUrl;
              return (
                // Key by the virtual slot, not the article id. The window of
                // rendered rows is a fixed band that slides as you scroll, so
                // keying by index lets React keep the same ~dozen DOM nodes
                // mounted and just swap their content + transform. Keying by
                // `a.id` instead remounts a node every time the window slides,
                // restarting `measureElement` from its estimate each time — a
                // freshly mounted row briefly reports the 98px estimate before
                // the real ~130px is measured, so the row below it renders too
                // high and overlaps. (It also collides when offset pagination
                // returns the same article on two pages.)
                <div
                  key={vi.key}
                  data-index={vi.index}
                  ref={virt.measureElement}
                  style={{
                    position: "absolute",
                    top: 0,
                    left: 0,
                    width: "100%",
                    transform: `translateY(${vi.start}px)`,
                  }}
                >
                  <div
                    className={`art ${viewMode === "card" ? "card" : ""} ${
                      selectedId === a.id ? "active" : ""
                    } ${a.isRead ? "read" : ""} ${hasThumb ? "has-thumb" : "no-thumb"}`}
                    role="option"
                    id={`option-article-${a.id}`}
                    aria-selected={selectedId === a.id}
                    onClick={() => openArticle(a.id)}
                    onContextMenu={(e) => {
                      e.preventDefault();
                      setMenu({ x: e.clientX, y: e.clientY, article: a });
                    }}
                  >
                    {hasThumb && (
                      <CardThumb article={a} />
                    )}
                    <div className="art-main">
                      <div className="art-head">
                        {!a.isRead && <span className="art-dot" />}
                        <span className="art-feed">{a.feedTitle}</span>
                        {feed && feed.sourceType !== "rss" && (
                          <span className="src-badge">{feed.sourceType}</span>
                        )}
                      </div>
                      <div className="art-title-row">
                        <h3 className="art-title">{a.title}</h3>
                        <span className="art-time">{relTime(a.publishedAt)}</span>
                      </div>
                      {a.snippet && <p className="art-snippet">{a.snippet}</p>}
                    </div>
                  </div>
                </div>
              );
            })}
          </div>
        )}
        <div style={{ height: 60 }} />
      </div>

      {menu && (
        <ContextMenu
          x={menu.x}
          y={menu.y}
          items={articleMenu(menu.article)}
          onClose={() => setMenu(null)}
        />
      )}

    </div>
  );
}

/** Card-view thumbnail: the article image, or nothing. When a card has no
 *  usable image — none supplied and none extractable from the body, or the
 *  image fails to load — the card simply renders without a thumbnail rather
 *  than showing a generic placeholder. */
function CardThumb({ article }: { article: ArticleSummary }) {
  const [broken, setBroken] = useState(false);
  // The virtualizer recycles this instance across rows — clear the error
  // flag whenever the image URL changes.
  useEffect(() => setBroken(false), [article.imageUrl]);

  if (!article.imageUrl || broken) return null;

  return (
    <div className="art-thumb">
      <img
        src={article.imageUrl}
        alt=""
        loading="lazy"
        onError={() => setBroken(true)}
        style={{
          position: "absolute",
          inset: 0,
          width: "100%",
          height: "100%",
          objectFit: "cover",
        }}
      />
    </div>
  );
}
