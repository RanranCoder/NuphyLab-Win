// Actions NuPhyLab can perform for a Claude Code session.
//
// Claude Code runs either inside the Claude desktop app or in a terminal the
// application cannot identify. Only actions that can be carried out reliably on
// this machine are offered; everything else is reported as unavailable with the
// reason, and approval decisions are never automated.
import { execFile, spawn } from "node:child_process";
import { access } from "node:fs/promises";
import { constants } from "node:fs";
import { promisify } from "node:util";
import path from "node:path";
import { homedir } from "node:os";

const run = promisify(execFile);
const IS_WIN = process.platform === "win32";
const SYSTEM_ROOT = process.env.SystemRoot ?? "C:\\Windows";
const CMD_EXE = path.join(SYSTEM_ROOT, "System32", "cmd.exe");
const CLIP_EXE = path.join(SYSTEM_ROOT, "System32", "clip.exe");
export const CLAUDE_APP_BUNDLE_ID = "com.anthropic.claudefordesktop";
export const CLAUDE_APP_PATHS = Object.freeze(
  IS_WIN
    ? [path.join(homedir(), "AppData", "Local", "AnthropicClaude", "claude.exe")]
    : ["/Applications/Claude.app"],
);

// The Claude desktop application handles claude://code/continue?session=<id>.
// The accepted form was read from the installed application's own URL router,
// which validates the parameter as "last" or /^local_[A-Za-z0-9-]{1,64}$/. That
// id is the application's own session id, not the Claude Code session id, so it
// has to come from the application's own record for the session.
export const CLAUDE_SESSION_URL = (desktopId) => `claude://code/continue?session=${desktopId}`;
const DESKTOP_ID = /^local_[A-Za-z0-9-]{1,64}$/;

