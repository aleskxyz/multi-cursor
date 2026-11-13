import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

interface Environment {
  id: string;
  name: string;
  createdAt: string;
}

interface Account {
  id: string;
  envId: string;
  /** Automatic name from the Cursor profile / email. */
  name: string;
  /** Custom name set in Multi Cursor; shown instead of `name` when present. */
  label?: string | null;
  email?: string | null;
  updatedAt: string;
  pendingLogin: boolean;
}

/** Plan and usage from Cursor's servers (amounts in US cents). */
interface AccountUsage {
  status: "ok" | "expired" | "locked" | "unavailable" | string;
  planName?: string | null;
  subscriptionStatus?: string | null;
  /** Why the status isn't "ok". */
  detail?: string | null;
  /** Share of included usage spent, 0–100. */
  usedPercent?: number | null;
  includedUsedCents?: number | null;
  includedLimitCents?: number | null;
  bonusCents?: number | null;
  onDemandEnabled?: boolean | null;
  onDemandUsedCents?: number | null;
  onDemandLimitCents?: number | null;
  /** When included usage resets (ISO 8601). */
  resetsAt?: string | null;
  /** Prepaid "additional usage" credit left. */
  creditAvailableCents?: number | null;
  /** Prepaid credit spent this calendar month. */
  creditSpentMonthCents?: number | null;
  creditAutoTopUp?: boolean | null;
  creditTopUpAmountCents?: number | null;
  creditTopUpThresholdCents?: number | null;
  /** Spend beyond the prepaid credit. */
  creditUncoveredCents?: number | null;
  /** When the monthly additional-usage figures reset (ISO 8601). */
  monthlyResetsAt?: string | null;
  fetchedAt: string;
}

interface ActiveSelection {
  envId?: string | null;
  accountId?: string | null;
}

interface AppConfig {
  environments: Environment[];
  accounts: Account[];
  active: ActiveSelection;
  cursorAppPath?: string | null;
}

interface AppState {
  config: AppConfig;
  cursorRunning: boolean;
  rootDir: string;
  os: "macos" | "linux" | "windows" | string;
  trashName: string;
}

interface ListStateResult {
  state: AppState;
  capturedEmail?: string | null;
  /** Environments rebuilt from data folders the config didn't know about. */
  recovered?: string[];
}

type PromptResult = string | null;

interface TextPromptOptions {
  title: string;
  initial?: string;
  confirmLabel?: string;
  checkboxLabel?: string;
  checkboxDefault?: boolean;
}

interface TextPromptResult {
  value: string;
  checked: boolean;
}

interface CopyProgressEvent {
  percent: number;
  label: string;
}

const els = {
  envSelect: document.querySelector("#env-select") as HTMLSelectElement,
  envDisk: document.querySelector("#env-disk") as HTMLElement,
  accountList: document.querySelector("#account-list") as HTMLUListElement,
  status: document.querySelector("#status") as HTMLElement,
  error: document.querySelector("#error") as HTMLElement,
  success: document.querySelector("#success") as HTMLElement,
  progress: document.querySelector("#progress") as HTMLElement,
  progressLabel: document.querySelector("#progress-label") as HTMLElement,
  progressBar: document.querySelector("#progress-bar") as HTMLElement,
  launch: document.querySelector("#launch") as HTMLButtonElement,
  switchBtn: document.querySelector("#switch") as HTMLButtonElement,
  envNew: document.querySelector("#env-new") as HTMLButtonElement,
  envRename: document.querySelector("#env-rename") as HTMLButtonElement,
  envDelete: document.querySelector("#env-delete") as HTMLButtonElement,
  acctNew: document.querySelector("#acct-new") as HTMLButtonElement,
  acctRename: document.querySelector("#acct-rename") as HTMLButtonElement,
  acctDelete: document.querySelector("#acct-delete") as HTMLButtonElement,
  modal: document.querySelector("#modal") as HTMLElement,
  modalTitle: document.querySelector("#modal-title") as HTMLElement,
  modalMessage: document.querySelector("#modal-message") as HTMLElement,
  modalLink: document.querySelector("#modal-link") as HTMLAnchorElement,
  modalInput: document.querySelector("#modal-input") as HTMLInputElement,
  modalCheckWrap: document.querySelector("#modal-check-wrap") as HTMLElement,
  modalCheck: document.querySelector("#modal-check") as HTMLInputElement,
  modalCheckLabel: document.querySelector("#modal-check-label") as HTMLElement,
  modalCancel: document.querySelector("#modal-cancel") as HTMLButtonElement,
  modalConfirm: document.querySelector("#modal-confirm") as HTMLButtonElement,
  ctxMenu: document.querySelector("#ctx-menu") as HTMLElement,
  ctxLaunch: document.querySelector("#ctx-launch") as HTMLButtonElement,
  ctxSwitch: document.querySelector("#ctx-switch") as HTMLButtonElement,
  ctxRename: document.querySelector("#ctx-rename") as HTMLButtonElement,
  ctxDelete: document.querySelector("#ctx-delete") as HTMLButtonElement,
  ctxUsage: document.querySelector("#ctx-usage") as HTMLButtonElement,
  footerLinks: document.querySelector("#footer-links") as HTMLElement,
  footerVersion: document.querySelector("#footer-version") as HTMLElement,
  aboutLink: document.querySelector("#about-link") as HTMLButtonElement,
  updatesLink: document.querySelector("#updates-link") as HTMLButtonElement,
};

interface AboutInfo {
  name: string;
  version: string;
  description: string;
  copyright: string;
  repoUrl: string;
}

interface UpdateCheckResult {
  updateAvailable: boolean;
  currentVersion: string;
  latestVersion: string;
  releaseUrl: string;
  message: string;
}

let state: AppState | null = null;
let selectedEnvId: string | null = null;
let selectedAccountId: string | null = null;
/** User picked an account in the list; do not clobber from polling/refresh. */
let userPickedAccount = false;
/** Last plan/usage result per account id; the backend also caches for a few minutes. */
const usageCache = new Map<string, AccountUsage>();
/** When each `usageCache` entry arrived (local clock; `fetchedAt` is when the backend fetched it). */
const usageReceivedAt = new Map<string, number>();
const usageInFlight = new Set<string>();

function forgetUsage(accountId: string) {
  usageCache.delete(accountId);
  usageReceivedAt.delete(accountId);
}

function accountCard(accountId: string): HTMLElement | null {
  return els.accountList.querySelector<HTMLElement>(`li[data-id="${CSS.escape(accountId)}"]`);
}
let modalResolver: ((value: PromptResult) => void) | null = null;
/** Captured before modal fields are cleared (checkbox would otherwise always be false). */
let lastModalChecked = false;
let busy = false;
let busyLabel: string | null = null;
let diskUsageEnvId: string | null = null;
let diskUsageInFlight = false;
/** Env id to measure after the in-flight `du` finishes (rapid dropdown switches). */
let diskUsageQueued: string | null = null;

