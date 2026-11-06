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
          const frameKind = payload.kind || kind;
          const text = payload.text != null
            ? String(payload.text)
            : frameKind === 'image' && payload.media_type && payload.data
              ? JSON.stringify({ media_type: payload.media_type, data: payload.data })
              : '';
          frames.push({ kind: frameKind, text });
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

export function isRelayId(value) {
  return typeof value === 'string' && RELAY_ID.test(value);
}

/// Normalizes one pending approval read from `GET /api/approvals` into the
/// same shape `normalizeApproval` produces, so both sources share a ledger.
export function approvalFromPending(row, now = Date.now()) {
  if (!row || !isRelayId(row.id)) return null;
  return {
    id: row.id,
    tool: String(row.tool || 'Tool request'),
    detail: String(row.detail || ''),
    session: String(row.session || ''),
    createdAt: now,
    sending: false,
    error: '',
  };
}

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

export function autocompactLabel(compact) {
  if (!compact || typeof compact.enabled !== 'boolean') return '—';
  if (!compact.enabled) return 'off';
  const percent = Number(compact.percent);
  if (!Number.isFinite(percent)) return '—';
  const threshold = Number(compact.threshold_tokens);
  return Number.isFinite(threshold) ? `${percent}% · ${formatNumber(threshold)}` : `${percent}%`;
}

export function formatDuration(seconds) {
  if (!Number.isFinite(seconds)) return '—';
  const minutes = Math.floor(seconds / 60);
  return minutes ? `${minutes}m ${Math.floor(seconds % 60)}s` : `${Math.floor(seconds)}s`;
}

/// Compact process uptime: `42s`, `7m 04s`, `2h 07m`, `3d 04h`.
export function formatUptime(seconds) {
  if (!Number.isFinite(seconds) || seconds < 0) return '—';
  const whole = Math.floor(seconds);
  if (whole < 60) return `${whole}s`;
  if (whole < 3600) return `${Math.floor(whole / 60)}m ${String(whole % 60).padStart(2, '0')}s`;
  if (whole < 86400) return `${Math.floor(whole / 3600)}h ${String(Math.floor((whole % 3600) / 60)).padStart(2, '0')}m`;
  return `${Math.floor(whole / 86400)}d ${String(Math.floor((whole % 86400) / 3600)).padStart(2, '0')}h`;
}

/// Percentage of `total`, clamped to 0–100; null when the denominator is not
/// a positive finite number.
export function percentOf(value, total) {
  const part = Number(value);
  const whole = Number(total);
  if (!Number.isFinite(part) || !Number.isFinite(whole) || whole <= 0 || part < 0) return null;
  return Math.min(100, Math.round((part / whole) * 100));
}

/// Maps `GET /api/channels` into display rows with a status tone. Only string
/// values are shown; the free-text `note` is returned separately.
export function channelRows(result) {
  const rows = [];
  if (!result || typeof result !== 'object' || Array.isArray(result)) return { rows, note: '' };
  for (const [key, raw] of Object.entries(result)) {
    if (key === 'note' || typeof raw !== 'string') continue;
    const value = raw.trim();
    const lower = value.toLowerCase();
    let tone = 'off';
    let label = 'Off';
    if (lower === 'live') { tone = 'good'; label = 'Live'; }
    else if (lower === 'planned') { tone = 'planned'; label = 'Planned'; }
    else if (lower.includes('pairing')) { tone = 'waiting'; label = 'Pairing'; }
    else if (!lower.startsWith('off')) { tone = 'waiting'; label = value.slice(0, 40) || 'Unknown'; }
    const detail = label.toLowerCase() === lower ? '' : value;
    rows.push({ name: key.charAt(0).toUpperCase() + key.slice(1), tone, label, detail });
  }
  return { rows, note: typeof result.note === 'string' ? result.note : '' };
}