export const CLAUDE_COMMANDS = Object.freeze(
  [
    { id: "openSession", label: "打开对应会话", type: "openSession" },
    { id: "nextAttention", label: "下一个需要关注的会话", type: "local" },
    { id: "nextTask", label: "下一个会话", type: "local" },
    { id: "prevTask", label: "上一个会话", type: "local" },
    { id: "focusApp", label: "切换到 Claude 应用", type: "focusApp" },
    { id: "revealProject", label: "打开项目文件夹", type: "revealProject" },
    { id: "copyResume", label: "复制恢复命令", type: "copyResume" },
    { id: "toggleLightMode", label: "切换灯光模式", type: "local" },
    { id: "wakeLights", label: "唤醒状态灯", type: "local" },
    { id: "refresh", label: "刷新会话状态", type: "local" },
    { id: "none", label: "不执行", type: "local" },
    {
      id: "approve", label: "批准当前权限请求", type: "unavailable",
      reason: "NuPhyLab 不会代替你批准权限请求，请在 Claude Code 中确认。",
    },
    {
      id: "decline", label: "拒绝当前权限请求", type: "unavailable",
      reason: "NuPhyLab 不会代替你拒绝权限请求，请在 Claude Code 中确认。",
    },
    {
      id: "sendInput", label: "向会话发送输入", type: "unavailable",
      reason: "Claude Code 没有面向第三方应用的本机输入接口；向无法确认的终端窗口发送按键不可靠。",
    },
    {
      id: "newSession", label: "新建会话", type: "unavailable",
      reason: "无法确定应该在哪个终端或窗口新建会话；请在 Claude Code 中新建。",
    },
  ].map((entry) => Object.freeze({ available: entry.type !== "unavailable", ...entry })),
);
export const DEFAULT_CLAUDE_COMMAND_IDS = Object.freeze([
  "nextAttention", "focusApp", "revealProject", "copyResume", "toggleLightMode", "refresh",
]);
export const CLAUDE_TASK_ACTIONS = Object.freeze(["openSession", "focusApp", "revealProject", "copyResume", "selectOnly"]);
export const LOCAL_COMMAND_IDS = Object.freeze(
  CLAUDE_COMMANDS.filter((entry) => entry.type === "local").map((entry) => entry.id),
);
export function isClaudeCommandId(value) {
  return CLAUDE_COMMANDS.some((entry) => entry.id === value);
}
/** Describes one action. Returns data only; nothing is opened or copied here. */
export function describeClaudeAction(id, { session = null, appInstalled = true } = {}) {
  if (id === "selectOnly") return { available: true, type: "local", id: "selectOnly", label: "仅选中会话" };
  const entry = CLAUDE_COMMANDS.find((item) => item.id === id);
  if (!entry) return { available: false, reason: "这个操作未在 Claude Code 模式中提供" };
  if (entry.type === "unavailable") return { available: false, reason: entry.reason, id, label: entry.label };
  if (entry.type === "local") return { available: true, type: "local", id, label: entry.label };
  if (!session) return { available: false, id, label: entry.label, reason: "请先选择一个会话" };
  if (entry.type === "openSession") {
    if (!appInstalled)
      return { available: false, id, label: entry.label, reason: "这台电脑上没有找到 Claude 桌面应用。" };
    if (!DESKTOP_ID.test(String(session.desktopId ?? "")))
      return {
        available: false, id, label: entry.label,
        reason: "Claude 桌面应用里没有这个会话的记录（终端里启动的会话就没有），无法定位到具体会话；可改用「打开项目文件夹」或「复制恢复命令」。",
      };
    return { available: true, type: "openSession", id, label: entry.label, url: CLAUDE_SESSION_URL(session.desktopId) };
  }
  if (entry.type === "focusApp") {
    if (!appInstalled)
      return { available: false, id, label: entry.label, reason: "这台电脑上没有找到 Claude 桌面应用。" };
    if (session.entrypoint && session.entrypoint !== "claude-desktop")
      return {
        available: false, id, label: entry.label,
        reason: "这个会话运行在终端里，NuPhyLab 无法可靠定位该终端窗口；可改用「打开项目文件夹」或「复制恢复命令」。",
      };
    return { available: true, type: "focusApp", id, label: entry.label, bundleId: CLAUDE_APP_BUNDLE_ID };
  }
  if (entry.type === "revealProject") {
    if (!session.cwd) return { available: false, id, label: entry.label, reason: "这个会话还没有记录项目目录" };
    return { available: true, type: "revealProject", id, label: entry.label, path: session.cwd };
  }
  if (entry.type === "copyResume") {
    if (!session.id) return { available: false, id, label: entry.label, reason: "这个会话还没有可用的会话标识" };
    // Windows forbids double quotes in paths, so plain double quoting is safe;
    // /d also switches drive letters, which plain cd would ignore.
    const target = session.cwd
      ? IS_WIN
        ? `cd /d "${session.cwd}" && `
        : `cd ${quote(session.cwd)} && `
      : "";
    return { available: true, type: "copyResume", id, label: entry.label, text: `${target}claude --resume ${session.id}` };
  }
  return { available: false, id, label: entry.label, reason: "这个操作暂不可用" };
}
function quote(value) {
  return `'${String(value).replaceAll("'", `'\\''`)}'`;
}
/** The first Claude desktop executable that exists on this machine, or null. */
export async function claudeAppPath(paths = CLAUDE_APP_PATHS) {
  for (const candidate of paths) {
    try {
      await access(candidate, constants.F_OK);
      return candidate;
    } catch {
      /* try the next known location */
    }
  }
  return null;
}
export async function claudeAppInstalled(paths = CLAUDE_APP_PATHS) {
  return (await claudeAppPath(paths)) !== null;
}
/** Carries out a described action with `open`/`pbcopy` (or `start`/`clip`); no key events. */
export class ClaudeActionRunner {
  constructor({ exec = run, launch = spawn, execRaw = execFile } = {}) {
    this.exec = exec;
    this.launch = launch;
    this.execRaw = execRaw;
  }
  async execute(action) {
    if (!action?.available) throw new Error(action?.reason ?? "这个操作暂不可用");
    if (action.type === "openSession") {
      if (typeof action.url !== "string" || !action.url.startsWith("claude://code/continue?session=local_"))
        throw new Error("会话链接无效");
      try {
        if (IS_WIN) {
          // `start "" <url>` resolves the claude:// protocol through the shell.
          // The session id was already regex-validated, so the URL is inert.
          await this.exec(CMD_EXE, ["/d", "/s", "/c", `start "" "${action.url}"`], { timeout: 8000 });
        } else {
          await this.exec("/usr/bin/open", ["--", action.url], { timeout: 8000 });
        }
      } catch {
        throw new Error("无法打开这个会话，请确认 Claude 桌面应用已安装。");
      }
      return { opened: action.url };
    }
    if (action.type === "focusApp") {
      if (!IS_WIN) {
        try {
          await this.exec("/usr/bin/open", ["-b", action.bundleId], { timeout: 8000 });
        } catch {
          throw new Error("无法切换到 Claude 应用，请确认它已安装。");
        }
        return { activated: action.bundleId };
      }
      // describeClaudeAction carries bundleId only, so resolve the executable
      // here; an explicit appPath lets callers and tests pin the location.
      const exe = action.appPath ?? (await claudeAppPath());
      if (!exe) throw new Error("无法切换到 Claude 应用，请确认它已安装。");
      // Claude is a single-instance Electron app: launching the executable
      // focuses the running window instead of starting a second copy. The
      // process keeps running, so never wait for it to exit.
      this.launch(exe, [], { detached: true, stdio: "ignore", windowsHide: true })?.unref?.();
      return { activated: exe };
    }
    if (action.type === "revealProject") {
      if (typeof action.path !== "string" || !path.isAbsolute(action.path)) throw new Error("项目目录无效");
      if (IS_WIN) {
        try {
          await access(action.path, constants.F_OK);
        } catch {
          throw new Error("无法打开项目文件夹，可能已被移动或删除。");
        }
        // explorer.exe exits non-zero even on success, so detach and ignore it.
        this.launch("explorer.exe", [action.path], { detached: true, stdio: "ignore" })?.unref?.();
        return { opened: action.path };
      }
      try {
        await this.exec("/usr/bin/open", ["--", action.path], { timeout: 8000 });
      } catch {
        throw new Error("无法打开项目文件夹，可能已被移动或删除。");
      }
      return { opened: action.path };
    }
    if (action.type === "copyResume") {
      const clipboard = IS_WIN ? CLIP_EXE : "/usr/bin/pbcopy";
      await new Promise((resolve, reject) => {
        const child = this.execRaw(clipboard, { timeout: 5000, windowsHide: true }, (error) =>
          error ? reject(new Error("无法写入剪贴板")) : resolve(),
        );
        child.stdin.end(action.text);
      });
      return { copied: true, text: action.text };
    }
    return { handled: action.type };
  }
}
