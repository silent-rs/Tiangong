import { useEffect, useRef, useMemo, useState, KeyboardEvent } from 'react';
import { ChevronUp, ChevronDown, X, ChevronDown as DropdownIcon, Loader2 } from 'lucide-react';
import { useSearchStore, type SearchScope } from '@/store/useSearchStore';
import { useStore } from '@/store/useStore';
import { findSearchMatches } from '@/utils/search';

const SCOPE_LABELS: Record<SearchScope, string> = {
  messages: '消息',
  withThinking: '含思考',
  all: '全部',
};

export function SearchBar() {
  const searchQuery = useSearchStore((s) => s.searchQuery);
  const currentMatchIndex = useSearchStore((s) => s.currentMatchIndex);
  const searchScope = useSearchStore((s) => s.searchScope);
  const caseSensitive = useSearchStore((s) => s.caseSensitive);
  const setSearchQuery = useSearchStore((s) => s.setSearchQuery);
  const setSearchScope = useSearchStore((s) => s.setSearchScope);
  const setCaseSensitive = useSearchStore((s) => s.setCaseSensitive);
  const closeSearch = useSearchStore((s) => s.closeSearch);
  const nextMatch = useSearchStore((s) => s.nextMatch);
  const prevMatch = useSearchStore((s) => s.prevMatch);
  const inputRef = useRef<HTMLInputElement>(null);
  const [scopeOpen, setScopeOpen] = useState(false);
  const scopeRef = useRef<HTMLDivElement>(null);

  const messages = useStore((s) => s.messages);
  // 搜索前会自动向前加载全部历史；加载完成前结果不完整，显示加载提示。
  const loadingHistory = useStore((s) => s.loadingOlderMessages);
  const setCurrentMatchInfo = useSearchStore((s) => s.setCurrentMatchInfo);

  const searchResults = useMemo(() => {
    if (!searchQuery) return [] as ReturnType<typeof findSearchMatches>;
    return findSearchMatches(messages, searchQuery, [], searchScope, caseSensitive);
  }, [messages, searchQuery, searchScope, caseSensitive]);
  const matchCount = searchResults.length;

  // 同步当前匹配信息到 store
  useEffect(() => {
    const match = matchCount > 0 ? searchResults[currentMatchIndex] : null;
    setCurrentMatchInfo(match?.messageId ?? null, match?.start ?? null);
  }, [matchCount, searchResults, currentMatchIndex, setCurrentMatchInfo]);

  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  useEffect(() => {
    if (!scopeOpen) return;
    const handler = (e: MouseEvent) => {
      if (scopeRef.current && !scopeRef.current.contains(e.target as Node)) {
        setScopeOpen(false);
      }
    };
    document.addEventListener('mousedown', handler);
    return () => document.removeEventListener('mousedown', handler);
  }, [scopeOpen]);

  const handleKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === 'Escape') {
      e.preventDefault();
      closeSearch();
    } else if (e.key === 'Enter') {
      e.preventDefault();
      if (e.shiftKey) {
        prevMatch(matchCount);
      } else {
        nextMatch(matchCount);
      }
    }
  };

  const position =
    matchCount > 0 ? `${currentMatchIndex + 1}/${matchCount}` : '无匹配';

  return (
    <div className="sticky top-0 z-20 flex items-center gap-2 rounded-md border bg-background/95 px-3 py-1.5 backdrop-blur shadow-sm">
      <input
        ref={inputRef}
        data-search-input
        type="text"
        value={searchQuery}
        onChange={(e) => setSearchQuery(e.target.value)}
        onKeyDown={handleKeyDown}
        placeholder="搜索消息..."
        className="flex-1 bg-transparent text-sm outline-none placeholder:text-muted-foreground"
      />
      <button
        onClick={() => setCaseSensitive(!caseSensitive)}
        className={`flex items-center justify-center w-6 h-6 rounded text-xs font-medium border transition-colors ${
          caseSensitive
            ? 'bg-primary text-primary-foreground border-primary'
            : 'bg-transparent text-muted-foreground border-transparent hover:text-foreground hover:bg-muted/50'
        }`}
        title={caseSensitive ? '区分大小写（已开启）' : '区分大小写'}
      >
        Aa
      </button>
      <div ref={scopeRef} className="relative">
        <button
          onClick={() => setScopeOpen(!scopeOpen)}
          className="flex items-center gap-0.5 px-1.5 py-0.5 rounded text-xs text-muted-foreground hover:text-foreground hover:bg-muted/50 transition-colors"
          title="搜索范围"
        >
          {SCOPE_LABELS[searchScope]}
          <DropdownIcon className="w-3 h-3" />
        </button>
        {scopeOpen && (
          <div className="absolute right-0 top-full mt-1 py-1 rounded-md border bg-popover shadow-md z-30 min-w-[5rem]">
            {(['messages', 'withThinking', 'all'] as SearchScope[]).map((s) => (
              <button
                key={s}
                onClick={() => { setSearchScope(s); setScopeOpen(false); }}
                className={`block w-full text-left px-3 py-1 text-xs transition-colors ${
                  s === searchScope ? 'text-foreground bg-muted' : 'text-muted-foreground hover:text-foreground hover:bg-muted/50'
                }`}
              >
                {SCOPE_LABELS[s]}
              </button>
            ))}
          </div>
        )}
      </div>
      <span className="shrink-0 inline-flex items-center justify-end gap-1 text-xs text-muted-foreground min-w-[3rem] text-right">
        {loadingHistory && <Loader2 className="h-3 w-3 animate-spin" />}
        {loadingHistory ? '加载全部历史…' : position}
      </span>
      <button
        onClick={() => prevMatch(matchCount)}
        disabled={matchCount === 0}
        className="text-muted-foreground hover:text-foreground disabled:opacity-30 transition-colors"
        title="上一个匹配 (Shift+Enter)"
      >
        <ChevronUp className="w-4 h-4" />
      </button>
      <button
        onClick={() => nextMatch(matchCount)}
        disabled={matchCount === 0}
        className="text-muted-foreground hover:text-foreground disabled:opacity-30 transition-colors"
        title="下一个匹配 (Enter)"
      >
        <ChevronDown className="w-4 h-4" />
      </button>
      <button
        onClick={closeSearch}
        className="text-muted-foreground hover:text-foreground transition-colors"
        title="关闭搜索 (Esc)"
      >
        <X className="w-4 h-4" />
      </button>
    </div>
  );
}