/// Composer key policy: Enter sends, Shift+Enter inserts a newline, IME
/// composition is never interrupted. Ctrl/Cmd+Enter also sends. On touch
/// (coarse pointer) devices there is no Shift key in practice, so plain Enter
/// keeps its newline meaning and only Ctrl/Cmd+Enter sends.
export function composerKeyAction(event, { touch = false } = {}) {
  if (!event || event.key !== 'Enter' || event.isComposing || event.keyCode === 229) return 'none';
  if (event.ctrlKey || event.metaKey) return 'send';
  if (touch) return 'none';
  if (event.shiftKey || event.altKey) return 'none';
  return 'send';
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
export const THEME_TOKENS = Object.freeze([
  '--bg', '--rail', '--panel', '--raised', '--line', '--line-soft',
  '--ink', '--muted', '--quiet', '--accent', '--accent-light',
  '--accent-surface', '--green', '--amber', '--red', '--cyan', '--radius',
]);

export const THEME_ALIASES = Object.freeze({
  background: '--bg',
  foreground: '--ink',
  card: '--panel',
  'card-foreground': '--ink',
  popover: '--panel',
  'popover-foreground': '--ink',
  primary: '--accent',
  'primary-foreground': '--ink',
  secondary: '--raised',
  'secondary-foreground': '--ink',
  muted: '--line-soft',
  'muted-foreground': '--muted',
  accent: '--accent-surface',
  'accent-foreground': '--ink',
  destructive: '--red',
  'destructive-foreground': '--ink',
  border: '--line',
  input: '--line',
  ring: '--accent',
  sidebar: '--rail',
  'sidebar-foreground': '--ink',
  'sidebar-primary': '--accent',
  'sidebar-primary-foreground': '--ink',
  'sidebar-accent': '--accent-surface',
  'sidebar-accent-foreground': '--ink',
  'sidebar-border': '--line',
  'sidebar-ring': '--accent',
  radius: '--radius',
});

const THEME_TOKEN_SET = new Set(THEME_TOKENS);
const HEX_COLOR = /^#[0-9a-f]{3,4}$|^#[0-9a-f]{6}(?:[0-9a-f]{2})?$/i;
const BARE_HSL = /^[-+]?(?:\d*\.)?\d+(?:\s+[-+]?(?:\d*\.)?\d+%?){2}(?:\s*\/\s*[-+]?(?:\d*\.)?\d+%?)?$/;
const COLOR_FUNCTION = /^(?:rgb|rgba|hsl|hsla|oklch|oklab|lab|lch|color)\(\s*[a-z0-9.%+\-/,\s]+\)$/i;
const RADIUS_VALUE = /^(?:0|(?:0|[1-9]\d?)px|(?:0|[01](?:\.\d+)?)rem)$/;
const MEDIA_UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
const RASTER_TYPES = new Set(['image/png', 'image/jpeg', 'image/webp', 'image/gif']);

export function rasterMediaType(value) {
  const type = String(value || '').split(';', 1)[0].trim().toLowerCase();
  return RASTER_TYPES.has(type) ? type : '';
}

export function rasterDataUrl(mediaType, data) {
  const type = rasterMediaType(mediaType);
  const encoded = String(data || '').trim();
  if (!type || !encoded || !/^[a-z0-9+/=\s]+$/i.test(encoded)) return '';
  return `data:${type};base64,${encoded}`;
}

export function mediaPath(id) {
  const value = String(id || '').trim();
  return MEDIA_UUID.test(value) ? `/api/media/${value}` : '';
}

function themeTarget(name) {
  const raw = String(name || '').trim().toLowerCase();
  if (raw.startsWith('--')) {
    if (THEME_TOKEN_SET.has(raw)) return raw;
    return THEME_ALIASES[raw.slice(2)] || '';
  }
  return THEME_ALIASES[raw] || (THEME_TOKEN_SET.has(`--${raw}`) && raw !== 'accent' ? `--${raw}` : '');
}

function normalizeThemeValue(token, value) {
  const text = String(value ?? '').trim();
  if (!text || text.length > 160 || /[{};`\n\r<>"']/.test(text) || /\b(?:url|var|env|image|attr)\s*\(/i.test(text)) return '';
  if (token === '--radius') return RADIUS_VALUE.test(text) ? text : '';
  if (HEX_COLOR.test(text) || COLOR_FUNCTION.test(text)) return text;
  return BARE_HSL.test(text) ? `hsl(${text})` : '';
}

export function validateThemeTokens(tokens) {
  if (!tokens || typeof tokens !== 'object' || Array.isArray(tokens)) {
    throw new Error('Theme tokens must be an object.');
  }
  const normalized = {};
  for (const [rawName, rawValue] of Object.entries(tokens)) {
    const name = themeTarget(rawName);
    if (!name) continue;
    const value = normalizeThemeValue(name, rawValue);
    if (!value) throw new Error(`Invalid value for theme token ${rawName}.`);
    normalized[name] = value;
  }
  if (!Object.keys(normalized).length) throw new Error('Add at least one allowed theme token.');
  return normalized;
}

function parseDeclarations(source) {
  const tokens = {};
  for (const declaration of String(source || '').split(';')) {
    const part = declaration.trim();
    if (!part) continue;
    const match = part.match(/^([a-zA-Z][a-zA-Z0-9_-]*|--[a-zA-Z0-9_-]+)\s*:\s*(.*?)\s*$/);
    if (!match) throw new Error('CSS input contains an invalid custom-property declaration.');
    tokens[match[1]] = match[2];
  }
  return tokens;
}

function parseCssThemeTokens(text) {
  const source = String(text || '').trim();
  if (!source || source.includes('/*') || source.includes('*/') || /[<>]/.test(source)) {
    throw new Error('CSS input must contain only allowed theme tokens.');
  }
  const tokens = {};
  const blocks = /(?:^|\s*)(:root|\.dark)\s*\{([^{}]*)\}/gi;
  let match;
  let cursor = 0;
  let found = false;
  while ((match = blocks.exec(source))) {
    if (source.slice(cursor, match.index).trim()) throw new Error('CSS input contains an unsupported selector.');
    Object.assign(tokens, parseDeclarations(match[2]));
    cursor = blocks.lastIndex;
    found = true;
  }
  if (found) {
    if (source.slice(cursor).trim()) throw new Error('CSS input contains content outside :root or .dark.');
    return tokens;
  }
  return parseDeclarations(source);
}

export function parseThemeInput(text) {
  const source = String(text || '').trim();
  if (!source) throw new Error('Enter theme JSON or CSS custom properties.');
  let tokens;
  if (source.startsWith('{')) {
    try { tokens = JSON.parse(source); }
    catch { throw new Error('Theme JSON is invalid.'); }
  } else {
    tokens = parseCssThemeTokens(source);
  }
  return validateThemeTokens(tokens);
}

export function safeImageSource(value) {
  const source = String(value || '').trim();
  if (/^data:image\/(?:png|jpe?g|webp|gif);base64,[a-z0-9+/=\s]+$/i.test(source)) return source;
  if (/^blob:[^\s<>"']+$/i.test(source)) return source;
  if (/^\/api\/media\/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(source)) return source;
  return '';
}

function imageSourceFromValue(value) {
  if (typeof value === 'string') return mediaPath(value) || safeImageSource(value);
  if (!value || typeof value !== 'object') return '';
  for (const key of ['id', 'media_id', 'mediaId', 'uuid']) {
    const source = mediaPath(value[key]);
    if (source) return source;
  }
  for (const key of ['url', 'src', 'image_url', 'imageUrl', 'data_url', 'dataUrl']) {
    if (typeof value[key] === 'string') {
      const source = safeImageSource(value[key]);
      if (source) return source;
    }
  }
  if (typeof value.data === 'string' && typeof value.media_type === 'string') {
    return rasterDataUrl(value.media_type, value.data);
  }
  return '';
}

export function messageImageSources(message) {
  if (!message || typeof message !== 'object' || !['user', 'assistant'].includes(message.role)) return [];
  const sources = [];
  const add = (value) => {
    if (Array.isArray(value)) value.forEach(add);
    else {
      const source = imageSourceFromValue(value);
      if (source && !sources.includes(source)) sources.push(source);
    }
  };
  for (const value of [
    message.images, message.image_urls, message.media, message.attachments,
    message.media_ids, message.metadata?.images, message.metadata?.image_urls,
    message.metadata?.media, message.metadata?.attachments, message.metadata?.media_ids,
  ]) add(value);
  return sources.slice(0, 8);
}

export function parseImageEvent(text) {
  try {
    const value = JSON.parse(String(text || ''));
    return rasterDataUrl(value?.media_type, value?.data);
  } catch {
    return '';
  }
}

export function validateSkillSource(source) {
  const value = String(source || '').trim();
  if (!value || value.length > 128 * 1024 || /[\u0000\u0001-\u0008\u000b\u000c\u000e-\u001f]/.test(value)) {
    throw new Error('Paste a valid SKILL.md or GitHub reference.');
  }
  const github = /^(?:https?:\/\/)?(?:www\.)?github\.com\/[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+(?:\/tree\/[^\s]+)?(?:#[^\s]+)?$/i;
  const skill = /(^---\s*$|^name\s*:\s*\S+|^description\s*:\s*\S+)/im.test(value) && value.length >= 40;
  if (!github.test(value) && !skill) throw new Error('Paste a valid SKILL.md or GitHub reference.');
  return value;
}