let toastTimer: number | undefined;

/** Hide the floating message after `ms` (none for sticky "working…" messages). */
function scheduleToastHide(ms: number | null) {
  window.clearTimeout(toastTimer);
  if (ms != null) toastTimer = window.setTimeout(() => hideToasts(), ms);
}

function hideToast(el: HTMLElement) {
  el.classList.add("hidden");
  el.textContent = "";
}

function hideToasts() {
  window.clearTimeout(toastTimer);
  hideToast(els.error);
  hideToast(els.success);
}

/** Show one toast (replacing the other), or hide it when `message` is empty. */
function showToast(el: HTMLElement, message: string | null, hideAfterMs: number | null) {
  if (!message) {
    hideToast(el);
    return;
  }
  hideToast(el === els.error ? els.success : els.error);
  el.textContent = message;
  el.classList.remove("hidden");
  scheduleToastHide(hideAfterMs);
}

function showError(message: string | null) {
  showToast(els.error, message, 10_000);
}

function showSuccess(message: string | null, opts?: { sticky?: boolean }) {
  showToast(els.success, message, opts?.sticky ? null : 5_000);
}

function setProgress(percent: number | null, label?: string) {
  if (percent === null) {
    els.progress.classList.add("hidden");
    els.progressBar.style.width = "0%";
    els.progressLabel.textContent = "";
    els.progress.removeAttribute("aria-valuenow");
    return;
  }
  const p = Math.max(0, Math.min(100, Math.round(percent)));
  els.progress.classList.remove("hidden");
  els.progressBar.style.width = `${p}%`;
  els.progressLabel.textContent = label ? `${label} ${p}%` : `Copying… ${p}%`;
  els.progress.setAttribute("aria-valuenow", String(p));
}

function hideModalLink() {
  els.modalLink.classList.add("hidden");
  els.modalLink.removeAttribute("href");
  els.modalLink.textContent = "";
}

function closeModal(value: PromptResult) {
  // Read checkbox BEFORE clearing — askTextWithCheckbox needs this value.
  lastModalChecked = els.modalCheck.checked;
  els.modal.classList.add("hidden");
  els.modalInput.value = "";
  els.modalCheck.checked = false;
  els.modalCheckWrap.classList.add("hidden");
  hideModalLink();
  els.modalCancel.classList.remove("hidden");
  const resolve = modalResolver;
  modalResolver = null;
  // Defer past paint so “Restart” can hide the dialog before any IPC work.
  window.setTimeout(() => resolve?.(value), 0);
}

/** Wait until the browser has painted pending DOM updates (WKWebView needs a real timer). */
async function yieldToPaint(): Promise<void> {
  await new Promise<void>((resolve) => {
    requestAnimationFrame(() => {
      requestAnimationFrame(() => resolve());
    });
  });
  await new Promise((r) => setTimeout(r, 80));
}

/** Show “Cursor is closing…”, paint, then quit (poll; force if needed). */
async function quitCursorWithUi(): Promise<void> {
  busy = true;
  busyLabel = "Closing…";
  showError(null);
  showSuccess("Cursor is closing…", { sticky: true });
  render();
  await yieldToPaint();

  await call<void>("quit_cursor_cmd");
  let closed = await waitForCursorState(false, 12_000);
  if (!closed) {
    await call<void>("force_quit_cursor_cmd");
    closed = await waitForCursorState(false, 8_000);
  }
  if (!closed) {
    throw "Cursor did not quit in time. Quit it manually and try again.";
  }
  if (state) state.cursorRunning = false;
}

/** Resolves to the entered text ("" when left empty) or null on Cancel. */
function askText(
  title: string,
  initial = "",
  confirmLabel = "OK",
  message?: string,
): Promise<PromptResult> {
  return new Promise((resolve) => {
    modalResolver = resolve;
    els.modalTitle.textContent = title;
    els.modalMessage.textContent = message ?? "";
    els.modalMessage.classList.toggle("hidden", !message);
    hideModalLink();
    els.modalInput.classList.remove("hidden");
    els.modalInput.value = initial;
    els.modalCheckWrap.classList.add("hidden");
    els.modalCancel.classList.remove("hidden");
    els.modalConfirm.textContent = confirmLabel;
    els.modalConfirm.classList.remove("danger");
    els.modal.classList.remove("hidden");
    requestAnimationFrame(() => {
      els.modalInput.focus();
      els.modalInput.select();
    });
  });
}

function askTextWithCheckbox(opts: TextPromptOptions): Promise<TextPromptResult | null> {
  return new Promise((resolve) => {
    modalResolver = (value) => {
      if (!value) {
        resolve(null);
        return;
      }
      // lastModalChecked is set in closeModal before the checkbox is cleared.
      resolve({ value, checked: lastModalChecked });
    };
    els.modalTitle.textContent = opts.title;
    els.modalMessage.classList.add("hidden");
    els.modalMessage.textContent = "";
    hideModalLink();
    els.modalInput.classList.remove("hidden");
    els.modalInput.value = opts.initial ?? "";
    els.modalCheckWrap.classList.remove("hidden");
    els.modalCheckLabel.textContent = opts.checkboxLabel ?? "";
    els.modalCheck.checked = Boolean(opts.checkboxDefault);
    els.modalCancel.classList.remove("hidden");
    els.modalConfirm.textContent = opts.confirmLabel ?? "OK";
    els.modalConfirm.classList.remove("danger");
    els.modal.classList.remove("hidden");
    requestAnimationFrame(() => {
      els.modalInput.focus();
      els.modalInput.select();
    });
  });
}

function askConfirm(
  title: string,
  message: string,
  confirmLabel = "OK",
  danger = false,
): Promise<boolean> {
  return new Promise((resolve) => {
    modalResolver = (value) => resolve(value === "ok");
    els.modalTitle.textContent = title;
    els.modalMessage.textContent = message;
    els.modalMessage.classList.remove("hidden");
    hideModalLink();
    els.modalInput.classList.add("hidden");
    els.modalCheckWrap.classList.add("hidden");
    els.modalCancel.classList.remove("hidden");
    els.modalConfirm.textContent = confirmLabel;
    els.modalConfirm.classList.toggle("danger", danger);
    els.modal.classList.remove("hidden");
    requestAnimationFrame(() => els.modalConfirm.focus());
  });
}

