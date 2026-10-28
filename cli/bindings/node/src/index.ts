import { appendFile, mkdir, readFile, writeFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";

export interface CommandResult {
  exitCode: number;
  stdout: string;
  stderr: string;
}

export interface VarynthClientOptions {
  binary?: string;
  cwd?: string;
  env?: NodeJS.ProcessEnv;
}

export class VarynthClient {
  readonly binary: string;
  readonly cwd: string;
  readonly env: NodeJS.ProcessEnv;

  constructor(options: VarynthClientOptions = {}) {
    this.binary = options.binary ?? defaultBinary();
    this.cwd = resolve(options.cwd ?? process.cwd());
    this.env = { ...process.env, ...options.env };
  }

  run(args: readonly string[]): Promise<CommandResult> {
    return new Promise((resolveResult, reject) => {
      const child = spawn(this.binary, [...args], {
        cwd: this.cwd,
        env: this.env,
        shell: false,
        windowsHide: true,
      });
      let stdout = "";
      let stderr = "";
      child.stdout.setEncoding("utf8");
      child.stderr.setEncoding("utf8");
      child.stdout.on("data", (chunk: string) => {
        stdout += chunk;
      });
      child.stderr.on("data", (chunk: string) => {
        stderr += chunk;
      });
      child.once("error", reject);
      child.once("close", (exitCode) => {
        resolveResult({ exitCode: exitCode ?? 1, stdout, stderr });
      });
    });
  }

  async exec(prompt: string): Promise<string> {
    const result = await this.run(["exec", prompt]);
    if (result.exitCode !== 0) {
      throw new Error(result.stderr.trim() || `varynth exited with ${result.exitCode}`);
    }
    return result.stdout.trim();
  }

  async addTask(
    name: string,
    prompt: string,
    trigger: { at?: string; everySeconds?: number },
  ): Promise<unknown> {
    const args = ["task", "add", name, prompt];
    if (trigger.at) args.push("--at", trigger.at);
    if (trigger.everySeconds !== undefined) {
      args.push("--every-seconds", String(trigger.everySeconds));
    }
    const result = await this.run(args);
    if (result.exitCode !== 0) {
      throw new Error(result.stderr.trim() || "failed to add task");
    }
    return JSON.parse(result.stdout);
  }
}

export class AgentWorkspace {
  readonly root: string;
  readonly globalDir: string;
  readonly projectDir: string;

  constructor(root: string = process.cwd(), globalDir = defaultVarynthHome()) {
    this.root = resolve(root);
    this.globalDir = resolve(globalDir);
    this.projectDir = join(this.root, ".varynth");
  }

  async ensureFiles(): Promise<void> {
    await ensureLayer(this.globalDir);
    await ensureLayer(this.projectDir);
  }

  async context(maxBytes = 128 * 1024): Promise<string> {
    const blocks: string[] = [];
    for (const directory of [this.globalDir, this.projectDir]) {
      for (const name of FILES) {
        const path = await existingPath(directory, name);
        if (!path) continue;
        const content = (await readFile(path, "utf8")).replaceAll(
          "</varynth-agent-file>",
          "<\\/varynth-agent-file>",
        );
        blocks.push(`<varynth-agent-file name="${name}">${content}</varynth-agent-file>`);
      }
    }
    return truncateUtf8(blocks.join("\n\n"), maxBytes);
  }

  async appendMemory(entry: string): Promise<void> {
    await mkdir(this.projectDir, { recursive: true });
    const path = join(this.projectDir, "MEMORY.md");
    const addition = `\n\n${entry.trim()}\n`;
    const current = existsSync(path) ? await readFile(path, "utf8") : "";
    if (Buffer.byteLength(current, "utf8") + Buffer.byteLength(addition, "utf8") > MAX_AGENT_FILE_BYTES) {
      throw new Error(`MEMORY.md exceeds the ${MAX_AGENT_FILE_BYTES} byte limit`);
    }
    await appendFile(path, addition, "utf8");
  }
}

const FILES = ["SOUL.md", "USER.md", "MEMORY.md", "DREAM.md", "HEART.md"] as const;
const MAX_AGENT_FILE_BYTES = 64 * 1024;

const DEFAULTS: Record<(typeof FILES)[number], string> = {
  "SOUL.md": "# Soul\n\nDefine the agent identity here.\n",
  "USER.md": "# User\n\nRecord stable user preferences here.\n",
  "MEMORY.md": "# Memory\n\nDurable facts and decisions go here.\n",
  "DREAM.md": "# Dreams\n\nLonger-term ideas go here.\n",
  "HEART.md": "# Heart\n\nCurrent priorities go here.\n",
};

function defaultVarynthHome(): string {
  return process.env.VARYNTH_HOME ?? join(homedir(), ".varynth");
}

function defaultBinary(): string {
  if (process.env.VARYNTH_BINARY) return process.env.VARYNTH_BINARY;
  const root = resolve(dirname(fileURLToPath(import.meta.url)), "../../..");
  const release = join(root, "target", "release", process.platform === "win32" ? "varynth.exe" : "varynth");
  if (existsSync(release)) return release;
  return join(root, "target", "debug", process.platform === "win32" ? "varynth.exe" : "varynth");
}

async function ensureLayer(directory: string): Promise<void> {
  await mkdir(directory, { recursive: true });
  for (const name of FILES) {
    const path = join(directory, name);
    if (!existsSync(path) && !existsSync(join(directory, name.toLowerCase()))) {
      await writeFile(path, DEFAULTS[name], { flag: "wx" });
    }
  }
}

async function existingPath(directory: string, name: string): Promise<string | undefined> {
  const canonical = join(directory, name);
  if (existsSync(canonical)) return canonical;
  const lowercase = join(directory, name.toLowerCase());
  return existsSync(lowercase) ? lowercase : undefined;
}

function truncateUtf8(value: string, maxBytes: number): string {
  const bytes = Buffer.from(value, "utf8");
  if (bytes.byteLength <= maxBytes) return value;
  return `${bytes.subarray(0, Math.max(0, maxBytes - 24)).toString("utf8")}\n[truncated]\n`;
}
