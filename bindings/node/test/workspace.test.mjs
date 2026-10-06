import assert from "node:assert/strict";
import { mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { AgentWorkspace } from "../dist/index.js";

test("AgentWorkspace creates layered files and appends memory", async () => {
  const root = await mkdtemp(join(tmpdir(), "varynth-node-test-"));
  const workspace = new AgentWorkspace(root, join(root, "global"));
  await workspace.ensureFiles();
  await workspace.appendMemory("binding fact");
  assert.match(await workspace.context(), /binding fact/);
});