function showMessageDialog(
  title: string,
  message: string,
  linkUrl?: string | null,
): Promise<void> {
  return new Promise((resolve) => {
    modalResolver = () => resolve();
    els.modalTitle.textContent = title;
    els.modalMessage.textContent = message;
    els.modalMessage.classList.remove("hidden");
    if (linkUrl) {
      els.modalLink.href = linkUrl;
      els.modalLink.textContent = linkUrl;
      els.modalLink.classList.remove("hidden");
    } else {
      hideModalLink();
    }
    els.modalInput.classList.add("hidden");
    els.modalCheckWrap.classList.add("hidden");
    els.modalCancel.classList.add("hidden");
    els.modalConfirm.textContent = "OK";
    els.modalConfirm.classList.remove("danger");
    els.modal.classList.remove("hidden");
    requestAnimationFrame(() => els.modalConfirm.focus());
  });
}

async function showAbout() {
  try {
    const info = await call<AboutInfo>("about_info");
    await showMessageDialog(
      info.name,
      `Version ${info.version}\n\n${info.description}\n\n${info.copyright}`,
      info.repoUrl,
    );
  } catch {
    // call() already surfaces the error in the UI
  }
}

/** macOS has these in the app menu; elsewhere they live in the footer. */
async function setupFooterLinks() {
  try {
    const info = await invoke<AboutInfo>("about_info");
    els.footerVersion.textContent = `v${info.version}`;
  } catch {
    els.footerVersion.textContent = "";
  }
  els.aboutLink.addEventListener("click", () => void showAbout());
  els.updatesLink.addEventListener("click", () => void checkForUpdates());
}

/** Right-click menu for an account. Items mirror the panel buttons. */
function openAccountMenu(account: Account, x: number, y: number) {
  if (busy) return;
  selectedAccountId = account.id;
  userPickedAccount = true;
  render(); // updates the buttons' enabled state for this account

  els.ctxLaunch.textContent = els.launch.disabled
    ? els.launch.textContent || "Launch"
    : "Launch";
  els.ctxLaunch.disabled = els.launch.disabled;
  els.ctxSwitch.disabled = els.switchBtn.disabled;
  els.ctxRename.disabled = els.acctRename.disabled;
  els.ctxDelete.disabled = els.acctDelete.disabled;
  els.ctxUsage.disabled = account.pendingLogin || usageInFlight.has(account.id);

  const menu = els.ctxMenu;
  menu.classList.remove("hidden");
  // Keep the menu inside the window.
  const { width, height } = menu.getBoundingClientRect();
  const left = Math.max(4, Math.min(x, window.innerWidth - width - 4));
  const top = Math.max(4, Math.min(y, window.innerHeight - height - 4));
  menu.style.left = `${left}px`;
  menu.style.top = `${top}px`;
  menu.querySelector<HTMLButtonElement>("button:not(:disabled)")?.focus();
}

function closeAccountMenu() {
  els.ctxMenu.classList.add("hidden");
}

function setupContextMenus() {
  // No browser menu (Back / Refresh / Print…) in the app; keep it only in
  // text fields, where it offers Cut / Copy / Paste.
  document.addEventListener("contextmenu", (e) => {
    const target = e.target as HTMLElement | null;
    if (target?.closest("input, textarea, [contenteditable='true']")) return;
    e.preventDefault();
    closeAccountMenu();
  });

  const run = (button: HTMLButtonElement) => {
    closeAccountMenu();
    if (!button.disabled) button.click();
  };
  els.ctxLaunch.addEventListener("click", () => run(els.launch));
  els.ctxSwitch.addEventListener("click", () => run(els.switchBtn));
  els.ctxRename.addEventListener("click", () => run(els.acctRename));
  els.ctxDelete.addEventListener("click", () => run(els.acctDelete));
  els.ctxUsage.addEventListener("click", () => {
    closeAccountMenu();
    if (!selectedAccountId) return;
    const id = selectedAccountId;
    forgetUsage(id);
    const card = accountCard(id);
    if (card) renderUsage(card, undefined);
    void loadUsage(id, true);
  });

  // Close on outside click, Escape, scroll, resize or losing focus.
  document.addEventListener("mousedown", (e) => {
    if (!els.ctxMenu.contains(e.target as Node)) closeAccountMenu();
  });
  document.addEventListener("keydown", (e) => {
    if (els.ctxMenu.classList.contains("hidden")) return;
    if (e.key === "Escape") {
      e.preventDefault();
      closeAccountMenu();
    } else if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      const items = [...els.ctxMenu.querySelectorAll<HTMLButtonElement>("button:not(:disabled)")];
      if (!items.length) return;
      const i = items.indexOf(document.activeElement as HTMLButtonElement);
      const next = e.key === "ArrowDown" ? i + 1 : i - 1;
      items[(next + items.length) % items.length]!.focus();
    }
  });
  window.addEventListener("blur", closeAccountMenu);
  window.addEventListener("resize", closeAccountMenu);
  els.accountList.addEventListener("scroll", closeAccountMenu);
}

async function checkForUpdates() {
  try {
    const result = await call<UpdateCheckResult>("check_for_updates");
    await showMessageDialog(
      result.updateAvailable ? "Update available" : "Check for Updates",
      result.message,
      result.updateAvailable ? result.releaseUrl : null,
    );
  } catch {
    // call() already surfaces the error in the UI
  }
}

async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  try {
    return await invoke<T>(cmd, args);
  } catch (err) {
    const message = typeof err === "string" ? err : String(err);
    showError(message);
    throw err;
  }
}

function accountsForEnv(envId: string | null): Account[] {
  if (!state || !envId) return [];
  return state.config.accounts.filter((a) => a.envId === envId);
}

function isActiveSelectionRunning(): boolean {
  if (!state?.cursorRunning || !selectedEnvId || !selectedAccountId) return false;
  return (
    state.config.active.envId === selectedEnvId &&
    state.config.active.accountId === selectedAccountId
  );
}

function accountDisplayName(account: Account): string {
  return account.label?.trim() || account.name || account.email || "Account";
}

/** Label for confirm dialogs — includes email when it distinguishes the account. */
function accountConfirmLabel(account: Account): string {
  const name = accountDisplayName(account).trim();
  const email = account.email?.trim();
  if (name && email && name !== email) return `${name} (${email})`;
  return name || email || "Account";
}

function accountInitials(account: Account): string {
  if (account.pendingLogin) return "?";
  const source = accountDisplayName(account).trim();
  if (!source) return "?";
  if (source.includes("@")) {
    const local = source.split("@")[0] ?? source;
    const parts = local.split(/[._+\-\s]+/).filter(Boolean);
    if (parts.length >= 2) {
      return `${parts[0]![0] ?? ""}${parts[1]![0] ?? ""}`.toUpperCase();
    }
    return local.slice(0, 2).toUpperCase();
  }
  const parts = source.split(/\s+/).filter(Boolean);
  if (parts.length >= 2) {
    const first = [...(parts[0] ?? "")][0] ?? "";
    const last = [...(parts[parts.length - 1] ?? "")][0] ?? "";
    return `${first}${last}`.toUpperCase();
  }
  return [...source].slice(0, 2).join("").toUpperCase();
}

