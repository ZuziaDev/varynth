export class SseDecoder {
  constructor() {
    this.buffer = '';
  }

  feed(text) {
    this.buffer += text;
    const frames = [];
    for (;;) {
      const separator = /\r?\n\r?\n/.exec(this.buffer);
      if (!separator) break;
      const block = this.buffer.slice(0, separator.index);
      this.buffer = this.buffer.slice(separator.index + separator[0].length);
      let kind = 'message';
      const data = [];
      for (const line of block.split(/\r?\n/)) {
        if (line.startsWith('event:')) kind = line.slice(6).trim();
        if (line.startsWith('data:')) data.push(line.slice(5).replace(/^ /, ''));
      }
      if (!data.length) continue;
      const raw = data.join('\n');
      try {
        const payload = JSON.parse(raw);
        if (typeof payload === 'object' && payload !== null) {
          frames.push({ kind: payload.kind || kind, text: String(payload.text ?? '') });
        } else {
          frames.push({ kind, text: String(payload) });
        }
      } catch {
        throw new Error('The server sent an invalid stream frame.');
      }
    }
    return frames;
  }

  finish() {
    if (this.buffer.trim() && !this.buffer.trim().startsWith(':')) {
      throw new Error('The response stream ended inside a frame.');
    }
  }
}

const RELAY_ID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

export function normalizeApproval(event, session, now = Date.now()) {
  if (!event || !['approval_pending', 'approval_request'].includes(event.kind)) return null;
  let data = event;
  if (event.kind === 'approval_pending') {
    try { data = JSON.parse(event.text); } catch { return null; }
  }
  const embedded = String(event.text || '').match(/\[id:([0-9a-f-]+)\]/i);
  const id = data.id || embedded?.[1];
  if (!id || !RELAY_ID.test(id)) return null;
  return {
    id,
    tool: String(data.tool || 'Tool request'),
    detail: String(data.detail ?? event.text ?? '').replace(/\s*\[id:[0-9a-f-]+\]\s*$/i, ''),
    session: String(session || ''),
    createdAt: now,
    sending: false,
    error: '',
  };
}

export class ApprovalLedger {
  constructor() {
    this.pending = new Map();
    this.resolved = new Set();
  }

  add(request) {
    if (!request || this.resolved.has(request.id)) return false;
    const current = this.pending.get(request.id);
    if (current) {
      current.tool = request.tool;
      current.detail = request.detail;
      if (request.session) current.session = request.session;
      return false;
    }
    this.pending.set(request.id, request);
    return true;
  }

  resolve(id) {
    this.pending.delete(id);
    this.resolved.add(id);
    if (this.resolved.size > 256) this.resolved.delete(this.resolved.values().next().value);
  }

  expire(now = Date.now()) {
    for (const request of this.pending.values()) {
      if (!request.sending && now - request.createdAt > 125_000) this.resolve(request.id);
    }
  }
}

export function selectedAfterRefresh(selected, serverSession) {
  return selected || serverSession || null;
}

export function matchesSessionQuery(session, query) {
  const value = query.toLocaleLowerCase().trim();
  return !value || `${session.title || ''} ${session.model || ''} ${session.id}`.toLocaleLowerCase().includes(value);
}

export function messageParts(text) {
  const parts = [];
  const code = /^```([^\n]*)\n([\s\S]*?)^```\s*$/gm;
  let offset = 0;
  for (const match of String(text).matchAll(code)) {
    if (match.index > offset) parts.push({ kind: 'text', text: text.slice(offset, match.index) });
    parts.push({ kind: 'code', language: match[1].trim(), text: match[2].replace(/\n$/, '') });
    offset = match.index + match[0].length;
  }
  if (offset < text.length) parts.push({ kind: 'text', text: text.slice(offset) });
  return parts.length ? parts : [{ kind: 'text', text: String(text) }];
}

export function eventMatchesFilter(kind, filter) {
  if (filter === 'all') return true;
  if (filter === 'tools') return ['tool', 'tool_result'].includes(kind);
  if (filter === 'errors') return kind === 'error';
  return ['system', 'round', 'connection', 'response'].includes(kind);
}

export function relativeTime(value, now = Date.now()) {
  const seconds = Math.max(0, Math.floor((now - Date.parse(value)) / 1000));
  if (!Number.isFinite(seconds)) return '';
  if (seconds < 60) return 'Now';
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m ago`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)}h ago`;
  return `${Math.floor(seconds / 86400)}d ago`;
}

export function formatNumber(value) {
  return Number.isFinite(value) ? new Intl.NumberFormat('en-US').format(value) : '—';
}

export function formatDuration(seconds) {
  if (!Number.isFinite(seconds)) return '—';
  const minutes = Math.floor(seconds / 60);
  return minutes ? `${minutes}m ${Math.floor(seconds % 60)}s` : `${Math.floor(seconds)}s`;
}

export function parseContext(text) {
  const messages = JSON.parse(text);
  if (!Array.isArray(messages) || messages.length > 200) {
    throw new Error('Context must be an array with at most 200 text messages.');
  }
  for (const message of messages) {
    if (!message || !['user', 'assistant', 'system'].includes(message.role) || typeof message.content !== 'string') {
      throw new Error('Each context message needs a user, assistant or system role and text content.');
    }
    if (message.tool_calls || message.tool_call_id || message.images?.length) {
      throw new Error('Tool calls and image attachments cannot be inserted into context.');
    }
  }
  const normalized = messages.map(({ role, content }) => ({ role, content }));
  if (new TextEncoder().encode(JSON.stringify(normalized)).length > 512 * 1024) {
    throw new Error('Context is limited to 512 KiB.');
  }
  return normalized;
}
