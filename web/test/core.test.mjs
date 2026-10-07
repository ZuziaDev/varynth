import test from 'node:test';
import assert from 'node:assert/strict';
import { ApprovalLedger, normalizeApproval, parseContext, selectedAfterRefresh, SseDecoder } from '../assets/core.mjs';
import { sideBySideRows } from '../assets/diff.mjs';

test('SSE decoder handles split frames and preserves deltas', () => {
  const decoder = new SseDecoder();
  assert.deepEqual(decoder.feed('event: delta\ndata: {"kind":"delta","text":"hel'), []);
  assert.deepEqual(decoder.feed('lo"}\n\n'), [{ kind: 'delta', text: 'hello' }]);
});

test('approval ledger deduplicates and resolves one-shot requests', () => {
  const ledger = new ApprovalLedger();
  const request = normalizeApproval({ kind: 'approval_pending', text: JSON.stringify({ id: '12345678-1234-1234-1234-123456789012', tool: 'bash', detail: 'git status' }) }, 'session');
  assert.equal(ledger.add(request), true);
  assert.equal(ledger.add(request), false);
  ledger.resolve(request.id);
  assert.equal(ledger.pending.size, 0);
  assert.equal(ledger.add(request), false);
});

test('context parser rejects tool content and caps messages', () => {
  assert.deepEqual(parseContext('[{"role":"user","content":"hello"}]'), [{ role: 'user', content: 'hello' }]);
  assert.throws(() => parseContext('[{"role":"tool","content":"no"}]'));
});

test('session selection is stable across refresh', () => {
  assert.equal(selectedAfterRefresh('history-id', 'active-id'), 'history-id');
  assert.equal(selectedAfterRefresh(null, 'active-id'), 'active-id');
});

test('diff rows align additions and removals', () => {
  const rows = sideBySideRows('@@ -1,2 +1,2 @@\n-old\n+new\n same');
  assert.equal(rows[1].old.text, 'old');
  assert.equal(rows[1].new.text, 'new');
  assert.equal(rows[2].kind, 'context');
});