const AVATAR_COLORS = [
  "#6b5cad",
  "#3d6ea8",
  "#2f7d6d",
  "#8a5a3c",
  "#4a6fa5",
  "#5c6bc0",
  "#3d7a8c",
];

function avatarColor(seed: string): string {
  let hash = 0;
  for (let i = 0; i < seed.length; i++) {
    hash = (hash * 31 + seed.charCodeAt(i)) >>> 0;
  }
  return AVATAR_COLORS[hash % AVATAR_COLORS.length]!;
}

function formatDiskUsage(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return "—";
  const mb = bytes / (1024 * 1024);
  if (mb < 1024) return `${Math.max(1, Math.round(mb))} MB`;
  const gb = mb / 1024;
  return `${gb >= 10 ? gb.toFixed(0) : gb.toFixed(1)} GB`;
}

async function refreshEnvDiskUsage(force = false) {
  if (!selectedEnvId) {
    diskUsageEnvId = null;
    diskUsageQueued = null;
    els.envDisk.textContent = "Disk usage: —";
    return;
  }
  if (!force && diskUsageEnvId === selectedEnvId) return;
  if (diskUsageInFlight) {
    diskUsageQueued = selectedEnvId;
    return;
  }
  diskUsageInFlight = true;
  const envId = selectedEnvId;
  if (diskUsageEnvId !== envId) {
    els.envDisk.textContent = "Disk usage: …";
  }
  try {
    const bytes = await invoke<number>("environment_disk_usage", { envId });
    if (selectedEnvId === envId) {
      diskUsageEnvId = envId;
      els.envDisk.textContent = `Disk usage: ${formatDiskUsage(bytes)}`;
    }
  } catch {
    if (selectedEnvId === envId) {
      els.envDisk.textContent = "Disk usage: —";
    }
  } finally {
    diskUsageInFlight = false;
    const queued = diskUsageQueued;
    diskUsageQueued = null;
    if (queued && selectedEnvId === queued && diskUsageEnvId !== queued) {
      void refreshEnvDiskUsage(true);
    }
  }
}

function render() {
  if (!state) return;
  const { config, cursorRunning } = state;
  els.footerLinks.classList.toggle("hidden", state.os === "macos");
  document.documentElement.dataset.os = state.os;

  els.status.textContent = cursorRunning ? "Cursor running" : "Cursor idle";
  els.status.classList.toggle("running", cursorRunning);
  els.status.classList.toggle("idle", !cursorRunning);

  if (!selectedEnvId || !config.environments.some((e) => e.id === selectedEnvId)) {
    selectedEnvId = config.active.envId ?? config.environments[0]?.id ?? null;
  }

  els.envSelect.innerHTML = "";
  if (config.environments.length === 0) {
    const opt = document.createElement("option");
    opt.value = "";
    opt.textContent = "No environments";
    els.envSelect.appendChild(opt);
    els.envSelect.disabled = true;
  } else {
    els.envSelect.disabled = busy;
    for (const env of config.environments) {
      const opt = document.createElement("option");
      opt.value = env.id;
      const isCurrent = env.id === config.active.envId;
      opt.textContent = isCurrent ? `${env.name} (current)` : env.name;
      if (env.id === selectedEnvId) opt.selected = true;
      els.envSelect.appendChild(opt);
    }
  }

  const accounts = accountsForEnv(selectedEnvId);
  if (!selectedAccountId || !accounts.some((a) => a.id === selectedAccountId)) {
    selectedAccountId =
      (config.active.envId === selectedEnvId ? config.active.accountId : null) ??
      accounts[0]?.id ??
      null;
  }

  els.accountList.innerHTML = "";
  for (const account of accounts) {
    const li = document.createElement("li");
    li.dataset.id = account.id;
    if (account.id === selectedAccountId) li.classList.add("selected");

    const avatar = document.createElement("div");
    avatar.className = "avatar";
    avatar.textContent = accountInitials(account);
    avatar.style.background = avatarColor(account.id || accountDisplayName(account));
    li.appendChild(avatar);

    const body = document.createElement("div");
    body.className = "body";

    const name = document.createElement("div");
    name.className = "name";
    name.textContent = accountDisplayName(account);
    body.appendChild(name);

    const meta = document.createElement("div");
    meta.className = "meta";
    if (account.pendingLogin) {
      meta.textContent = "Sign in inside Cursor — email will appear here";
    } else if (account.email) {
      meta.textContent = account.email;
    }
    if (meta.textContent) body.appendChild(meta);
    li.appendChild(body);

    // Right column: plan pill + status badge, reset time underneath.
    const side = document.createElement("div");
    side.className = "side";
    const sideTop = document.createElement("div");
    sideTop.className = "side-top";
    const planSlot = document.createElement("span");
    planSlot.className = "plan-slot";
    sideTop.appendChild(planSlot);
    const isCurrent =
      account.id === config.active.accountId && account.envId === config.active.envId;
    if (account.pendingLogin) {
      const badge = document.createElement("span");
      badge.className = "badge";
      badge.textContent = "waiting";
      sideTop.appendChild(badge);
    } else if (isCurrent) {
      // Green dot on the avatar marks Cursor's current account.
      avatar.classList.add("is-current");
      avatar.title = "Current account";
      avatar.setAttribute("aria-label", "Current account");
    }
    side.appendChild(sideTop);
    const sideNote = document.createElement("div");
    sideNote.className = "side-note";
    side.appendChild(sideNote);
    li.appendChild(side);

    // Full-width meters under the header row.
    const usageBox = document.createElement("div");
    usageBox.className = "usage";
    li.appendChild(usageBox);
    if (!account.pendingLogin) renderUsage(li, usageCache.get(account.id));

    li.addEventListener("click", () => {
      if (busy) return;
      selectedAccountId = account.id;
      userPickedAccount = true;
      render();
    });
    li.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      e.stopPropagation();
      openAccountMenu(account, e.clientX, e.clientY);
    });
    li.addEventListener("dblclick", () => {
      if (busy) return;
      selectedAccountId = account.id;
      userPickedAccount = true;
      void doLaunch();
    });
    els.accountList.appendChild(li);
  }

  const hasEnv = Boolean(selectedEnvId);
  const hasAcct = Boolean(selectedAccountId);
  const alreadyRunning = isActiveSelectionRunning();

  const isCurrentEnv = Boolean(
    selectedEnvId && config.active.envId === selectedEnvId,
  );
  const onlyEnv = config.environments.length <= 1;

  els.envNew.disabled = busy;
  els.envRename.disabled = busy || !hasEnv;
  els.envDelete.disabled = busy || !hasEnv || isCurrentEnv || onlyEnv;
  els.acctNew.disabled = busy || !hasEnv;
  els.acctRename.disabled = busy || !hasAcct;
  els.acctDelete.disabled = busy || !hasAcct;
  els.launch.disabled = busy || !hasEnv || !hasAcct || alreadyRunning;
  const selectionIsCurrent =
    config.active.envId === selectedEnvId && config.active.accountId === selectedAccountId;
  els.switchBtn.disabled = busy || !hasEnv || !hasAcct || selectionIsCurrent;

  if (busy && busyLabel) {
    els.launch.textContent =
      busyLabel === "Closing…" ||
      busyLabel === "Launching…" ||
      busyLabel === "Switching…" ||
      busyLabel === "Copying…" ||
      busyLabel === "Creating…" ||
      busyLabel === "Deleting…"
        ? busyLabel
        : "Launch";
  } else if (alreadyRunning) {
    els.launch.textContent = "Running";
  } else {
    els.launch.textContent = "Launch";
  }

  // Defer so the accounts list paints before the (slow) disk-usage IPC starts.
  window.setTimeout(() => void refreshEnvDiskUsage(false), 0);
  window.setTimeout(() => {
    for (const account of accounts) {
      if (!account.pendingLogin) void loadUsage(account.id, false);
    }
  }, 0);
}

