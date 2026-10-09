import { textContent, type Message } from '@/api/tauri';
import { messageKind, reasoningOf, userSourceOf } from '@/api/message';
import type { SearchScope } from '@/store/useSearchStore';

export interface SearchMatch {
  messageId: string;
  groupIndex: number;
  start: number;
  end: number;
}

export interface TextMatch {
  start: number;
  end: number;
}

/** 纯文本匹配，返回所有 (start, end) 位置 */
export function findTextOccurrences(text: string, query: string, caseSensitive: boolean = false): TextMatch[] {
  if (!query) return [];
  const hay = caseSensitive ? text : text.toLowerCase();
  const needle = caseSensitive ? query : query.toLowerCase();
  const matches: TextMatch[] = [];
  let offset = 0;
  while (offset < hay.length) {
    const idx = hay.indexOf(needle, offset);
    if (idx < 0) break;
    matches.push({ start: idx, end: idx + needle.length });
    offset = idx + 1;
  }
  return matches;
}

/** 去掉 agent 回复消息的注释头，只保留用户可见的正文 */
function stripAgentReplyHeader(content: string): string {
  const m = content.match(/^<!-- tiangong-agent-reply -->\n<!-- label:[^\n]* -->\n\n?([\s\S]*)$/);
  return m ? m[1].trim() : content;
}

export interface MessageGroupLike {
  messages: Array<{ id: string }>;
}

/** 获取消息的可搜索正文（assistant 消息会去掉 agent reply header） */
function getSearchableContent(msg: Message): string {
  const raw = textContent(msg);
  if (messageKind(msg) === 'assistant') return stripAgentReplyHeader(raw);
  return raw;
}

/** 根据搜索范围遍历消息，收集匹配 */
export function findSearchMatches(
  messages: Message[],
  query: string,
  groups: MessageGroupLike[],
  scope: SearchScope = 'messages',
  caseSensitive: boolean = false,
): SearchMatch[] {
  if (!query) return [];

  const msgGroupMap = new Map<string, number>();
  for (let gi = 0; gi < groups.length; gi++) {
    for (const msg of groups[gi].messages) {
      msgGroupMap.set(msg.id, gi);
    }
  }

  const matches: SearchMatch[] = [];
  for (const msg of messages) {
    const source = userSourceOf(msg);
    if (source === 'compressed_resume' || source === 'host_injected') continue;
    const kind = messageKind(msg);
    const reasoning = reasoningOf(msg);
    const groupIndex = msgGroupMap.get(msg.id) ?? -1;

    if (scope === 'messages') {
      if (kind !== 'user' && kind !== 'assistant') continue;
      const content = getSearchableContent(msg);
      if (!content) continue;
      for (const occ of findTextOccurrences(content, query, caseSensitive)) {
        matches.push({ messageId: msg.id, groupIndex, start: occ.start, end: occ.end });
      }
    } else if (scope === 'withThinking') {
      if (kind !== 'user' && kind !== 'assistant') continue;
      const content = getSearchableContent(msg);
      if (content) {
        for (const occ of findTextOccurrences(content, query, caseSensitive)) {
          matches.push({ messageId: msg.id, groupIndex, start: occ.start, end: occ.end });
        }
      }
      if (reasoning) {
        for (const occ of findTextOccurrences(reasoning, query, caseSensitive)) {
          matches.push({ messageId: msg.id, groupIndex, start: occ.start, end: occ.end });
        }
      }
    } else {
      const content = getSearchableContent(msg);
      if (content) {
        for (const occ of findTextOccurrences(content, query, caseSensitive)) {
          matches.push({ messageId: msg.id, groupIndex, start: occ.start, end: occ.end });
        }
      }
      if (reasoning) {
        for (const occ of findTextOccurrences(reasoning, query, caseSensitive)) {
          matches.push({ messageId: msg.id, groupIndex, start: occ.start, end: occ.end });
        }
      }
    }
  }

  return matches;
}
