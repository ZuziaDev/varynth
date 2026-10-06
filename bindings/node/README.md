# @varynth/node

ESM TypeScript bindings for the Varynth Rust agent.

```ts
import { AgentWorkspace, VarynthClient } from "@varynth/node";

const workspace = new AgentWorkspace(process.cwd());
await workspace.ensureFiles();
const client = new VarynthClient();
const reply = await client.exec("summarize the current workspace");
console.log(reply);
```

The Rust binary still enforces provider credentials and tool permissions. Set `VARYNTH_BINARY` when the binary is outside the repository.