function formatDollars(cents: number): string {
  const dollars = cents / 100;
  return dollars >= 100 || Number.isInteger(dollars)
    ? `$${Math.round(dollars)}`
    : `$${dollars.toFixed(2)}`;
}

function percentText(pct: number): string {
  return pct > 0 && pct < 1 ? pct.toFixed(1) : String(Math.round(pct));
}

/** Short money for tight columns: "$1.36", "$31", "$120". */
function shortDollars(cents: number): string {
  const dollars = cents / 100;
  return dollars >= 10 ? `$${Math.round(dollars)}` : formatDollars(cents);
}

/** "5d", "18h", "now" until an ISO time; null if unparseable. */
function shortUntil(iso: string): string | null {
  const ms = new Date(iso).getTime() - Date.now();
  if (!Number.isFinite(ms)) return null;
  if (ms <= 0) return "now";
  const hours = ms / 3_600_000;
  return hours < 24 ? `${Math.max(1, Math.round(hours))}h` : `${Math.round(hours / 24)}d`;
}

/**
 * "Dec 1" in UTC (month boundaries are UTC, so it never shows "Nov 30").
 * Used for resets at most a month away, so no year; `full` (tooltips) adds
 * the weekday and year.
 */
function shortDate(iso: string, full = false): string | null {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return null;
  return d.toLocaleDateString(undefined, {
    month: "short",
    day: "numeric",
    timeZone: "UTC",
    ...(full ? { weekday: "short", year: "numeric" } : {}),
  });
}

/** One meter row: label | bar | value | aside (reset time or ⚠). Details on hover. */
function meterRow(opts: {
  label: string;
  pct: number | null;
  value: string;
  aside?: string;
  alert?: boolean;
  tooltip: string;
}): HTMLElement {
  const row = document.createElement("div");
  row.className = "meter";
  row.title = opts.tooltip;
  const name = document.createElement("span");
  name.className = "meter-label";
  name.textContent = opts.label;
  const bar = document.createElement("div");
  bar.className = "usage-bar";
  const fill = document.createElement("div");
  const shown = opts.pct == null ? 0 : Math.min(100, Math.max(0, opts.pct));
  // Keep a sliver visible once anything is used.
  fill.style.width = `${shown > 0 ? Math.max(shown, 1.5) : 0}%`;
  if (shown >= 90) fill.classList.add("high");
  bar.appendChild(fill);
  const val = document.createElement("span");
  val.className = "meter-value";
  val.textContent = opts.value;
  const aside = document.createElement("span");
  aside.className = opts.alert ? "meter-aside alert" : "meter-aside";
  aside.textContent = opts.aside ?? "";
  row.append(name, bar, val, aside);
  return row;
}

/** Fill a card's plan pill, status note and meters. Every field is optional. */
function renderUsage(card: HTMLElement, usage: AccountUsage | undefined) {
  const planSlot = card.querySelector<HTMLElement>(".plan-slot");
  const note = card.querySelector<HTMLElement>(".side-note");
  const box = card.querySelector<HTMLElement>(".usage");
  if (!planSlot || !note || !box) return;
  planSlot.replaceChildren();
  planSlot.removeAttribute("title");
  note.textContent = "";
  box.replaceChildren();
  box.classList.remove("loading");
  box.removeAttribute("title");

  if (!usage) {
    box.classList.add("loading");
    box.textContent = "Loading…";
    return;
  }

  if (usage.planName) {
    const pill = document.createElement("span");
    pill.className = "plan-pill";
    pill.textContent = usage.planName;
    planSlot.appendChild(pill);
  }
  // Only problems go under the plan pill (e.g. "past due").
  const status = usage.subscriptionStatus;
  if (status && !/^(active|trialing)$/i.test(status)) note.textContent = status.replace(/_/g, " ");

  const message = (text: string) => {
    const d = document.createElement("div");
    d.className = "usage-message";
    d.textContent = text;
    box.appendChild(d);
  };
  if (usage.status === "expired") {
    message("Sign-in expired. Launch to refresh.");
    return;
  }
  if (usage.status === "locked") {
    // Key prompt denied or keyring unavailable; not retried automatically.
    message("Locked. Right-click → Refresh usage.");
    box.title = usage.detail ? `Couldn't unlock Cursor's saved login: ${usage.detail}` : "";
    return;
  }
  if (usage.status !== "ok") {
    planSlot.title = usage.detail ? `Usage unavailable: ${usage.detail}` : "Usage unavailable";
    return;
  }

  const monthly = usage.monthlyResetsAt ? shortDate(usage.monthlyResetsAt) : null;

  // Plan: included usage.
  const used = usage.includedUsedCents;
  const limit = usage.includedLimitCents;
  const pct =
    usage.usedPercent ??
    (used != null && limit != null && limit > 0 ? (used / limit) * 100 : null);
  if (pct != null || used != null) {
    const until = usage.resetsAt ? shortUntil(usage.resetsAt) : null;
    const tip: string[] = [];
    if (pct != null) {
      tip.push(
        limit != null && limit > 0
          ? `${percentText(pct)}% of ${formatDollars(limit)} included usage`
          : `${percentText(pct)}% of included usage`,
      );
    } else if (used != null) {
      tip.push(`${formatDollars(used)} of included usage`);
    }
    if (usage.bonusCents) tip.push(`+${formatDollars(usage.bonusCents)} bonus`);
    if (usage.resetsAt) {
      const when = shortDate(usage.resetsAt, true);
      if (when) tip.push(`resets ${when}`);
    }
    box.appendChild(
      meterRow({
        label: "Plan",
        pct,
        value: pct != null ? `${percentText(pct)}%` : shortDollars(used ?? 0),
        aside: until ? `↻ ${until}` : "",
        tooltip: tip.join(" · "),
      }),
    );
  }

  // Extra: post-paid on-demand usage.
  if (usage.onDemandEnabled) {
    const odUsed = usage.onDemandUsedCents ?? 0;
    const odLimit = usage.onDemandLimitCents;
    const capped = odLimit != null && odLimit > 0;
    const tip = capped
      ? `On-demand: ${formatDollars(odUsed)} of ${formatDollars(odLimit)} limit`
      : `On-demand: ${formatDollars(odUsed)}, no limit`;
    box.appendChild(
      meterRow({
        label: "Extra",
        pct: capped ? (odUsed / odLimit) * 100 : null,
        value: capped ? `${shortDollars(odUsed)}/${shortDollars(odLimit)}` : shortDollars(odUsed),
        aside: monthly ? `↻ ${monthly}` : "",
        tooltip: monthly ? `${tip} · resets ${monthly}` : tip,
      }),
    );
  }

  // Credit: prepaid "additional usage".
  if (usage.creditAvailableCents != null) {
    const left = usage.creditAvailableCents;
    const spent = usage.creditSpentMonthCents ?? 0;
    const total = left + spent;
    const over = usage.creditUncoveredCents ?? 0;
    // The balance itself never resets; only the monthly spend counter does.
    const tip = [
      `${formatDollars(left)} credit left`,
      monthly
        ? `${formatDollars(spent)} used this month (counter resets ${monthly})`
        : `${formatDollars(spent)} used this month`,
    ];
    if (usage.creditAutoTopUp) {
      const amt = usage.creditTopUpAmountCents;
      const at = usage.creditTopUpThresholdCents;
      tip.push(
        amt != null && at != null
          ? `auto top-up ${formatDollars(amt)} below ${formatDollars(at)}`
          : "auto top-up on",
      );
    }
    if (over > 0) tip.push(`⚠ ${formatDollars(over)} used beyond credit`);
    const alert = over > 0 || (left <= 0 && !usage.creditAutoTopUp);
    box.appendChild(
      meterRow({
        label: "Credit",
        pct: total > 0 ? (spent / total) * 100 : null,
        value: shortDollars(left),
        aside: alert ? "⚠" : "",
        alert,
        tooltip: tip.join(" · "),
      }),
    );
  }

  box.title = `Updated ${new Date(usage.fetchedAt).toLocaleTimeString()}`;
}

/** Fetch plan/usage for one account and update its card in place. */
async function loadUsage(accountId: string, force: boolean) {
  if (usageInFlight.has(accountId)) return;
  const cached = usageCache.get(accountId);
  const receivedAt = usageReceivedAt.get(accountId);
  if (!force && cached && receivedAt != null) {
    // render() runs on every poll; don't refetch each time. Only successes
    // are cached by the backend; retry anything else (e.g. "expired" after
    // Cursor refreshed the token) sooner.
    const ttl = cached.status === "ok" ? 5 * 60_000 : 60_000;
    if (Date.now() - receivedAt < ttl) return;
  }
  usageInFlight.add(accountId);
  try {
    // Not `call()`: a usage failure must never pop an error dialog.
    const usage = await invoke<AccountUsage>("account_usage", { accountId, force });
    usageCache.set(accountId, usage);
  } catch {
    usageCache.set(accountId, { status: "unavailable", fetchedAt: new Date().toISOString() });
  } finally {
    usageInFlight.delete(accountId);
    usageReceivedAt.set(accountId, Date.now());
  }
  const card = accountCard(accountId);
  if (card) renderUsage(card, usageCache.get(accountId));
}

async function refresh(opts?: { silent?: boolean; syncSelection?: boolean }) {
  const result = await call<ListStateResult>("list_state");
  state = result.state;
  // Only sync selection from "active" on first load — never clobber a user click.
  if (opts?.syncSelection || !userPickedAccount) {
    if (!selectedEnvId) {
      selectedEnvId = state.config.active.envId ?? state.config.environments[0]?.id ?? null;
    }
    if (!selectedAccountId || opts?.syncSelection) {
      selectedAccountId =
        state.config.active.accountId ??
        accountsForEnv(selectedEnvId)[0]?.id ??
        null;
    }
  }
  // Drop selection only if it no longer exists.
  if (selectedEnvId && !state.config.environments.some((e) => e.id === selectedEnvId)) {
    selectedEnvId = state.config.active.envId ?? state.config.environments[0]?.id ?? null;
    userPickedAccount = false;
    selectedAccountId = state.config.active.accountId ?? null;
  }
  if (
    selectedAccountId &&
    !state.config.accounts.some((a) => a.id === selectedAccountId)
  ) {
    selectedAccountId =
      (state.config.active.envId === selectedEnvId
        ? state.config.active.accountId
        : null) ??
      accountsForEnv(selectedEnvId)[0]?.id ??
      null;
  }
  if (result.recovered?.length) {
    const names = result.recovered.map((n) => `“${n}”`).join(", ");
    showSuccess(
      `Found Cursor data without a matching entry and restored it as ${names}. Rename or delete it if you don't need it.`,
      { sticky: true },
    );
  } else if (result.capturedEmail && !opts?.silent) {
    showSuccess(`Signed in as ${result.capturedEmail}.`);
  }
  render();
}

async function withBusy(label: string, fn: () => Promise<void>) {
  if (busy) return;
  busy = true;
  busyLabel = label;
  showError(null);
  render();
  try {
    await fn();
  } finally {
    busy = false;
    busyLabel = null;
    render();
  }
}

async function confirmQuitIfNeeded(reason: string): Promise<boolean> {
  if (!state?.cursorRunning) return true;
  return askConfirm(
    "Restart Cursor?",
    `${reason}\n\nCursor will quit and reopen as needed.`,
    "Continue",
    false,
  );
}

async function waitForCursorState(
  wantRunning: boolean,
  timeoutMs = 60_000,
): Promise<boolean> {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    const running = await invoke<boolean>("is_cursor_running");
    if (state) state.cursorRunning = running;
    if (running === wantRunning) return true;
    await new Promise((r) => setTimeout(r, 250));
  }
  return false;
}

/**
 * Make the selected account Cursor's current one. `open` (Launch) also starts
 * Cursor; without it (Switch) the user opens Cursor themselves. Either way a
 * running Cursor has to quit first, since it holds the auth database.
 */
async function doLaunch(open = true) {
  if (!selectedEnvId || !selectedAccountId || busy) return;
  if (open && isActiveSelectionRunning()) return;

  const envChanging = state?.config.active.envId !== selectedEnvId;
  const accountChanging = state?.config.active.accountId !== selectedAccountId;
  if (!open && !envChanging && !accountChanging) return; // already current
  const running = Boolean(state?.cursorRunning);
  const needsRestart = running && (envChanging || accountChanging);

  if (needsRestart || (envChanging && !running)) {
    const account = state?.config.accounts.find((a) => a.id === selectedAccountId);
    const env = state?.config.environments.find((e) => e.id === selectedEnvId);
    const label = account ? accountConfirmLabel(account) : "the selected account";
    const envName = env?.name ?? "the selected environment";
    const title = envChanging ? "Switching environments" : "Switching accounts";
    const message = envChanging
      ? `Activate the “${envName}” environment with the ${label} account?`
      : `Activate the ${label} account?`;
    const tail = !running
      ? ""
      : open
        ? "\n\nCursor will quit and reopen."
        : "\n\nCursor will quit. Open it again when you’re ready.";
    const ok = await askConfirm(
      title,
      message + tail,
      running ? (open ? "Restart" : "Quit & switch") : "Switch",
      false,
    );
    if (!ok) return;
  }

  if (busy) return;
  const mustQuit = needsRestart || running;
  const working = open ? "Launching…" : "Switching…";
  const workingMessage = open ? "Cursor is launching…" : "Switching…";
  try {
    if (mustQuit) {
      // Dialog is already closed; show closing feedback before any quit IPC.
      await quitCursorWithUi();
    } else {
      busy = true;
      busyLabel = working;
      showError(null);
      showSuccess(workingMessage, { sticky: true });
      render();
      await yieldToPaint();
    }

    busyLabel = working;
    showSuccess(workingMessage, { sticky: true });
    render();
    await yieldToPaint();

    const launchedId = selectedAccountId;
    state = await call<AppState>("launch", {
      envId: selectedEnvId,
      accountId: launchedId,
      open,
    });
    // Its usage may have been "expired" or read from an older token.
    forgetUsage(launchedId);

    if (!open) {
      showSuccess("Switched. Open Cursor whenever you’re ready.");
      return;
    }
    const opened = await waitForCursorState(true, 45_000);
    if (opened) {
      state.cursorRunning = true;
      showSuccess(null);
    } else {
      showSuccess("Launch was requested; Cursor is still starting…");
    }
  } catch (err) {
    if (typeof err === "string") showError(err);
  } finally {
    busy = false;
    busyLabel = null;
    render();
  }
}

window.addEventListener("DOMContentLoaded", () => {
  els.modalCancel.addEventListener("click", () => closeModal(null));
  els.modalConfirm.addEventListener("click", () => {
    if (els.modalInput.classList.contains("hidden")) {
      closeModal("ok");
      return;
    }
    // "" (confirmed but empty) is distinct from null (cancelled).
    closeModal(els.modalInput.value.trim());
  });
  els.modalLink.addEventListener("click", (e) => {
    e.preventDefault();
    const url = els.modalLink.getAttribute("href");
    if (url && url !== "#") {
      void invoke("open_url", { url }).catch((err) => {
        showError(typeof err === "string" ? err : String(err));
      });
    }
  });
  els.modalInput.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      els.modalConfirm.click();
    } else if (e.key === "Escape") {
      e.preventDefault();
      closeModal(null);
    }
  });
  els.modal.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      e.preventDefault();
      closeModal(null);
    }
  });
  void listen("check-for-updates", () => {
    void checkForUpdates();
  });

  els.envSelect.addEventListener("change", () => {
    selectedEnvId = els.envSelect.value || null;
    selectedAccountId = null;
    userPickedAccount = false;
    diskUsageEnvId = null;
    // Drop the previous env's accounts immediately so they never linger during `du`.
    els.accountList.innerHTML = "";
    els.envDisk.textContent = "Disk usage: …";
    render();
  });

  els.envNew.addEventListener("click", async () => {
    if (busy) return;
    const result = await askTextWithCheckbox({
      title: "New environment",
      confirmLabel: "Create",
      checkboxLabel: "Copy current environment (Cursor data, settings and extensions)",
      checkboxDefault: true,
    });
    if (!result) return;

    const wantCopy = result.checked;
    const didQuitForCopy = Boolean(wantCopy && state?.cursorRunning);
    if (didQuitForCopy) {
      const ok = await askConfirm(
        "Quit Cursor?",
        "Cursor must quit before its data folders can be copied into the new environment.",
        "Quit & Create",
        false,
      );
      if (!ok) return;
    }

    let unlistenProgress: UnlistenFn | null = null;
    busy = true;
    showError(null);
    setProgress(null);
    let createdId: string | null = null;
    try {
      // 1) Create empty env and select it immediately (accounts list clears).
      busyLabel = "Creating…";
      showSuccess("Creating environment…", { sticky: true });
      render();
      await yieldToPaint();

      state = await call<AppState>("create_environment", {
        name: result.value,
      });
      const created =
        state.config.environments.find((e) => e.name === result.value) ??
        state.config.environments[state.config.environments.length - 1];
      createdId = created?.id ?? null;
      selectedEnvId = createdId;
      selectedAccountId = null;
      userPickedAccount = false;
      render();
      await yieldToPaint();

      if (wantCopy && createdId) {
        if (didQuitForCopy) {
          await quitCursorWithUi();
          await new Promise((r) => setTimeout(r, 500));
        }

        busyLabel = "Copying…";
        showSuccess(null);
        setProgress(0, "Copying…");
        unlistenProgress = await listen<CopyProgressEvent>("env-copy-progress", (ev) => {
          setProgress(ev.payload.percent, ev.payload.label);
        });
        render();
        await yieldToPaint();

        state = await call<AppState>("copy_environment_from_current", {
          envId: createdId,
        });
        // Keep selection on the new env after copy.
        selectedEnvId = createdId;
        selectedAccountId = null;
        userPickedAccount = false;
      }

      setProgress(null);
      // Do not auto-launch Cursor — new env has no accounts until the user adds one.
      diskUsageEnvId = null;
      void refreshEnvDiskUsage(true);
      showSuccess(
        wantCopy
          ? `Environment “${result.value}” created (as a copy).`
          : `Environment “${result.value}” created.`,
      );
    } catch (err) {
      setProgress(null);
      if (typeof err === "string") showError(err);
      // Keep the new env selected even if copy failed; do not auto-launch Cursor.
      if (createdId) {
        selectedEnvId = createdId;
        selectedAccountId = null;
        userPickedAccount = false;
      }
      try {
        await refresh({ silent: true });
        if (createdId) {
          selectedEnvId = createdId;
          selectedAccountId = null;
          userPickedAccount = false;
        }
      } catch {
        /* ignore */
      }
    } finally {
      if (unlistenProgress) unlistenProgress();
      busy = false;
      busyLabel = null;
      setProgress(null);
      render();
    }
  });

  els.envRename.addEventListener("click", async () => {
    if (!selectedEnvId || !state || busy) return;
    const current = state.config.environments.find((e) => e.id === selectedEnvId);
    const name = await askText("Rename environment", current?.name ?? "");
    if (!name) return;
    await withBusy("Renaming…", async () => {
      state = await call<AppState>("rename_environment", {
        id: selectedEnvId,
        name,
      });
      showSuccess(`Renamed to “${name}”.`);
    });
  });

  els.envDelete.addEventListener("click", async () => {
    if (!selectedEnvId || !state || busy) return;
    if (selectedEnvId === state.config.active.envId) {
      showError("Cannot delete the current environment. Switch to another one first.");
      return;
    }
    const current = state.config.environments.find((e) => e.id === selectedEnvId);
    const trashName = state.trashName || "Trash";
    const ok = await askConfirm(
      "Delete environment?",
      `Move “${current?.name ?? "this environment"}” (inactive data + ~/.cursor pool + saved logins) to ${trashName}?`,
      `Move to ${trashName}`,
      true,
    );
    if (!ok) return;

    let unlistenProgress: UnlistenFn | null = null;
    busy = true;
    busyLabel = "Deleting…";
    showError(null);
    showSuccess(null);
    setProgress(0, "Deleting…");
    render();
    await yieldToPaint();
    try {
      unlistenProgress = await listen<CopyProgressEvent>("env-copy-progress", (ev) => {
        setProgress(ev.payload.percent, ev.payload.label);
      });
      state = await call<AppState>("delete_environment", { id: selectedEnvId });
      selectedEnvId = state.config.active.envId ?? null;
      selectedAccountId = state.config.active.accountId ?? null;
      userPickedAccount = false;
      diskUsageEnvId = null;
      setProgress(null);
      showSuccess(`Environment moved to ${trashName}.`);
    } catch (err) {
      setProgress(null);
      if (typeof err === "string") showError(err);
    } finally {
      if (unlistenProgress) unlistenProgress();
      busy = false;
      busyLabel = null;
      setProgress(null);
      render();
    }
  });

  els.acctNew.addEventListener("click", async () => {
    if (!selectedEnvId || busy) return;
    if (state?.cursorRunning) {
      const ok = await askConfirm(
        "Add account?",
        "Cursor will quit and reopen so you can sign in. The account name will be set from the email after login.",
        "Restart",
        false,
      );
      if (!ok) return;
      try {
        await quitCursorWithUi();
      } catch (err) {
        if (typeof err === "string") showError(err);
        busy = false;
        busyLabel = null;
        render();
        return;
      }
      // quitCursorWithUi leaves busy set; clear it so withBusy below runs
      // (otherwise the account is never created and the status poller stalls).
      busy = false;
      busyLabel = null;
    } else {
      const ok = await askConfirm(
        "Add account?",
        "Cursor will open so you can sign in. The account name will be set from the email after login.",
        "Create & Launch",
        false,
      );
      if (!ok) return;
    }
    await withBusy("Launching…", async () => {
      els.launch.textContent = "Launching…";
      state = await call<AppState>("create_account", {
        envId: selectedEnvId,
      });
      selectedAccountId = state.config.active.accountId ?? null;
      userPickedAccount = true;
      showSuccess("Cursor opened for sign-in. Waiting for email…");
    });
  });

  els.acctRename.addEventListener("click", async () => {
    if (!selectedAccountId || !state || busy) return;
    const current = state.config.accounts.find((a) => a.id === selectedAccountId);
    if (!current) return;
    const automatic = current.pendingLogin
      ? "the email you sign in with"
      : current.name || current.email || "the name from Cursor";
    const label = await askText(
      "Rename account",
      current.label ?? "",
      "Save",
      `Shown in Multi Cursor only. Leave empty to use ${automatic}.`,
    );
    if (label === null) return;
    await withBusy("Saving…", async () => {
      state = await call<AppState>("rename_account", { id: current.id, label });
      const updated = state.config.accounts.find((a) => a.id === current.id);
      showSuccess(
        label ? `Renamed to “${label}”.` : `Name reset to “${updated ? accountDisplayName(updated) : automatic}”.`,
      );
    });
  });

  els.acctDelete.addEventListener("click", async () => {
    if (!selectedAccountId || !state || busy) return;
    const current = state.config.accounts.find((a) => a.id === selectedAccountId);
    if (!current) return;
    const isActive = state.config.active.accountId === current.id;
    const ok = await askConfirm(
      "Delete account?",
      isActive
        ? `Delete “${accountDisplayName(current)}”? This is the current account — Cursor will quit so its login can be cleared.`
        : `Remove “${accountDisplayName(current)}” from Multi Cursor? Only the saved login snapshot is deleted; Cursor is unaffected.`,
      "Delete",
      true,
    );
    if (!ok) return;
    if (isActive && state.cursorRunning) {
      if (
        !(await confirmQuitIfNeeded(
          "Deleting the current account requires Cursor to quit first.",
        ))
      ) {
        return;
      }
      try {
        await quitCursorWithUi();
      } catch (err) {
        if (typeof err === "string") showError(err);
        busy = false;
        busyLabel = null;
        render();
        return;
      }
      busy = false;
      busyLabel = null;
    }
    await withBusy("Deleting…", async () => {
      const deletedId = selectedAccountId;
      state = await call<AppState>("delete_account", { id: deletedId });
      if (deletedId) forgetUsage(deletedId);
      selectedAccountId = state.config.active.accountId ?? null;
      userPickedAccount = false;
      showSuccess("Account deleted.");
    });
  });

  els.launch.addEventListener("click", () => void doLaunch());
  els.switchBtn.addEventListener("click", () => void doLaunch(false));
  els.error.addEventListener("click", hideToasts);
  els.success.addEventListener("click", hideToasts);

  setupContextMenus();
  void setupFooterLinks();
  void refresh({ syncSelection: true }).catch(() => undefined);

  window.setInterval(() => {
    if (busy) return;
    // Poll while waiting for sign-in, or while an account is still labeled
    // "Signing in…" (covers the stuck case where tokens arrived before email).
    const waitingSignIn = accountsForEnv(selectedEnvId).some(
      (a) => a.pendingLogin || a.name.startsWith("Signing in"),
    );
    if (waitingSignIn) {
      void refresh({ silent: true })
        .then(() => {
          const stillWaiting = accountsForEnv(selectedEnvId).some(
            (a) => a.pendingLogin || a.name.startsWith("Signing in"),
          );
          if (!stillWaiting) {
            const email =
              state?.config.accounts.find(
                (a) => a.id === state?.config.active.accountId,
              )?.email ??
              accountsForEnv(selectedEnvId).find((a) => a.email)?.email ??
              null;
            if (email) showSuccess(`Signed in as ${email}.`);
            render();
          }
        })
        .catch(() => undefined);
      return;
    }
    void invoke<boolean>("is_cursor_running")
      .then((running) => {
        if (!state) return;
        if (state.cursorRunning !== running) {
          state.cursorRunning = running;
          render();
        }
      })
      .catch(() => undefined);
  }, 2000);
});
